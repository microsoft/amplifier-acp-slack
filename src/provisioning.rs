//! Per-installation Slack setup. Tooling credentials never cross ACP or the UI.
use crate::{
    auth,
    config::private_write,
    driver,
    error::{ApiError, ApiResult},
    process, server, slack,
    state::AppState,
};
use axum::{Json, extract::State, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Debug)]
struct ConfirmationRefusal(&'static str);
impl std::fmt::Display for ConfirmationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            "invalid_confirmation_format" => "Enter the confirmation code shown by Slack.",
            "authorization_expired" => "That authorization attempt expired. Start authorization again.",
            _ => "Slack refused that confirmation code and expired its ticket. Start authorization again.",
        })
    }
}
impl std::error::Error for ConfirmationRefusal {}
fn helper_result(doc: Value) -> anyhow::Result<Value> {
    if doc["ok"] == true {
        return Ok(doc["result"].clone());
    }
    let refused = &doc["confirmation_refusal"];
    if doc["ok"] == false && refused["schema"] == 1 && refused["action"] == "confirm" {
        let code = match refused["code"].as_str() {
            Some("invalid_confirmation_format") => Some("invalid_confirmation_format"),
            Some("authorization_expired") => Some("authorization_expired"),
            Some("invalid_challenge") => Some("invalid_challenge"),
            _ => None,
        };
        if let Some(code) = code {
            return Err(ConfirmationRefusal(code).into());
        }
    }
    anyhow::bail!(
        "{}",
        doc["error"]
            .as_str()
            .unwrap_or("Slack setup could not complete")
    )
}

// GET reads only fixed owner-protected files. It never materializes a helper,
// starts the Slack CLI, creates a ticket, repairs a path, or changes permissions.
fn setup_file(home: &Path, relative: &str) -> anyhow::Result<Option<Value>> {
    use anyhow::ensure;
    use std::io::Read;
    let path = home.join("secrets/slack-setup").join(relative);
    let mut directory = home.to_owned();
    // Legacy installations can have a readable home while their secrets subtree
    // is private. Observe that root without repairing or tightening its policy.
    let root = std::fs::symlink_metadata(home)?;
    ensure!(
        root.is_dir() && !root.file_type().is_symlink(),
        "Unsafe connector home"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            root.uid() == unsafe { libc::geteuid() } && root.mode() & 0o022 == 0,
            "Connector home is not owner-controlled"
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            root.file_attributes() & 0x400 == 0,
            "Connector home is a reparse point"
        );
    }
    for component in Path::new("secrets/slack-setup")
        .join(relative)
        .parent()
        .unwrap()
        .components()
    {
        directory.push(component);
        match std::fs::symlink_metadata(&directory) {
            Ok(_) => amplifier_native_platform::validate_private_directory(&directory)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Unsafe saved Slack setup file"
    );
    crate::config::check_private(&path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(&path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "Invalid saved Slack setup file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Saved Slack setup file is not private"
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Saved Slack setup file is a reparse point"
        );
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "Saved Slack setup file exceeded its read bound"
    );
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value.is_object(), "Saved Slack setup is malformed");
    Ok(Some(value))
}

fn saved_setup_summary(home: &Path) -> anyhow::Result<Value> {
    use anyhow::ensure;
    let credentials = setup_file(home, "auth/credentials.json")?.unwrap_or(json!({}));
    let mut entries = Vec::new();
    for entry in credentials
        .as_object()
        .unwrap()
        .values()
        .filter(|v| v.is_object())
    {
        if let Some(token) = entry.get("token") {
            ensure!(
                token.is_string() || token.is_null(),
                "Malformed saved Slack authorization"
            );
            if token.as_str().is_some_and(|v| !v.is_empty()) {
                entries.push(entry);
            }
        }
    }
    let mut result =
        json!({"authorized":false,"authorization":"missing","authorization_pending":false});
    if entries.len() == 1 {
        let auth = entries[0];
        let team = auth["team_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Saved Slack authorization has no workspace"))?;
        let workspace = auth
            .get("team_domain")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("Malformed saved Slack workspace"))
            })
            .transpose()?
            .unwrap_or(team);
        result = json!({"authorized":true,"authorization":"managed","authorization_pending":false,"team_id":team,"workspace":workspace});
    }
    let health = setup_file(home, "authorization-health.json")?.unwrap_or(json!({}));
    if result["authorized"] == true && health["state"] == "needs_attention" {
        result["authorization"] = json!("needs_attention");
        result["authorization_issue"] =
            json!("Saved Slack authorization needs attention. Reconnect through the guided flow.");
    }
    let pending = setup_file(home, "pending.json")?.unwrap_or(json!({}));
    let seconds = pending
        .get("expires_at")
        .map(|v| {
            v.as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| anyhow::anyhow!("Malformed pending Slack authorization"))
        })
        .transpose()?
        .unwrap_or(0.0);
    if seconds > crate::model::now_ms() as f64 / 1000.0 {
        let ticket = pending["ticket"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Pending Slack authorization has no ticket"))?;
        ensure!(
            !ticket.is_empty()
                && ticket.len() <= 1024
                && ticket
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+/=.-".contains(&b)),
            "Invalid pending Slack authorization ticket"
        );
        result["authorization_pending"] = json!(true);
        result["authorization_command"] =
            json!({"command":format!("/slackauthticket {ticket}"),"expires_at":seconds});
    }
    Ok(result)
}

