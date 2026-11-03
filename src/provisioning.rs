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
    let mut command = Command::new("python3");
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
    Ok(Json(helper(&state.home, "status", json!({})).await?))
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
        let result = helper(
            &state.home,
            &body.action,
            json!({"challenge":body.challenge,"team_id":c.slack.team_id}),
        )
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
        if body.action == "confirm" {
            let _ = std::fs::remove_file(state.home.join("secrets/portrait-disabled"));
            state.event("slack_authorized", json!({"actor":actor}))?;
        }
        return Ok(Json(result));
    }
    if c.enabled {
        return Err(ApiError::bad(
            "This connection is already running. Authorizing Slack access does not require reinstalling it.",
        ));
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
    Ok(Json(result))
}
