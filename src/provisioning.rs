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
    command.args(["-I", "-B", "-X", "utf8"]);
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
        let mut result = helper(
            &state.home,
            &body.action,
            json!({"challenge":body.challenge,"team_id":c.slack.team_id,"team_name":c.slack.workspace_name}),
        )
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
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
}