pub fn configured(home: &Path) -> bool {
    home.join("secrets/slack-setup/auth/credentials.json")
        .exists()
}

pub async fn helper(home: &Path, action: &str, body: Value) -> anyhow::Result<Value> {
    // Use a pipe for codes and tokens; no command-line credential or CLI log output.
    let script = home.join("secrets/slack-setup-helper.py");
    private_write(&script, include_bytes!("../scripts/slack_setup.py"))?;
    let bundled = crate::desktop::python_path(home);
    let config = crate::config::Config::load(home)?;
    let program = if bundled.is_file() {
        bundled
    } else {
        anyhow::ensure!(
            cfg!(target_os = "linux") && config.server.auth_mode == crate::config::AuthMode::Pam,
            "The connector's private setup runtime is missing. Reopen the complete downloaded app bundle; no system Python fallback is used"
        );
        std::path::PathBuf::from("python3")
    };
    let mut command = Command::new(program);
    command.args(["-I", "-B", "-X", "utf8"]);
    command
        .arg(&script)
        .arg(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process::prepare_hidden(&mut command);
    let mut child = command.spawn()?;
    let _tree = process::ProcessTree::attach(&child)?;
    let input = serde_json::to_vec(&json!({"action":action,"body":body}))?;
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&input).await?;
        drop(stdin);
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .take(65537)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(bytes.len() <= 65536, "Slack setup returned too much data");
        anyhow::ensure!(
            child.wait().await?.success(),
            "Slack setup helper could not run"
        );
        let doc: Value = serde_json::from_slice(&bytes)?;
        helper_result(doc)
    })
    .await;
    result.map_err(|_| {
        anyhow::anyhow!(
            "Slack setup timed out; its outcome is not confirmed. Check saved setup before continuing."
        )
    })?
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<(HeaderMap, Json<Value>)> {
    auth::check(&state, &headers, "admin", false)?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        axum::http::header::CACHE_CONTROL,
        "no-store".parse().unwrap(),
    );
    Ok((response_headers, Json(setup_status(&state).await?)))
}

fn connection_ready(state: &AppState) -> bool {
    let c = state.instance_config();
    !c.slack.app_id.is_empty() && !c.slack.team_id.is_empty() && c.secrets(&state.home).is_ok()
}

fn describe_setup(state: &AppState, mut result: Value) -> Value {
    let c = state.instance_config();
    let portrait = crate::identity::status(&state.home);
    if result["authorization"] != "needs_attention" {
        result["authorization"] = if portrait["state"] == "needs_access" {
            json!("needs_attention")
        } else if result["authorized"] == true {
            json!("managed")
        } else if portrait["credential_configured"] == true {
            json!("legacy")
        } else {
            json!("missing")
        };
    }
    let ready = connection_ready(state);
    // The workspace this connection is declared for (or already connected to) and
    // the one Slack approved are reported separately. Until the app exists they
    // can differ, and neither the page nor an API client should have to guess.
    let approved = result["team_id"]
        .as_str()
        .filter(|_| result["authorized"] == true)
        .unwrap_or("")
        .to_owned();
    let domain = result["workspace"]
        .as_str()
        .filter(|d| *d != approved)
        .unwrap_or("")
        .to_owned();
    let declared = &c.slack.team_id;
    let mismatch = !ready && !approved.is_empty() && !declared.is_empty() && *declared != approved;
    result["intended_workspace"] = workspace(declared, "name", &c.slack.workspace_name);
    result["authorized_workspace"] = workspace(&approved, "domain", &domain);
    result["workspace_mismatch"] = mismatch.into();
    result["connection_ready"] = ready.into();
    result["connection_running"] = c.enabled.into();
    result["setup_complete"] = c.setup_complete.into();
    result["next_action"] = if ready && c.setup_complete {
        // Missing optional app-management access never restarts working setup.
        json!("done")
    } else if result["authorization_pending"] == true {
        json!("confirm")
    } else if ready || (result["authorization"] == "managed" && !mismatch) {
        json!("connect")
    } else {
        // Includes a saved approval for a workspace other than the declared one.
        json!("authorize")
    };
    result
}

fn workspace(team: &str, detail: &str, text: &str) -> Value {
    if team.is_empty() {
        Value::Null
    } else {
        json!({"team_id":team,detail:text})
    }
}

async fn setup_status(state: &AppState) -> anyhow::Result<Value> {
    Ok(describe_setup(state, saved_setup_summary(&state.home)?))
}

