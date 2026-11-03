//! Optional portable agent portrait -> Slack app icon synchronization.
//! Credentials remain in this adapter; portrait bytes travel over ACP.
use crate::{
    auth,
    config::{Config, check_private, private_write},
    driver,
    error::{ApiError, ApiResult},
    state::{AppState, now_ms},
};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State, http::HeaderMap};
use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const SECRET: &str = "secrets/portrait-configuration.json";
const STATUS: &str = "portrait-sync.json";
const LIMIT: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: i64,
}

fn lock(home: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("portrait-sync.lock"))?;
    file.try_lock_exclusive()
        .context("An image update is already running; try again shortly")?;
    Ok(file)
}
pub fn status(home: &Path) -> Value {
    let saved = std::fs::read(home.join(STATUS)).ok().and_then(|v|serde_json::from_slice::<Value>(&v).ok()).unwrap_or(json!({"state":"not_connected","message":"Connect app configuration access to keep the Slack icon in sync."}));
    let mut value = saved;
    value["credential_configured"] = home.join(SECRET).exists().into();
    value["profile"] = "agent-identity/1".into();
    value["poll_seconds"] = 120.into();
    value
}
fn record(home: &Path, mut value: Value, state: &str, message: &str) -> Result<Value> {
    value["state"] = state.into();
    value["message"] = message.into();
    value["checked_at"] = now_ms().into();
    private_write(&home.join(STATUS), &serde_json::to_vec(&value)?)?;
    Ok(value)
}
pub async fn get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(status(&state.home)))
}
pub async fn credential(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Credential>,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    let _guard = lock(&state.home).map_err(|e| ApiError::bad(e.to_string()))?;
    if body.token.len() < 20
        || body.token.len() > 4096
        || body.refresh_token.len() > 4096
        || body.token.chars().any(char::is_whitespace)
        || body.refresh_token.chars().any(char::is_whitespace)
    {
        return Err(ApiError::bad(
            "Enter an app configuration access token and its optional refresh token",
        ));
    }
    validate_transport_credential(&state.instance_config(), &body)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    private_write(
        &state.home.join(SECRET),
        &serde_json::to_vec(&body).map_err(anyhow::Error::from)?,
    )?;
    record(
        &state.home,
        status(&state.home),
        "waiting",
        "Access saved. The next sync will verify it with Slack.",
    )?;
    state.event("portrait_access_saved", json!({}))?;
    Ok(Json(status(&state.home)))
}
pub async fn disconnect(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    let _guard = lock(&state.home).map_err(|e| ApiError::bad(e.to_string()))?;
    if state.home.join(SECRET).exists() {
        std::fs::remove_file(state.home.join(SECRET))?;
    }
    record(
        &state.home,
        status(&state.home),
        "not_connected",
        "Automatic image updates are off. Slack keeps its current icon.",
    )?;
    Ok(Json(status(&state.home)))
}
pub async fn sync_api(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    Ok(Json(
        sync(&state.home, &state.instance_config())
            .await
            .map_err(|e| ApiError::bad(e.to_string()))?,
    ))
}

