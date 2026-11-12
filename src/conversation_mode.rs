//! Owner controls for one saved ACP conversation; never changes agent defaults.
use crate::{
    auth,
    config::{Config, private_dir},
    driver,
    error::{ApiError, ApiResult},
    state::AppState,
    store::Store,
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use fs2::FileExt;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs::File, path::Path as FsPath, sync::Arc, time::Duration};

fn lock_file(home: &FsPath, context: &str) -> anyhow::Result<File> {
    let folder = home.join("conversation-locks");
    private_dir(&folder)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(folder.join(auth::digest(context)))?)
}

/// Shared with prompt execution, so an arriving message waits for a mode edit.
pub(crate) async fn turn_lock(home: &FsPath, context: &str) -> anyhow::Result<File> {
    let file = lock_file(home, context)?;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if crate::process::lock_busy(&error) => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "This conversation is busy. Wait for its current work to finish, then retry.".into(),
        None,
    )
}

fn prepare(
    state: &AppState,
    context: &str,
    expected: Option<&str>,
) -> ApiResult<(File, Config, String, Value)> {
    if uuid::Uuid::parse_str(context).is_err() {
        return Err(ApiError::missing());
    }
    let guard = lock_file(&state.home, context)?;
    guard.try_lock_exclusive().map_err(|_| busy())?;
    let config = state.instance_config();
    let store = Store::open(&state.home)?;
    if store.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE context=?1 AND status IN ('pending','running'))",
        [context],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(busy());
    }
    let session = store.session(context,&driver::endpoint_key(&config))?.ok_or_else(||ApiError::bad("This conversation has no resumable agent session yet. Talk to the agent here first."))?;
    if expected.is_some_and(|expected| expected != session) {
        return Err(ApiError::conflict(json!({"session_id":session})));
    }
    let body: Option<String> = store.db.query_row("SELECT body FROM messages WHERE context=?1 AND NOT COALESCE(json_extract(body,'$.deleted'),0) AND json_extract(body,'$.bot')=0 ORDER BY ts DESC LIMIT 1",[context],|r|r.get(0)).optional()?;
    let message: crate::model::Message = serde_json::from_str(&body.ok_or_else(|| {
        ApiError::bad("The source conversation is unavailable; no session change was attempted.")
    })?)
    .map_err(anyhow::Error::from)?;
    if !config.allows(&message.channel, "") {
        return Err(ApiError::bad(
            "This conversation is outside the connector's current scope.",
        ));
    }
    let input = driver::request(&config, &message, vec![]);
    Ok((guard, config, session, input))
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(context): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    inspect(state, context, None, None).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub mode_id: String,
    pub expected_session_id: String,
}

pub async fn set(
    State(state): State<Arc<AppState>>,
    Path(context): Path<String>,
    headers: HeaderMap,
    Json(change): Json<Change>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    if change.mode_id.is_empty()
        || change.mode_id.len() > 256
        || change.expected_session_id.is_empty()
        || change.expected_session_id.len() > 1024
    {
        return Err(ApiError::bad(
            "Choose an advertised mode for this conversation.",
        ));
    }
    inspect(state, context, Some(change), Some(actor)).await
}

async fn inspect(
    state: Arc<AppState>,
    context: String,
    change: Option<Change>,
    actor: Option<String>,
) -> ApiResult<Json<Value>> {
    let (_guard, config, session, input) = prepare(
        &state,
        &context,
        change.as_ref().map(|c| c.expected_session_id.as_str()),
    )?;
    let mode = change.as_ref().map(|c| c.mode_id.as_str());
    if let Some(mode) = mode {
        state.event(
            "conversation_mode_requested",
            json!({"context":context,"session_id":session,"mode_id":mode,"actor":actor}),
        )?;
    }
    let work = driver::conversation_mode(&config, &state.home, &session, &input, mode);
    let result = tokio::time::timeout(Duration::from_secs(30), work).await;
    let mut result = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return Err(ApiError::bad(error.to_string())),
        Err(_) => {
            return Err(ApiError::bad(if mode.is_some() {
                "The agent did not confirm the mode change in time. Reload this conversation's permissions to inspect its actual state before retrying."
            } else {
                "The agent did not return this conversation's permissions in time."
            }));
        }
    };
    result["context_id"] = json!(context);
    result["session_id"] = json!(session);
    if mode.is_some() {
        state.event("conversation_mode_changed",json!({"context":context,"session_id":session,"mode_id":result["current_mode_id"],"actor":actor}))?;
    }
    Ok(Json(result))
}