fn update_image(state: &Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move {
        let _ = crate::identity::sync(&state.home, &state.instance_config()).await;
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    action: String,
    #[serde(default)]
    challenge: String,
    /// With `connect`: the approved workspace the caller was shown.
    #[serde(default)]
    team_id: String,
}

pub async fn action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Request>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    if !["authorize", "confirm", "connect", "discard"].contains(&body.action.as_str())
        || body.challenge.len() > 256
        || body.team_id.len() > 32
    {
        return Err(ApiError::bad(
            "Choose authorize, confirm, connect, or discard",
        ));
    }
    let _permit = state
        .setup_gate
        .try_acquire()
        .map_err(|_| ApiError::bad("Slack setup is already running"))?;
    let c = state.instance_config();
    if body.action == "discard" {
        // Only an approval that no app was created with can be dropped. A working
        // or partly created connection keeps its Slack access.
        if c.setup_complete || !c.slack.app_id.is_empty() {
            return Err(ApiError::bad(
                "This connection already has its Slack app, so its Slack access was kept.",
            ));
        }
        let result = helper(&state.home, "discard", json!({}))
            .await
            .map_err(|e| ApiError::bad(e.to_string()))?;
        state.event("slack_authorization_discarded", json!({"actor":actor}))?;
        return Ok(Json(describe_setup(&state, result)));
    }
    if body.action != "connect" {
        let response = helper(
            &state.home,
            &body.action,
            json!({"challenge":body.challenge,"team_id":c.slack.team_id,"team_name":c.slack.workspace_name}),
        ).await;
        let mut result = match response {
            Ok(value) => value,
            Err(error) => {
                if body.action == "confirm"
                    && let Some(refusal) = error.downcast_ref::<ConfirmationRefusal>()
                {
                    let mut value = setup_status(&state).await?;
                    let current = state.instance_config();
                    value["confirmation_refusal"] = json!({"schema":1,"action":"confirm","code":refusal.0,"id":current.id,"config_revision":current.revision});
                    value["message"] = json!(refusal.to_string());
                    return Ok(Json(value));
                }
                return Err(ApiError::bad(error.to_string()));
            }
        };
        if body.action == "confirm" {
            let _ = std::fs::remove_file(state.home.join("secrets/portrait-disabled"));
            state.event(
                "slack_authorized",
                json!({"actor":actor,"team_id":result["team_id"]}),
            )?;
            if connection_ready(&state) {
                // Authorization renewal finishes here: the existing app,
                // message credentials, and paused/running state stay intact.
                update_image(&state);
                result = describe_setup(&state, result);
                result["connection_reused"] = true.into();
                result["next_action"] = "done".into();
                return Ok(Json(result));
            }
            if c.slack.team_id.is_empty() {
                // No workspace was declared, so nothing says this approval came
                // from the intended one. Its owner confirms the workspace with
                // `connect`, or discards the approval, before any app is created.
                return Ok(Json(describe_setup(&state, result)));
            }
            // The helper refused any workspace but the declared one, so a new
            // installation needs no separate "Connect" button after approval.
            // Retrying connect remains safe if Slack needs approval.
        } else {
            result["next_action"] = "confirm".into();
            return Ok(Json(result));
        }
    }
    if connection_ready(&state) {
        if !c.setup_complete {
            driver::check(&c, &state.home).await.map_err(|_| {
                ApiError::bad(
                    "The agent connection needs attention. Check Advanced settings, then retry.",
                )
            })?;
            let _ = server::complete(State(state.clone()), headers).await?;
        }
        let mut result = setup_status(&state).await?;
        result["connection_reused"] = true.into();
        result["next_action"] = "done".into();
        return Ok(Json(result));
    }
    // A caller that names the workspace it was shown creates the app only there.
    let team = if c.slack.team_id.is_empty() {
        body.team_id.as_str()
    } else {
        c.slack.team_id.as_str()
    };
    if !body.team_id.is_empty() && body.team_id != team {
        return Err(ApiError::bad(format!(
            "This connection is for {}. Nothing was created in {}.",
            c.slack.workspace_label(),
            body.team_id
        )));
    }
    driver::check(&c, &state.home).await.map_err(|_| {
        ApiError::bad("The agent connection needs attention. Check Advanced settings, then retry.")
    })?;
    let identity = driver::identity(&c, &state.home).await.ok().flatten();
    let name = identity
        .as_ref()
        .and_then(|v| v["display_name"].as_str())
        .unwrap_or(&c.label);
    let result = helper(
        &state.home,
        "connect",
        json!({"manifest":slack::manifest(name),"team_id":team,"team_name":c.slack.workspace_name,"app_id":c.slack.app_id}),
    )
    .await
    .map_err(|e| ApiError::bad(e.to_string()))?;
    let issued = state.home.join("secrets/slack-setup/issued.json");
    let tokens: Value =
        serde_json::from_slice(&std::fs::read(&issued)?).map_err(anyhow::Error::from)?;
    let _ = server::credentials(
        State(state.clone()),
        headers.clone(),
        Json(server::SlackCredentials {
            app_token: tokens["app_token"].as_str().unwrap_or("").into(),
            bot_token: tokens["bot_token"].as_str().unwrap_or("").into(),
            app_id: result["app_id"].as_str().unwrap_or("").into(),
            expected_revision: Some(c.revision),
        }),
    )
    .await?;
    let _ = std::fs::remove_file(issued);
    let _ = std::fs::remove_file(state.home.join("secrets/portrait-disabled"));
    // A Slack login does not automatically link private cross-surface history.
    let _ = server::complete(State(state.clone()), headers).await?;
    state.event("slack_setup_complete", json!({"actor":actor}))?;
    update_image(&state);
    Ok(Json(describe_setup(&state, result)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn existing() -> (tempfile::TempDir, Arc<AppState>, HeaderMap) {
        let (home, state, headers) = installation("Existing agent", |c| {
            c.setup_complete = true;
            c.slack.team_id = "TEXISTING".into();
            c.slack.app_id = "AEXISTING".into();
            c.slack.loopback_test = true;
        });
        private_write(
            &state.home.join("secrets/slack.json"),
            br#"{"app_token":"test-app","bot_token":"test-bot"}"#,
        )
        .unwrap();
        (home, state, headers)
    }

    /// A new installation that has not been connected to Slack.
    fn fresh(team_id: &str, name: &str) -> (tempfile::TempDir, Arc<AppState>, HeaderMap) {
        installation("New agent", |c| {
            c.slack.team_id = team_id.into();
            c.slack.workspace_name = name.into();
        })
    }

    fn installation(
        label: &str,
        configure: impl FnOnce(&mut Config),
    ) -> (tempfile::TempDir, Arc<AppState>, HeaderMap) {
        let home = tempfile::tempdir().unwrap();
        // AppState::open can restrict an existing directory's DACL but must
        // not repair its owner. Seed this fresh fixture's secrets directory
        // with current-user ownership before opening the application state.
        let secrets = home.path().join("secrets");
        amplifier_native_platform::private_create_dir(&secrets).unwrap();
        amplifier_native_platform::validate_private_directory(&secrets).unwrap();
        Config::new(label.into(), "owner".into())
            .save(home.path())
            .unwrap();
        let state = Arc::new(AppState::open(home.path().into()).unwrap());
        configure(&mut state.config.write().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!(
                "Bearer {}",
                std::fs::read_to_string(state.home.join("secrets/control-token"))
                    .unwrap()
                    .trim()
            )
            .parse()
            .unwrap(),
        );
        (home, state, headers)
    }

    /// Points the installation at the repository's independent ACP stdio fixture,
    /// so setup can pass its agent check and reach the Slack CLI boundary.
    #[cfg(target_os = "linux")]
    fn with_agent(state: &AppState) {
        let mut c = state.config.write().unwrap();
        c.acp.command = "python3".into();
        c.acp.args = vec![
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/protocol_acp_fixture.py").into(),
            state.home.join("endpoint").to_string_lossy().into_owned(),
            "Fixture".into(),
        ];
        c.acp.cwd = state.home.to_string_lossy().into_owned();
    }

    fn request(action: &str, challenge: &str, team_id: &str) -> Json<Request> {
        Json(Request {
            action: action.into(),
            challenge: challenge.into(),
            team_id: team_id.into(),
        })
    }

    /// Replaces the Slack CLI with a fixture whose approval belongs to `team`, and
    /// leaves one unexpired ticket pending. A deploy attempt fails and leaves a marker.
    #[cfg(target_os = "linux")]
    fn approving_cli(state: &AppState, team: &str, domain: &str) -> std::path::PathBuf {
        use sha2::{Digest, Sha256};
        use std::{
            os::unix::fs::PermissionsExt,
            time::{SystemTime, UNIX_EPOCH},
        };
        let folder = state.home.join("secrets/slack-setup");
        let binary = folder.join("bin/slack");
        let fixture = r##"#!/usr/bin/env python3
import json, pathlib, sys
config = pathlib.Path(sys.argv[sys.argv.index('--config-dir') + 1])
if 'deploy' in sys.argv:
    (config.parent/'deploy-attempted').write_text('')
    raise SystemExit(1)
if '--challenge' in sys.argv:
    (config/'credentials.json').write_text(json.dumps({'TEAM': {'team_id': 'TEAM', 'team_domain': 'DOMAIN', 'user_id': 'UOWNER', 'token': 'xoxp-fixture'}}))
"##
        .replace("TEAM", team)
        .replace("DOMAIN", domain);
        private_write(&binary, fixture.as_bytes()).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        private_write(
            &folder.join("cli.json"),
            serde_json::to_string(
                &json!({"version":"4.12.0","sha256":format!("{:x}",Sha256::digest(fixture.as_bytes()))}),
            )
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        private_write(&folder.join("pending.json"),serde_json::to_string(&json!({"ticket":"fixture-ticket","expires_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+600})).unwrap().as_bytes()).unwrap();
        folder
    }

    // Uses the legacy PAM/system-Python fixture. The native package fixture covers
    // this same paused-connection retry with its private Python on every platform.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn retrying_connect_preserves_existing_app_and_paused_state() {
        let (_home, state, headers) = existing();
        let before = std::fs::read(state.home.join("secrets/slack.json")).unwrap();
        let revision = state.instance_config().revision;
        let result = action(State(state.clone()), headers, request("connect", "", ""))
            .await
            .unwrap()
            .0;
        assert_eq!(result["next_action"], "done");
        assert_eq!(result["connection_reused"], true);
        assert_eq!(result["connection_ready"], true);
        assert_eq!(result["connection_running"], false);
        assert_eq!(state.instance_config().revision, revision);
        assert_eq!(state.instance_config().slack.app_id, "AEXISTING");
        assert_eq!(
            std::fs::read(state.home.join("secrets/slack.json")).unwrap(),
            before
        );
        assert!(!state.home.join("secrets/slack-setup/project").exists());
    }

    // This fixture exercises the legacy Linux system-Python/PAM installation path.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn confirming_access_finishes_existing_setup_without_installing_an_app() {
        let (_home, state, headers) = existing();
        let folder = approving_cli(&state, "TEXISTING", "existing");
        let before = std::fs::read(state.home.join("secrets/slack.json")).unwrap();
        let result = action(
            State(state.clone()),
            headers,
            request("confirm", "fixture-code", ""),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["next_action"], "done");
        assert_eq!(result["connection_reused"], true);
        assert_eq!(result["authorization"], "managed");
        assert_eq!(result["connection_running"], false);
        assert_eq!(state.instance_config().slack.app_id, "AEXISTING");
        assert!(
            state
                .instance_config()
                .context
                .owner_slack_user_id
                .is_empty()
        );
        assert_eq!(
            std::fs::read(state.home.join("secrets/slack.json")).unwrap(),
            before
        );
        assert!(!folder.join("project").exists());
        assert!(!folder.join("pending.json").exists());
    }

    #[test]
    fn working_legacy_connection_never_requests_setup_again() {
        let (_home, state, _) = existing();
        private_write(
            &state.home.join("secrets/portrait-configuration.json"),
            b"{}",
        )
        .unwrap();
        private_write(
            &state.home.join("portrait-sync.json"),
            br#"{"state":"synced"}"#,
        )
        .unwrap();
        let result = describe_setup(
            &state,
            json!({"authorized":false,"authorization":"missing","authorization_pending":false}),
        );
        assert_eq!(result["authorization"], "legacy");
        assert_eq!(result["next_action"], "done");
        private_write(
            &state.home.join("portrait-sync.json"),
            br#"{"state":"needs_attention"}"#,
        )
        .unwrap();
        assert_eq!(
            describe_setup(&state, json!({"authorized":false}))["authorization"],
            "legacy"
        );
        private_write(
            &state.home.join("portrait-sync.json"),
            br#"{"state":"needs_access"}"#,
        )
        .unwrap();
        assert_eq!(
            describe_setup(&state, json!({"authorized":false}))["authorization"],
            "needs_attention"
        );
    }

    #[test]
    fn declaring_a_workspace_reports_it_without_looking_connected() {
        let (home, state, _) = fresh("T0EXAMPLE1", "Example Workspace");
        let c = state.instance_config();
        c.save(home.path()).unwrap();
        let saved = Config::load(home.path()).unwrap();
        assert_eq!(saved.slack.team_id, "T0EXAMPLE1");
        assert_eq!(saved.slack.workspace_name, "Example Workspace");
        assert!(saved.validate_ready().is_err());
        let result = describe_setup(
            &state,
            json!({"authorized":false,"authorization":"missing","authorization_pending":false}),
        );
        assert_eq!(
            result["intended_workspace"],
            json!({"team_id":"T0EXAMPLE1","name":"Example Workspace"})
        );
        assert_eq!(result["authorized_workspace"], Value::Null);
        assert_eq!(result["connection_ready"], false);
        assert_eq!(result["setup_complete"], false);
        assert_eq!(result["next_action"], "authorize");
    }

    #[test]
    fn earlier_configuration_loads_and_saves_without_the_workspace_name() {
        let (home, state, _) = fresh("", "");
        let stored = std::fs::read_to_string(home.path().join("adapter.json")).unwrap();
        assert!(!stored.contains("workspace_name"));
        let loaded = Config::load(home.path()).unwrap();
        assert!(loaded.slack.workspace_name.is_empty());
        let mut named = state.instance_config();
        named.slack.workspace_name = "Example Workspace".into();
        assert!(named.validate().is_err(), "A name needs its workspace ID");
        named.slack.team_id = "T0EXAMPLE1".into();
        named.save(home.path()).unwrap();
        assert_eq!(
            Config::load(home.path()).unwrap().slack.workspace_label(),
            "Example Workspace (T0EXAMPLE1)"
        );
        let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
        assert!(schema.to_string().contains("workspace_name"));
    }

    #[test]
    fn intended_and_approved_workspaces_are_reported_separately() {
        let approved = json!({"authorized":true,"authorization":"managed","authorization_pending":false,"team_id":"T0OTHER0001","workspace":"other-workspace"});
        let (_home, state, _) = fresh("T0EXAMPLE1", "Example Workspace");
        let result = describe_setup(&state, approved.clone());
        assert_eq!(result["intended_workspace"]["team_id"], "T0EXAMPLE1");
        assert_eq!(
            result["authorized_workspace"],
            json!({"team_id":"T0OTHER0001","domain":"other-workspace"})
        );
        assert_eq!(result["workspace_mismatch"], true);
        assert_eq!(
            result["next_action"], "authorize",
            "An approval for another workspace is never offered for creating the app"
        );
        let (_home, state, _) = fresh("", "");
        let result = describe_setup(&state, approved);
        assert_eq!(result["intended_workspace"], Value::Null);
        assert_eq!(result["authorized_workspace"]["team_id"], "T0OTHER0001");
        assert_eq!(result["workspace_mismatch"], false);
        assert_eq!(result["next_action"], "connect");
    }

    // The remaining workspace tests use the legacy Linux system-Python/PAM fixture.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn undeclared_approval_waits_for_its_owner_before_creating_an_app() {
        let (_home, state, headers) = fresh("", "");
        with_agent(&state);
        let folder = approving_cli(&state, "T0OTHER0001", "other-workspace");
        let revision = state.instance_config().revision;
        let result = action(
            State(state.clone()),
            headers.clone(),
            request("confirm", "fixture-code", ""),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["next_action"], "connect");
        assert_eq!(result["intended_workspace"], Value::Null);
        assert_eq!(
            result["authorized_workspace"],
            json!({"team_id":"T0OTHER0001","domain":"other-workspace"})
        );
        assert_eq!(result["connection_ready"], false);
        assert_eq!(result["setup_complete"], false);
        assert!(!folder.join("deploy-attempted").exists());
        assert!(!folder.join("project").exists());
        assert!(!state.home.join("secrets/slack.json").exists());
        assert_eq!(state.instance_config().revision, revision);
        assert!(state.instance_config().slack.team_id.is_empty());

        // Confirming a workspace other than the approved one creates nothing.
        let ApiError(_, message, _) = action(
            State(state.clone()),
            headers.clone(),
            request("connect", "", "T0EXAMPLE1"),
        )
        .await
        .unwrap_err();
        assert!(
            message.contains("other-workspace (T0OTHER0001)"),
            "{message}"
        );
        assert!(message.contains("T0EXAMPLE1"), "{message}");
        assert!(!folder.join("deploy-attempted").exists());
        assert!(!folder.join("project").exists());

        // Confirming the approved workspace is what reaches app creation. The
        // fixture CLI fails without an app record; that cannot prove no effect.
        assert!(
            action(
                State(state.clone()),
                headers.clone(),
                request("connect", "", "T0OTHER0001"),
            )
            .await
            .is_err()
        );
        assert!(folder.join("deploy-attempted").exists());
        assert!(!state.home.join("secrets/slack.json").exists());
        assert!(state.instance_config().slack.team_id.is_empty());

        // A failed external attempt keeps its reconciliation access even though
        // neither the Rust config nor the CLI app record identifies an app yet.
        assert!(!folder.join("project/.slack/apps.json").exists());
        let saved = std::fs::read(folder.join("auth/credentials.json")).unwrap();
        let ApiError(_, message, _) =
            action(State(state.clone()), headers, request("discard", "", ""))
                .await
                .unwrap_err();
        assert!(message.contains("saved Slack access was kept"), "{message}");
        assert_eq!(
            saved,
            std::fs::read(folder.join("auth/credentials.json")).unwrap()
        );
        assert!(configured(&state.home));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn declared_workspace_refuses_another_and_names_both() {
        let (_home, state, headers) = fresh("T0EXAMPLE1", "Example Workspace");
        with_agent(&state);
        let folder = approving_cli(&state, "T0OTHER0001", "other-workspace");
        let revision = state.instance_config().revision;
        let ApiError(status, message, _) = action(
            State(state.clone()),
            headers,
            request("confirm", "fixture-code", ""),
        )
        .await
        .unwrap_err();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            message.contains("other-workspace (T0OTHER0001)"),
            "{message}"
        );
        assert!(
            message.contains("Example Workspace (T0EXAMPLE1)"),
            "{message}"
        );
        assert!(message.contains("Nothing was created"), "{message}");
        assert!(message.contains("Slackbot"), "{message}");
        assert!(!message.contains("fixture"), "{message}");
        // Nothing is kept from the refused approval, and its used ticket is retired.
        assert!(!configured(&state.home));
        assert!(!folder.join("pending-auth/credentials.json").exists());
        assert!(!folder.join("pending.json").exists());
        assert!(!folder.join("deploy-attempted").exists());
        assert!(!folder.join("project").exists());
        assert!(!state.home.join("secrets/slack.json").exists());
        assert_eq!(state.instance_config().revision, revision);
        assert_eq!(state.instance_config().slack.team_id, "T0EXAMPLE1");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn declared_matching_workspace_creates_the_app_in_one_step() {
        let (_home, state, headers) = fresh("T0EXAMPLE1", "Example Workspace");
        with_agent(&state);
        let folder = approving_cli(&state, "T0EXAMPLE1", "example-workspace");
        // One confirmation goes straight on to app creation, with no question
        // about the workspace. The fixture CLI declines the deploy itself.
        assert!(
            action(
                State(state.clone()),
                headers,
                request("confirm", "fixture-code", ""),
            )
            .await
            .is_err()
        );
        assert!(folder.join("deploy-attempted").exists());
        assert!(folder.join("project/manifest.json").exists());
        let result = setup_status(&state).await.unwrap();
        assert_eq!(result["workspace_mismatch"], false);
        assert_eq!(result["next_action"], "connect");
        assert_eq!(
            result["authorized_workspace"],
            json!({"team_id":"T0EXAMPLE1","domain":"example-workspace"})
        );
    }

    #[tokio::test]
    async fn discard_never_touches_a_working_connection() {
        let (_home, state, headers) = existing();
        let access = state.home.join("secrets/slack-setup/auth/credentials.json");
        private_write(&access, b"{}").unwrap();
        let before = std::fs::read(state.home.join("secrets/slack.json")).unwrap();
        assert!(
            action(State(state.clone()), headers, request("discard", "", ""))
                .await
                .is_err()
        );
        assert!(access.exists());
        assert_eq!(
            std::fs::read(state.home.join("secrets/slack.json")).unwrap(),
            before
        );
        assert_eq!(state.instance_config().slack.app_id, "AEXISTING");
    }
    #[test]
    fn saved_status_is_read_only_and_retains_only_transient_pending_command() {
        let (_home, state, _) = fresh("", "");
        let directory = state.home.join("secrets/slack-setup");
        let empty = saved_setup_summary(&state.home).unwrap();
        assert_eq!(empty["authorization"], "missing");
        assert!(!directory.exists());
        let path = directory.join("pending.json");
        private_write(&path, serde_json::to_vec(&json!({"ticket":"fixed-ticket","expires_at":crate::model::now_ms() as f64/1000.0+600.0})).unwrap().as_slice()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let first = saved_setup_summary(&state.home).unwrap();
        let second = saved_setup_summary(&state.home).unwrap();
        assert_eq!(first, second);
        assert_eq!(first["authorization_pending"], true);
        assert_eq!(
            first["authorization_command"]["command"],
            "/slackauthticket fixed-ticket"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!state.home.join("secrets/slack-setup-helper.py").exists());
        assert!(!directory.join("owner.lock").exists());
        private_write(&path, br#"{"ticket":"expired-ticket","expires_at":0}"#).unwrap();
        let expired = saved_setup_summary(&state.home).unwrap();
        assert_eq!(expired["authorization_pending"], false);
        assert!(expired.get("authorization_command").is_none());
        assert!(path.exists());
    }

    #[test]
    fn saved_status_refuses_malformed_or_unsafe_files_instead_of_missing() {
        let (_home, state, _) = fresh("", "");
        let path = state.home.join("secrets/slack-setup/pending.json");
        for bytes in [
            b"{".as_slice(),
            b"[]",
            br#"{"expires_at":"future"}"#,
            br#"{"expires_at":999999999999,"ticket":"bad ticket"}"#,
        ] {
            private_write(&path, bytes).unwrap();
            assert!(saved_setup_summary(&state.home).is_err());
        }
        private_write(&path, &vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(saved_setup_summary(&state.home).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::remove_file(&path).unwrap();
            let elsewhere = state.home.join("outside.json");
            private_write(&elsewhere, b"{}").unwrap();
            symlink(&elsewhere, &path).unwrap();
            assert!(saved_setup_summary(&state.home).is_err());
            std::fs::remove_file(&path).unwrap();
            private_write(&path, b"{}").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(saved_setup_summary(&state.home).is_err());
        }
    }

    #[tokio::test]
    async fn pending_command_requires_admin_authentication() {
        let (_home, state, headers) = fresh("", "");
        private_write(&state.home.join("secrets/slack-setup/pending.json"),serde_json::to_vec(&json!({"ticket":"private-ticket","expires_at":crate::model::now_ms() as f64/1000.0+600.0})).unwrap().as_slice()).unwrap();
        assert!(get(State(state.clone()), HeaderMap::new()).await.is_err());
        let (response_headers, Json(result)) = get(State(state.clone()), headers).await.unwrap();
        assert_eq!(
            response_headers[axum::http::header::CACHE_CONTROL],
            "no-store"
        );
        assert_eq!(
            result["authorization_command"]["command"],
            "/slackauthticket private-ticket"
        );
        assert!(!state.home.join("secrets/slack-setup-helper.py").exists());
    }

    #[test]
    fn only_exact_helper_refusal_envelope_preserves_typed_confirmation() {
        for code in [
            "invalid_confirmation_format",
            "authorization_expired",
            "invalid_challenge",
        ] {
            let error=helper_result(json!({"ok":false,"error":"must not be reflected","confirmation_refusal":{"schema":1,"action":"confirm","code":code}})).unwrap_err();
            assert_eq!(error.downcast_ref::<ConfirmationRefusal>().unwrap().0, code);
            assert!(!error.to_string().contains("must not be reflected"));
        }
        for doc in [
            json!({"ok":false,"error":"generic"}),
            json!({"ok":false,"confirmation_refusal":{"schema":1,"action":"connect","code":"invalid_challenge"}}),
            json!({"ok":false,"confirmation_refusal":{"schema":2,"action":"confirm","code":"invalid_challenge"}}),
            json!({"ok":false,"confirmation_refusal":{"schema":1,"action":"confirm","code":"other"}}),
        ] {
            assert!(
                helper_result(doc)
                    .unwrap_err()
                    .downcast_ref::<ConfirmationRefusal>()
                    .is_none()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn saved_status_matches_helper_summary_without_launching_it_on_get() {
        for (credentials, pending) in [
            (json!({}), json!({})),
            (json!({}), json!({"ticket":"expired","expires_at":0})),
            (
                json!({}),
                json!({"ticket":"pending","expires_at":crate::model::now_ms() as f64/1000.0+600.0}),
            ),
            (
                json!({"TONE":{"token":"private-never-output","refresh_token":"private-refresh","team_id":"TONE","team_domain":"one"}}),
                json!({}),
            ),
        ] {
            let (_home, state, _) = fresh("", "");
            private_write(
                &state.home.join("secrets/slack-setup/auth/credentials.json"),
                serde_json::to_vec(&credentials).unwrap().as_slice(),
            )
            .unwrap();
            private_write(
                &state.home.join("secrets/slack-setup/pending.json"),
                serde_json::to_vec(&pending).unwrap().as_slice(),
            )
            .unwrap();
            let old = helper(&state.home, "status", json!({})).await.unwrap();
            let mut new = saved_setup_summary(&state.home).unwrap();
            new.as_object_mut().unwrap().remove("authorization_command");
            assert_eq!(old, new);
            assert!(!new.to_string().contains("private-never-output"));
            assert!(!new.to_string().contains("private-refresh"));
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn actual_helper_known_confirmation_refusal_is_typed_but_generic_nonzero_is_not() {
        use sha2::{Digest, Sha256};
        for known in [false, true] {
            let (_home, state, headers) = fresh("", "");
            let folder = approving_cli(&state, "TONE", "one");
            private_write(
                &folder.join("auth/credentials.json"),
                br#"{"TOLD":{"team_id":"TOLD","token":"preserved"}}"#,
            )
            .unwrap();
            let before = std::fs::read(folder.join("auth/credentials.json")).unwrap();
            let output = if known {
                "The following error was returned by the apps.hosted.exchangeAuthTicket Slack API method\nThe challenge code is invalid (invalid_challenge)"
            } else {
                "generic invalid_challenge failure"
            };
            let script = format!(
                "#!/usr/bin/env python3\nimport sys\nprint({:?},file=sys.stderr)\nraise SystemExit(1)\n",
                output
            );
            let binary = folder.join("bin/slack");
            private_write(&binary, script.as_bytes()).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            private_write(&folder.join("cli.json"),serde_json::to_vec(&json!({"version":"4.12.0","sha256":format!("{:x}",Sha256::digest(script.as_bytes()))})).unwrap().as_slice()).unwrap();
            let result = action(
                State(state.clone()),
                headers,
                request("confirm", "fixture-code", ""),
            )
            .await;
            if known {
                let value = result.unwrap().0;
                assert_eq!(value["confirmation_refusal"]["code"], "invalid_challenge");
                assert_eq!(
                    value["confirmation_refusal"]["id"],
                    state.instance_config().id
                );
                assert!(!folder.join("pending.json").exists());
            } else {
                assert!(result.is_err());
                assert!(folder.join("pending.json").exists());
            }
            assert_eq!(
                std::fs::read(folder.join("auth/credentials.json")).unwrap(),
                before
            );
            assert!(!folder.join("project").exists());
            assert!(!state.instance_config().setup_complete);
        }
    }
    #[cfg(unix)]
    #[test]
    fn legacy_readable_home_keeps_its_policy_while_secrets_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_home, state, _) = fresh("", "");
        std::fs::set_permissions(&state.home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            saved_setup_summary(&state.home).unwrap()["authorization"],
            "missing"
        );
        assert_eq!(
            std::fs::metadata(&state.home).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::set_permissions(
            state.home.join("secrets"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(saved_setup_summary(&state.home).is_err());
    }
}
