//! Optional installation-local deployment controls. These are management APIs,
//! not ACP permissions, and never accept a caller-selected source or command.
use crate::{
    auth,
    config::{Config, private_dir, private_write},
    error::{ApiError, ApiResult},
    model::now_ms,
    state::AppState,
};
use anyhow::{Result, ensure};
use axum::{Json, extract::State, http::HeaderMap};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};
const SCRIPT: &str = include_str!("../scripts/update.py");
const REPOSITORY: &str = "microsoft/amplifier-acp-slack";
const ACTIVE: &[&str] = &[
    "queued",
    "planning",
    "draining",
    "stopping",
    "swapping",
    "verifying",
    "rolling_back",
];
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema: u32,
    pub adapter_id: String,
    pub revision: u64,
    pub enabled: bool,
    pub agent_enabled: bool,
    pub owner_slack_user_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyUpdate {
    pub expected_revision: u64,
    pub enabled: bool,
    pub agent_enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub action: String,
    pub plan_id: Option<String>,
}
fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
fn policy(home: &Path) -> Result<Policy> {
    let file = home.join("updates/policy.json");
    if !file.exists() {
        return Ok(Policy::default());
    }
    Ok(serde_json::from_slice(&std::fs::read(file)?)?)
}
fn supported() -> bool {
    cfg!(target_os = "linux")
        && [
            "/usr/bin/python3",
            "/usr/bin/systemd-run",
            "/usr/bin/systemctl",
            "/usr/bin/gh",
        ]
        .iter()
        .all(|p| Path::new(p).is_file())
}
fn lock(home: &Path, name: &str) -> Result<std::fs::File> {
    private_dir(&home.join("updates"))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join("updates").join(name))?;
    file.lock_exclusive()?;
    Ok(file)
}
fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id)
}
fn plan_file(home: &Path, id: &str) -> Result<std::path::PathBuf> {
    ensure!(valid_id(id), "Invalid update plan");
    Ok(home.join("updates/plans").join(id).join("plan.json"))
}
fn active(home: &Path) -> Option<Value> {
    let id = read(&home.join("updates/active.json")).ok()?["id"]
        .as_str()?
        .to_owned();
    read(&plan_file(home, &id).ok()?).ok()
}
/// The default is disabled. A read-only context token is never sufficient.
pub fn agent_allowed(home: &Path, c: &Config, actor: &str, direct_owner: bool) -> bool {
    policy(home).is_ok_and(|p| {
        supported()
            && p.schema == 1
            && p.adapter_id == c.id
            && p.enabled
            && p.agent_enabled
            && direct_owner
            && !actor.is_empty()
            && p.owner_slack_user_id == actor
            && c.context.owner_slack_user_id == actor
    })
}
pub fn status(home: &Path) -> Result<Value> {
    let c = Config::load(home)?;
    let p = policy(home)?;
    Ok(
        json!({"schema":"slack-connector.updates/1","repository":REPOSITORY,"source":"main","workflow":"check.yml","supported":supported(),"platform":"linux-systemd","enabled":p.enabled,"agent_enabled":p.agent_enabled,"policy_revision":p.revision,"owner_linked":!c.context.owner_slack_user_id.is_empty(),"current":active(home),"configuration_and_sessions":"preserved; only the connector executable is replaced","prerequisites":["installer-created Linux user service","GitHub CLI authenticated for Actions artifacts"]}),
    )
}
fn launch(home: &Path, id: &str) -> Result<()> {
    ensure!(supported(), "Linux systemd update tooling is unavailable");
    let root = home.join("updates");
    private_dir(&root)?;
    let hash = format!("{:x}", Sha256::digest(SCRIPT.as_bytes()));
    let script = root.join(format!("controller-{}.py", &hash[..16]));
    if !script.exists() {
        private_write(&script, SCRIPT.as_bytes())?;
    }
    ensure!(
        std::fs::read(&script)? == SCRIPT.as_bytes(),
        "Update helper changed"
    );
    let result = std::process::Command::new("/usr/bin/systemd-run")
        .args([
            "--user",
            "--collect",
            "--quiet",
            "--service-type=exec",
            "--property=UMask=0077",
            "--property=Restart=on-failure",
            "--property=RestartSec=5",
            "--property=RuntimeMaxSec=3600",
        ])
        .arg(format!(
            "--unit=acp-slack-update-{id}-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .arg("/usr/bin/python3")
        .arg("-I")
        .arg(script)
        .arg("--home")
        .arg(home)
        .arg("--id")
        .arg(id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    ensure!(result.success(), "Update worker could not be started");
    Ok(())
}
pub fn recover(home: &Path) {
    let Ok(c) = Config::load(home) else { return };
    let Ok(registered) = read(&home.join("updates/registered-service.json")) else {
        return;
    };
    if registered["adapter_id"] != c.id
        || registered["service"] != format!("acp-slack-adapter-{}.service", c.id)
    {
        return;
    }
    if let Some(job) = active(home)
        && ACTIVE.contains(&job["state"].as_str().unwrap_or(""))
        && let Some(id) = job["id"].as_str()
    {
        let _ = launch(home, id);
    }
}
pub fn queue(home: &Path, command: Command, agent: bool) -> ApiResult<Value> {
    if !supported() {
        return Err(ApiError::bad(
            "Self-update currently requires the Linux systemd installer and GitHub CLI",
        ));
    }
    let _lock = lock(home, "queue.lock")?;
    let c = Config::load(home)?;
    let p = policy(home)?;
    if !p.enabled || p.adapter_id != c.id || (agent && !p.agent_enabled) {
        return Err(ApiError::forbidden());
    }
    if let Some(job) = active(home)
        && (ACTIVE.contains(&job["state"].as_str().unwrap_or(""))
            || (job["state"] == "recovery_required"
                && !(command.action == "rollback"
                    && command.plan_id.as_deref() == job["id"].as_str())))
    {
        return Err(ApiError::conflict(job));
    }
    let mut job = match command.action.as_str() {
        "plan" => {
            if command.plan_id.is_some() {
                return Err(ApiError::bad("Plan takes no source or plan ID"));
            }
            json!({"id":uuid::Uuid::new_v4().to_string(),"created_at":now_ms()})
        }
        "apply" | "rollback" => {
            let id = command
                .plan_id
                .as_deref()
                .ok_or_else(|| ApiError::bad("Choose a prepared plan_id"))?;
            if !valid_id(id) {
                return Err(ApiError::bad("Invalid plan_id"));
            }
            let job = read(&plan_file(home, id)?).map_err(|_| ApiError::missing())?;
            let expected = if command.action == "apply" {
                "ready"
            } else {
                "succeeded"
            };
            if job["state"] != expected
                && !(command.action == "apply" && job["state"] == "deferred")
                && !(command.action == "rollback" && job["state"] == "recovery_required")
            {
                return Err(ApiError::conflict(job));
            }
            job
        }
        _ => return Err(ApiError::bad("Choose plan, apply or rollback")),
    };
    job["policy_revision"] = json!(p.revision);
    job["authorized_owner"] = json!(p.owner_slack_user_id);
    job["action"] = json!(command.action);
    job["state"] = if job["state"] == "recovery_required" {
        json!("rolling_back")
    } else {
        json!("queued")
    };
    job["actor_kind"] = json!(if agent { "agent" } else { "operator" });
    job["updated_at"] = json!(now_ms());
    let id = job["id"].as_str().unwrap();
    let path = plan_file(home, id)?;
    private_dir(path.parent().unwrap())?;
    private_write(&path, &serde_json::to_vec_pretty(&job).unwrap())?;
    private_write(
        &home.join("updates/active.json"),
        json!({"id":id}).to_string().as_bytes(),
    )?;
    if launch(home, id).is_err() {
        job["state"] = if job["state"] == "rolling_back" {
            json!("recovery_required")
        } else {
            json!("failed")
        };
        job["reason"] = json!("update_worker_unavailable");
        private_write(&path, job.to_string().as_bytes())?;
    }
    Ok(job)
}
/// Shared admission lock closes the last-idle-check/worker-start race. Intake
/// still persists new Slack messages; only starting another ACP turn is delayed.
pub fn admission(home: &Path) -> Result<Option<std::fs::File>> {
    // The host may own update controls and expose them read-only to a container.
    // flock needs an open descriptor, not write access. Keep using the SAME
    // inode as the host updater so its drain/critical-section fence still works.
    let file = match std::fs::File::open(home.join("updates/admission.lock")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            private_dir(&home.join("updates"))?;
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(home.join("updates/admission.lock"))?
        }
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    if crate::connection::gated(home) {
        return Ok(None);
    }
    if let Some(job) = active(home)
        && [
            "stopping",
            "swapping",
            "verifying",
            "rolling_back",
            "recovery_required",
        ]
        .contains(&job["state"].as_str().unwrap_or(""))
    {
        return Ok(None);
    }
    if let Ok(gate) = read(&home.join("updates/drain.json"))
        && gate["expires_at"].as_i64().unwrap_or(0) > now_ms()
    {
        return Ok(None);
    }
    Ok(Some(file))
}
pub async fn get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(status(&state.home)?))
}
pub async fn configure(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PolicyUpdate>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let _lock = lock(&state.home, "queue.lock")?;
    let old = policy(&state.home)?;
    if body.expected_revision != old.revision {
        return Err(ApiError::conflict(json!({"revision":old.revision})));
    }
    let c = state.instance_config();
    if body.agent_enabled && (!body.enabled || c.context.owner_slack_user_id.is_empty()) {
        return Err(ApiError::bad(
            "Enable updates and link the Slack owner before granting owner-DM update access",
        ));
    }
    let value = Policy {
        schema: 1,
        adapter_id: c.id.clone(),
        revision: old.revision + 1,
        enabled: body.enabled,
        agent_enabled: body.agent_enabled,
        owner_slack_user_id: c.context.owner_slack_user_id.clone(),
    };
    private_write(
        &state.home.join("updates/policy.json"),
        &serde_json::to_vec_pretty(&value).unwrap(),
    )?;
    state.event(
        "update_policy_changed",
        json!({"actor":actor,"enabled":body.enabled,"agent_enabled":body.agent_enabled}),
    )?;
    Ok(Json(status(&state.home)?))
}
pub async fn action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Command>,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    Ok(Json(queue(&state.home, body, false)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn admission_reads_host_owned_controls_without_changing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let host_lock = lock(dir.path(), "admission.lock").unwrap();
        let root = dir.path().join("updates");
        let path = root.join("admission.lock");
        let original = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Contention still blocks turns even when self-update is disabled.
        assert!(admission(dir.path()).unwrap().is_none());
        drop(host_lock);
        let permit = admission(dir.path()).unwrap().unwrap();
        let other = std::fs::File::open(&path).unwrap();
        assert!(other.try_lock_exclusive().is_err());
        drop(permit);
        assert!(other.try_lock_exclusive().is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o500
        );
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn owner_context_does_not_grant_updates_and_scope_cannot_expand() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::new("Connector".into(), "owner".into());
        c.context.owner_slack_user_id = "UOWNER".into();
        c.save(dir.path()).unwrap();
        assert!(!agent_allowed(dir.path(), &c, "UOWNER", true));
        private_dir(&dir.path().join("updates")).unwrap();
        private_write(&dir.path().join("updates/policy.json"),json!({"schema":1,"adapter_id":c.id,"revision":1,"enabled":true,"agent_enabled":true,"owner_slack_user_id":"UOWNER"}).to_string().as_bytes()).unwrap();
        assert_eq!(agent_allowed(dir.path(), &c, "UOWNER", true), supported());
        assert!(!agent_allowed(dir.path(), &c, "UPEER", true));
        assert!(!agent_allowed(dir.path(), &c, "UOWNER", false));
        c.context.owner_slack_user_id = "UNEWOWNER".into();
        assert!(!agent_allowed(dir.path(), &c, "UNEWOWNER", true));
        assert!(!valid_id("../../bin/acp-slack-adapter"));
        assert!(
            serde_json::from_value::<Command>(
                json!({"action":"plan","url":"https://example.invalid"})
            )
            .is_err()
        );
    }
    #[test]
    fn admission_respects_lease_and_critical_recovery_without_blocking_service() {
        let dir = tempfile::tempdir().unwrap();
        let _gate = lock(dir.path(), "admission.lock").unwrap();
        assert!(admission(dir.path()).unwrap().is_none());
        drop(_gate);
        private_write(
            &dir.path().join("updates/drain.json"),
            json!({"expires_at":now_ms()+10000}).to_string().as_bytes(),
        )
        .unwrap();
        assert!(admission(dir.path()).unwrap().is_none());
        private_write(
            &dir.path().join("updates/drain.json"),
            json!({"expires_at":0}).to_string().as_bytes(),
        )
        .unwrap();
        assert!(admission(dir.path()).unwrap().is_some());
        let id = uuid::Uuid::new_v4().to_string();
        let path = plan_file(dir.path(), &id).unwrap();
        private_dir(path.parent().unwrap()).unwrap();
        private_write(
            &path,
            json!({"id":id,"state":"verifying"}).to_string().as_bytes(),
        )
        .unwrap();
        private_write(
            &dir.path().join("updates/active.json"),
            json!({"id":id}).to_string().as_bytes(),
        )
        .unwrap();
        assert!(admission(dir.path()).unwrap().is_none());
        private_write(
            &path,
            json!({"id":id,"state":"succeeded"}).to_string().as_bytes(),
        )
        .unwrap();
        assert!(admission(dir.path()).unwrap().is_some());
    }
}
