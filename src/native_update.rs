//! Fixed published-bundle updates for a native installation. External enrollment
//! retains update ownership; caller-provided commands, paths and URLs are refused.
use crate::{
    auth,
    config::{self, AuthMode, Config, WebTransport},
    error::{ApiError, ApiResult},
    state::AppState,
};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::Arc};
const SCRIPT: &[u8] = include_bytes!("../scripts/native_update.py");
fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
fn allowed(home: &Path) -> Result<()> {
    let c = Config::load(home)?;
    let marker = read(&home.join("native-installation.json"))?;
    ensure!(
        c.server.auth_mode == AuthMode::LocalPassword
            && c.server.web_transport == WebTransport::LocalHttp
            && marker["id"] == c.id
            && marker["update_owner"] == "installation",
        "This installation is not managed by the native app"
    );
    ensure!(
        !home.join("saved-connections.json").exists()
            && !home.join("updates/registered-service.json").exists(),
        "Updates belong to this installation's enrolled host controls"
    );
    Ok(())
}
pub fn active(home: &Path) -> Option<Value> {
    let root = home.join("native-updates");
    let id = read(&root.join("active.json")).ok()?["id"]
        .as_str()?
        .to_owned();
    let uuid = uuid::Uuid::parse_str(&id).ok()?;
    if uuid.to_string() != id {
        return None;
    }
    read(&root.join(id).join("plan.json")).ok()
}
pub fn blocks(home: &Path) -> bool {
    active(home).is_some_and(|j| {
        [
            "stopping",
            "swapping",
            "verifying",
            "rolling_back",
            "recovery_required",
        ]
        .contains(&j["state"].as_str().unwrap_or(""))
    })
}
fn worker_guard(home: &Path) -> Result<std::fs::File> {
    let path = home.join("native-updates/worker.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file).context("The update worker is still running")?;
    Ok(file)
}
pub fn status(home: &Path) -> Value {
    let support = allowed(home);
    let mut current = active(home);
    if let Some(job) = current.as_mut() {
        let running = match std::fs::File::open(home.join("native-updates/worker.lock")) {
            Ok(file) => match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => Some(false),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Some(true),
                Err(_) => None,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        };
        let old = crate::model::now_ms()
            - job["updated_at"]
                .as_i64()
                .or_else(|| job["created_at"].as_i64())
                .unwrap_or(0)
            > 10000;
        job["worker_active"] = json!(running);
        job["recovery_available"] = json!(
            running == Some(false)
                && old
                && [
                    "queued",
                    "checking",
                    "draining",
                    "stopping",
                    "swapping",
                    "verifying",
                    "rolling_back",
                    "recovery_required"
                ]
                .contains(&job["state"].as_str().unwrap_or(""))
        );
    }
    json!({"supported":support.is_ok(),"reason":support.err().map(|e|e.to_string()),"repository":"microsoft/amplifier-acp-slack","source":"published releases","current":current})
}
pub async fn get(State(s): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", false)?;
    Ok(Json(status(&s.home)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    action: String,
    request_id: uuid::Uuid,
    plan_id: Option<uuid::Uuid>,
}
pub async fn command(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Command>,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", true)?;
    allowed(&s.home).map_err(|e| ApiError::bad(e.to_string()))?;
    let home = s.home.clone();
    let value = tokio::task::spawn_blocking(move || queue(&home, input))
        .await
        .map_err(|_| ApiError::bad("Update preparation did not finish"))??;
    Ok(Json(value))
}
fn queue(home: &Path, input: Command) -> Result<Value> {
    let root = home.join("native-updates");
    config::private_dir(&root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("queue.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("Another update operation is being prepared")?;
    let request_file = root.join(format!("request-{}.json", input.request_id));
    if request_file.exists() {
        let previous = read(&request_file)?;
        ensure!(
            previous["request_action"]
                .as_str()
                .or_else(|| previous["action"].as_str())
                == Some(input.action.as_str())
                && input
                    .plan_id
                    .is_none_or(|id| previous["id"] == id.to_string()),
            "This request ID already belongs to another update operation"
        );
        return Ok(previous);
    }
    if let Some(job) = active(home) {
        let recovery = ["recover", "rollback"].contains(&input.action.as_str())
            && input.plan_id.is_some_and(|id| job["id"] == id.to_string());
        ensure!(
            recovery
                || ![
                    "queued",
                    "checking",
                    "draining",
                    "stopping",
                    "swapping",
                    "verifying",
                    "rolling_back",
                    "recovery_required"
                ]
                .contains(&job["state"].as_str().unwrap_or("")),
            "An update is already in progress or needs recovery"
        );
    }
    let _recovery_guard = if ["recover", "rollback"].contains(&input.action.as_str()) {
        Some(worker_guard(home)?)
    } else {
        None
    };
    let id = input.plan_id.unwrap_or(input.request_id).to_string();
    let folder = root.join(&id);
    config::private_dir(&folder)?;
    let mut effective_action = input.action.clone();
    let mut job = match input.action.as_str() {
        "recover" => {
            ensure!(input.plan_id.is_some(), "Choose the interrupted plan");
            let mut previous = read(&folder.join("plan.json"))?;
            ensure!(
                [
                    "queued",
                    "checking",
                    "draining",
                    "stopping",
                    "swapping",
                    "verifying",
                    "rolling_back",
                    "recovery_required"
                ]
                .contains(&previous["state"].as_str().unwrap_or("")),
                "This update is not interrupted"
            );
            if ["queued", "checking", "draining"]
                .contains(&previous["state"].as_str().unwrap_or(""))
            {
                previous["state"] = json!("failed");
                previous["reason"] = json!("interrupted_before_cutover");
                previous["request_action"] = json!("recover");
                previous["request_id"] = json!(input.request_id);
                config::private_write(
                    &folder.join("plan.json"),
                    &serde_json::to_vec_pretty(&previous)?,
                )?;
                config::private_write(&request_file, &serde_json::to_vec(&previous)?)?;
                let gate = home.join("updates/drain.json");
                if read(&gate).is_ok_and(|v| v["plan_id"] == id) {
                    std::fs::remove_file(gate)?;
                }
                return Ok(previous);
            }
            effective_action = "rollback".into();
            previous
        }
        "check" => {
            ensure!(input.plan_id.is_none(), "Check takes no plan ID");
            json!({"schema":1,"id":id,"created_at":crate::model::now_ms()})
        }
        "apply" | "rollback" => {
            ensure!(input.plan_id.is_some(), "Choose an existing update plan");
            let job = read(&folder.join("plan.json"))?;
            ensure!(
                if input.action == "apply" {
                    ["ready", "deferred"].contains(&job["state"].as_str().unwrap_or(""))
                        && job["action"] != "rollback"
                } else {
                    ["succeeded", "recovery_required"]
                        .contains(&job["state"].as_str().unwrap_or(""))
                        || (job["state"] == "deferred" && job["action"] == "rollback")
                },
                "This plan cannot perform that operation"
            );
            job
        }
        _ => anyhow::bail!("Choose check, apply, rollback or recover"),
    };
    let critical_recovery = [
        "stopping",
        "swapping",
        "verifying",
        "rolling_back",
        "recovery_required",
    ]
    .contains(&job["state"].as_str().unwrap_or(""));
    if effective_action == "rollback" {
        ensure!(
            folder.join("backup/binary").is_file(),
            "No retained prior version is available"
        );
    }
    job["critical_recovery"] = json!(critical_recovery);
    job["action"] = json!(effective_action);
    job["request_action"] = json!(input.action);
    job["updated_at"] = json!(crate::model::now_ms());
    job["state"] = json!(if critical_recovery {
        "rolling_back"
    } else {
        "queued"
    });
    job["request_id"] = json!(input.request_id);
    // The worker runs from its own private Python copy so Windows can replace the installed runtime.
    if !folder.join("worker-runtime").exists() {
        crate::desktop::copy_runtime(&home.join("runtime"), &folder.join("worker-runtime"))?;
    }
    config::private_write(&folder.join("worker.py"), SCRIPT)?;
    config::private_write(&folder.join("plan.json"), &serde_json::to_vec_pretty(&job)?)?;
    config::private_write(
        &root.join("active.json"),
        &serde_json::to_vec(&json!({"id":id}))?,
    )?;
    config::private_write(&request_file, &serde_json::to_vec(&job)?)?;
    let python = folder.join("worker-runtime/python").join(if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    });
    let mut child = std::process::Command::new(python);
    child
        .args(["-I", "-B", "-X", "utf8"])
        .arg(folder.join("worker.py"))
        .arg(home)
        .arg(&id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        child.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.process_group(0);
    }
    if child.spawn().is_err() {
        job["state"] = json!(if critical_recovery {
            "recovery_required"
        } else {
            "failed"
        });
        job["reason"] = json!("update_worker_unavailable");
        config::private_write(&folder.join("plan.json"), &serde_json::to_vec_pretty(&job)?)?;
    }
    Ok(job)
}
pub async fn readiness(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", false)?;
    let count:i64=s.db.lock().unwrap().query_row("SELECT (SELECT COUNT(*) FROM jobs WHERE status='running')+(SELECT COUNT(*) FROM outbox WHERE status IN ('ready','sending','uncertain'))+(SELECT COUNT(*) FROM public_speech WHERE status IN ('ready','sending','uncertain'))",[],|r|r.get(0))?;
    Ok(Json(
        json!({"idle":count==0 && s.check_gate.available_permits()>0 && s.setup_gate.available_permits()>0,"pending_or_unknown":count,"id":s.instance_config().id}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    plan_id: uuid::Uuid,
}
pub async fn stop(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Stop>,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", true)?;
    allowed(&s.home).map_err(|e| ApiError::bad(e.to_string()))?;
    let job = active(&s.home).ok_or_else(ApiError::missing)?;
    if job["id"] != input.plan_id.to_string()
        || !["stopping", "rolling_back"].contains(&job["state"].as_str().unwrap_or(""))
    {
        return Err(ApiError::forbidden());
    }
    let handle = s
        .shutdown
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| ApiError::bad("Service shutdown is unavailable"))?;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });
    Ok(Json(json!({"stopping":true,"plan_id":input.plan_id})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(phase: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native-updates");
        let id = uuid::Uuid::new_v4().to_string();
        config::private_dir(&root.join(&id)).unwrap();
        config::private_write(
            &root.join("active.json"),
            json!({"id":id}).to_string().as_bytes(),
        )
        .unwrap();
        config::private_write(
            &root.join(&id).join("plan.json"),
            json!({"id":id,"state":phase,"action":"check","created_at":1})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        (dir, id)
    }
    #[test]
    fn interrupted_pre_cutover_request_closes_without_redispatch() {
        for phase in ["queued", "checking", "draining"] {
            let (dir, id) = fixture(phase);
            let result = queue(
                dir.path(),
                Command {
                    action: "recover".into(),
                    request_id: uuid::Uuid::new_v4(),
                    plan_id: Some(uuid::Uuid::parse_str(&id).unwrap()),
                },
            )
            .unwrap();
            assert_eq!(result["state"], "failed");
            assert_eq!(result["reason"], "interrupted_before_cutover");
            assert!(
                !dir.path()
                    .join("native-updates")
                    .join(&id)
                    .join("worker.py")
                    .exists()
            );
        }
    }
    #[test]
    fn active_worker_cannot_be_recovered_and_critical_plan_cannot_be_replaced() {
        let (dir, id) = fixture("checking");
        let guard = worker_guard(dir.path()).unwrap();
        assert!(
            queue(
                dir.path(),
                Command {
                    action: "recover".into(),
                    request_id: uuid::Uuid::new_v4(),
                    plan_id: Some(uuid::Uuid::parse_str(&id).unwrap())
                }
            )
            .is_err()
        );
        drop(guard);
        let (dir, _) = fixture("recovery_required");
        assert!(
            queue(
                dir.path(),
                Command {
                    action: "check".into(),
                    request_id: uuid::Uuid::new_v4(),
                    plan_id: None
                }
            )
            .is_err()
        );
        assert!(blocks(dir.path()));
    }
    #[test]
    fn critical_recovery_worker_failure_never_opens_admission() {
        let (dir, id) = fixture("recovery_required");
        let folder = dir.path().join("native-updates").join(&id);
        std::fs::create_dir_all(folder.join("backup")).unwrap();
        std::fs::write(folder.join("backup/binary"), b"retained").unwrap();
        // A missing worker executable reproduces failed dispatch after durable intent.
        std::fs::create_dir_all(folder.join("worker-runtime")).unwrap();
        let result = queue(
            dir.path(),
            Command {
                action: "recover".into(),
                request_id: uuid::Uuid::new_v4(),
                plan_id: Some(uuid::Uuid::parse_str(&id).unwrap()),
            },
        )
        .unwrap();
        assert_eq!(result["state"], "recovery_required");
        assert_eq!(result["critical_recovery"], true);
        assert!(blocks(dir.path()));
    }
    #[test]
    fn recovery_receipt_is_idempotent_and_does_not_accept_other_action() {
        let (dir, id) = fixture("queued");
        let request_id = uuid::Uuid::new_v4();
        let command = || Command {
            action: "recover".into(),
            request_id,
            plan_id: Some(uuid::Uuid::parse_str(&id).unwrap()),
        };
        let first = queue(dir.path(), command()).unwrap();
        assert_eq!(first, queue(dir.path(), command()).unwrap());
        assert!(
            queue(
                dir.path(),
                Command {
                    action: "check".into(),
                    request_id,
                    plan_id: None
                }
            )
            .is_err()
        );
    }
}