/// Reject external URLs and unrelated metadata. Even third-party ACP peers
/// supply bounded, hash-verified static PNG bytes through the same connection.
pub fn portrait(doc: &Value) -> Result<(Vec<u8>, String)> {
    ensure!(
        doc["schema"] == "agent-identity/1",
        "Unsupported identity format"
    );
    ensure!(
        doc["agent_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 256),
        "Missing agent identity"
    );
    let image = &doc["avatar"];
    ensure!(
        image["media_type"] == "image/png",
        "Portrait must be a static PNG"
    );
    ensure!(
        ["uploaded", "generated"].contains(&image["kind"].as_str().unwrap_or("")),
        "Unknown portrait kind"
    );
    let data = image["data"]
        .as_str()
        .context("Portrait bytes are missing")?;
    ensure!(data.len() <= LIMIT.div_ceil(3) * 4, "Portrait too large");
    let bytes = STANDARD.decode(data).context("Invalid portrait encoding")?;
    ensure!(bytes.len() <= LIMIT, "Portrait too large");
    let sha = format!("{:x}", Sha256::digest(&bytes));
    ensure!(image["sha256"] == sha, "Portrait checksum mismatch");
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().context("Unreadable portrait")?;
    ensure!(
        decoded.width() == decoded.height() && (512..=1024).contains(&decoded.width()),
        "Portrait must be square, 512–1024 pixels"
    );
    // Strip metadata from third-party sources too. Only normalized pixels leave.
    let mut out = Cursor::new(Vec::new());
    decoded.write_to(&mut out, image::ImageFormat::Png)?;
    Ok((out.into_inner(), sha))
}

fn validate_transport_credential(c: &Config, secret: &Credential) -> Result<()> {
    if c.slack.loopback_test {
        ensure!(
            secret.token.starts_with("test-")
                && (secret.refresh_token.is_empty() || secret.refresh_token.starts_with("test-")),
            "Real configuration credentials cannot be used with test transport"
        );
    } else {
        ensure!(
            secret.token.starts_with("xoxp-")
                || secret.token.starts_with("xoxe.xoxp-")
                || secret.token.starts_with("xoxb-"),
            "Use a Slack app configuration token, not the Socket Mode token"
        );
        ensure!(
            secret.refresh_token.is_empty() || secret.refresh_token.starts_with("xoxe-"),
            "Invalid configuration refresh token"
        );
    }
    Ok(())
}

async fn response(response: reqwest::Response) -> Result<Value> {
    ensure!(
        response.content_length().is_none_or(|v| v <= 1024 * 1024),
        "Slack response too large"
    );
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 1024 * 1024,
            "Slack response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn error_code(value: &Value) -> String {
    value["error"]
        .as_str()
        .unwrap_or("request_failed")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(60)
        .collect()
}

pub async fn sync(home: &Path, c: &Config) -> Result<Value> {
    let _guard = lock(home)?;
    let mut saved = status(home);
    if !home.join(SECRET).exists() {
        return record(
            home,
            saved,
            "not_connected",
            "Connect app configuration access to keep the Slack icon in sync.",
        );
    }
    if saved["retry_at"].as_i64().unwrap_or(0) > now_ms() {
        return Ok(saved);
    }
    ensure!(
        !c.slack.app_id.is_empty(),
        "Connect this adapter to a Slack app first"
    );
    let result = sync_inner(home, c, &mut saved).await;
    match result {
        Ok((state, message)) => record(home, saved, &state, &message),
        Err(_) => {
            saved["retry_at"] = (now_ms() + 120_000).into();
            record(
                home,
                saved,
                "needs_attention",
                "Image sync could not complete. Check the agent connection and app configuration access, then retry.",
            )
        }
    }
}
async fn sync_inner(home: &Path, c: &Config, saved: &mut Value) -> Result<(String, String)> {
    let Some(doc) = driver::identity(c, home).await? else {
        return Ok((
            "unsupported".into(),
            "This ACP agent does not advertise portable portraits. Its Slack icon is unchanged."
                .into(),
        ));
    };
    let (bytes, hash) = portrait(&doc)?;
    saved["agent_name"] = doc["display_name"]
        .as_str()
        .unwrap_or("Agent")
        .chars()
        .take(200)
        .collect::<String>()
        .into();
    saved["source_revision"] = doc["revision"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(128)
        .collect::<String>()
        .into();
    if saved["synced_hash"] == hash && saved["app_id"] == c.slack.app_id {
        return Ok((
            "synced".into(),
            "Slack is using this agent’s current image.".into(),
        ));
    }
    // An unconfigured agent must not replace a manually chosen Slack app icon.
    // After our first managed upload, removal propagates a neutral fallback.
    if doc["avatar"]["kind"] == "generated" && saved["synced_hash"].is_null() {
        return Ok((
            "waiting_for_image".into(),
            "Upload an image in the agent’s settings. It will appear here automatically.".into(),
        ));
    }
    check_private(&home.join(SECRET))?;
    let mut secret: Credential = serde_json::from_slice(&std::fs::read(home.join(SECRET))?)?;
    validate_transport_credential(c, &secret)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    if !secret.refresh_token.is_empty() && secret.expires_at <= now_ms() / 1000 + 300 {
        let r = client
            .post(format!("{}tooling.tokens.rotate", c.slack.api_base))
            .json(&json!({"refresh_token":secret.refresh_token}))
            .send()
            .await?;
        if r.status().as_u16() == 429 {
            saved["retry_at"] = (now_ms() + retry_seconds(&r) * 1000).into();
            return Ok((
                "rate_limited".into(),
                "Slack asked us to wait before refreshing image access.".into(),
            ));
        }
        ensure!(
            r.status().is_success(),
            "Slack configuration access unavailable"
        );
        let result = response(r).await?;
        if result["ok"] != true {
            return Ok((
                "needs_access".into(),
                format!(
                    "Reconnect app configuration access: {}.",
                    error_code(&result)
                ),
            ));
        }
        ensure!(
            result["team_id"] == c.slack.team_id,
            "Configuration access belongs to another workspace"
        );
        secret.token = result["token"].as_str().context("No access token")?.into();
        secret.refresh_token = result["refresh_token"]
            .as_str()
            .context("No refresh token")?
            .into();
        secret.expires_at = result["exp"].as_i64().context("No expiry")?;
        private_write(&home.join(SECRET), &serde_json::to_vec(&secret)?)?;
    }
    // Bind the destination to the independently verified bot identity. A config
    // token is never passed to the agent or reused for ordinary Slack messages.
    let slack = crate::slack::Slack::new(c, c.secrets(home)?)?;
    slack.verify(c).await?;
    let bot = slack
        .call("bots.info", json!({"bot":c.slack.bot_id}), false)
        .await
        .map_err(|_| anyhow::anyhow!("Could not verify app"))?;
    ensure!(
        bot["bot"]["app_id"] == c.slack.app_id,
        "Slack app identity changed"
    );
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("agent-avatar.png")
        .mime_str("image/png")?;
    let r = client
        .post(format!("{}apps.icon.set", c.slack.api_base))
        .bearer_auth(&secret.token)
        .multipart(
            reqwest::multipart::Form::new()
                .text("app_id", c.slack.app_id.clone())
                .part("file", part),
        )
        .send()
        .await?;
    // Every attempt, including an uncertain response, respects the tier-1 limit.
    saved["retry_at"] = (now_ms() + 120_000).into();
    if r.status().as_u16() == 429 {
        saved["retry_at"] = (now_ms() + retry_seconds(&r) * 1000).into();
        return Ok((
            "rate_limited".into(),
            "Slack asked us to wait; the image will retry automatically.".into(),
        ));
    }
    ensure!(r.status().is_success(), "Slack icon update unavailable");
    let result = response(r).await?;
    if result["ok"] != true {
        return Ok((
            "needs_access".into(),
            format!(
                "Slack declined the image update: {}. Check app configuration access.",
                error_code(&result)
            ),
        ));
    }
    saved["synced_hash"] = hash.into();
    saved["app_id"] = c.slack.app_id.clone().into();
    saved["synced_at"] = now_ms().into();
    Ok((
        "synced".into(),
        "Slack accepted this agent’s image. Slack clients may briefly cache the previous icon."
            .into(),
    ))
}
fn retry_seconds(r: &reqwest::Response) -> i64 {
    r.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(120)
        .clamp(60, 86400)
}

pub async fn run(home: PathBuf, c: Config) {
    loop {
        if let Ok(current) = Config::load(&home) {
            if !current.enabled || current.revision != c.revision {
                break;
            }
        } else {
            break;
        }
        if home.join(SECRET).exists() {
            let _ = sync(&home, &c).await;
        }
        tokio::time::sleep(Duration::from_secs(120)).await;
    }
}
