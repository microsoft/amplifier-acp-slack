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
    command.args(["-I", "-B"]);
    command
        .arg(&script)
        .arg(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process::prepare(&mut command);
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
        anyhow::ensure!(
            doc["ok"] == true,
            "{}",
            doc["error"]
                .as_str()
                .unwrap_or("Slack setup could not complete")
        );
        Ok::<_, anyhow::Error>(doc["result"].clone())
    })
    .await;
    result.map_err(|_| {
        anyhow::anyhow!(
            "Slack setup timed out. Retry; saved authorization and app identity are preserved."
        )
    })?
}

pub async fn get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    Ok(Json(setup_status(&state).await?))
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
    result["connection_ready"] = ready.into();
    result["connection_running"] = c.enabled.into();
    result["setup_complete"] = c.setup_complete.into();
    result["next_action"] = if ready && c.setup_complete {
        // Missing optional app-management access never restarts working setup.
        json!("done")
    } else if result["authorization_pending"] == true {
        json!("confirm")
    } else if ready || result["authorization"] == "managed" {
        json!("connect")
    } else {
        json!("authorize")
    };
    result
}

async fn setup_status(state: &AppState) -> anyhow::Result<Value> {
    Ok(describe_setup(
        state,
        helper(&state.home, "status", json!({})).await?,
    ))
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
}

pub async fn action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Request>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    if !["authorize", "confirm", "connect"].contains(&body.action.as_str())
        || body.challenge.len() > 256
    {
        return Err(ApiError::bad("Choose authorize, confirm, or connect"));
    }
    let _permit = state
        .setup_gate
        .try_acquire()
        .map_err(|_| ApiError::bad("Slack setup is already running"))?;
    let c = state.instance_config();
    if body.action != "connect" {
        let mut result = helper(
            &state.home,
            &body.action,
            json!({"challenge":body.challenge,"team_id":c.slack.team_id}),
        )
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
        if body.action == "confirm" {
            let _ = std::fs::remove_file(state.home.join("secrets/portrait-disabled"));
            state.event("slack_authorized", json!({"actor":actor}))?;
            if connection_ready(&state) {
                // Authorization renewal finishes here: the existing app,
                // message credentials, and paused/running state stay intact.
                update_image(&state);
                result = describe_setup(&state, result);
                result["connection_reused"] = true.into();
                result["next_action"] = "done".into();
                return Ok(Json(result));
            }
            // A new installation needs no separate "Connect" button after
            // approval. Retrying connect remains safe if Slack needs approval.
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
        json!({"manifest":slack::manifest(name),"team_id":c.slack.team_id,"app_id":c.slack.app_id}),
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
        let home = tempfile::tempdir().unwrap();
        Config::new("Existing agent".into(), "owner".into())
            .save(home.path())
            .unwrap();
        let state = Arc::new(AppState::open(home.path().into()).unwrap());
        {
            let mut c = state.config.write().unwrap();
            c.setup_complete = true;
            c.slack.team_id = "TEXISTING".into();
            c.slack.app_id = "AEXISTING".into();
            c.slack.loopback_test = true;
        }
        private_write(
            &state.home.join("secrets/slack.json"),
            br#"{"app_token":"test-app","bot_token":"test-bot"}"#,
        )
        .unwrap();
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

    #[tokio::test]
    async fn retrying_connect_preserves_existing_app_and_paused_state() {
        let (_home, state, headers) = existing();
        let before = std::fs::read(state.home.join("secrets/slack.json")).unwrap();
        let revision = state.instance_config().revision;
        let result = action(
            State(state.clone()),
            headers,
            Json(Request {
                action: "connect".into(),
                challenge: String::new(),
            }),
        )
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
        use sha2::{Digest, Sha256};
        use std::{
            os::unix::fs::PermissionsExt,
            time::{SystemTime, UNIX_EPOCH},
        };
        let (_home, state, headers) = existing();
        let folder = state.home.join("secrets/slack-setup");
        let binary = folder.join("bin/slack");
        let fixture = br##"#!/usr/bin/env python3
import json, pathlib, sys
if 'deploy' in sys.argv:
    raise SystemExit(1)
config = pathlib.Path(sys.argv[sys.argv.index('--config-dir') + 1])
if '--challenge' in sys.argv:
    (config/'credentials.json').write_text(json.dumps({'TEXISTING': {'team_id': 'TEXISTING', 'user_id': 'UOWNER', 'token': 'xoxp-fixture'}}))
"##;
        private_write(&binary, fixture).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        private_write(
            &folder.join("cli.json"),
            serde_json::to_string(
                &json!({"version":"4.12.0","sha256":format!("{:x}",Sha256::digest(fixture))}),
            )
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        private_write(&folder.join("pending.json"),serde_json::to_string(&json!({"ticket":"fixture-ticket","expires_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+600})).unwrap().as_bytes()).unwrap();
        let before = std::fs::read(state.home.join("secrets/slack.json")).unwrap();
        let result = action(
            State(state.clone()),
            headers,
            Json(Request {
                action: "confirm".into(),
                challenge: "fixture-code".into(),
            }),
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
}
