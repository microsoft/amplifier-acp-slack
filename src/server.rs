use crate::{
    auth,
    config::{Config, Secrets, private_dir, private_write, valid_secret_name},
    driver,
    error::{ApiError, ApiResult},
    runtime, slack,
    state::{AppState, now_ms},
    store::Store,
    views,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::FutureExt;
use rusqlite::OptionalExtension;

use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, TcpListener},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/app.css", get(style))
        .route("/healthz", get(|| async { Json(json!({"ok":true})) }))
        .route("/context/v1/query", post(crate::context::query))
        .route("/context/mcp", post(crate::context::mcp))
        .route("/certificate/root.crt", get(certificate))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/session", get(auth::session))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/config", get(config_get).put(config_put))
        .route("/api/v1/status", get(status))
        .route("/api/v1/lifecycle", post(lifecycle))
        .route("/api/v1/credentials/slack", post(credentials))
        .route("/api/v1/check", post(check))
        .route("/api/v1/manifest", get(manifest))
        .route("/api/v1/media/readiness", post(media_check))
        .route("/api/v1/media/enable", post(media_enable))
        .route("/api/v1/setup/complete", post(complete))
        .route("/api/v1/descriptor", get(descriptor))
        .route("/api/v1/schema", get(schema))
        .route("/api/v1/openapi.json", get(openapi))
        .route("/api/v1/activity", get(activity))
        .route("/api/v1/conversations", get(conversations))
        .route("/api/v1/questions", get(questions_get))
        .route("/api/v1/questions/{id}/decision", post(question_decision))
        .route("/api/v1/permissions", get(permissions_get))
        .route(
            "/api/v1/permissions/{id}/decision",
            post(permission_decision),
        )
        .route("/api/v1/secrets", get(secrets))
        .route(
            "/api/v1/secrets/{name}",
            axum::routing::put(secret_put).delete(secret_delete),
        )
        .route("/api/v1/tokens", get(tokens).post(token_create))
        .route("/api/v1/tokens/{id}", delete(token_delete))
        .route("/api/v1/views", get(views::list).post(views::create))
        .route("/api/v1/views/{id}", get(views::get))
        .route("/api/v1/views/{id}/commands", post(views::commands))
        .route("/api/v1/views/{id}/ack", post(views::ack))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state)
}
async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../ui/index.html"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../ui/app.js"),
    )
}
async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../ui/app.css"),
    )
}
async fn certificate(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let bytes = std::fs::read(state.home.join("tls/root-ca.crt"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=slack-adapter-root.crt",
            ),
        ],
        bytes,
    )
        .into_response())
}
async fn security_headers(
    State(state): State<Arc<AppState>>,
    mut request: axum::http::Request<Body>,
    next: Next,
) -> Response {
    if !request.headers().contains_key(header::HOST)
        && let Some(authority) = request.uri().authority().map(|a| a.as_str().to_owned())
        && let Ok(value) = authority.parse()
    {
        request.headers_mut().insert(header::HOST, value);
    }
    let mut response = next.run(request).await;
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self' {}",
        state.instance_config().server.embed_origins.join(" ")
    );
    let h = response.headers_mut();
    h.insert("content-security-policy", csp.parse().unwrap());
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert("cache-control", "no-store".parse().unwrap());
    response
}
async fn config_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    Ok(Json(json!(state.instance_config())))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigUpdate {
    expected_revision: u64,
    config: Config,
}
fn immutable_identity(state: &AppState, old: &Config, new: &Config) -> ApiResult<()> {
    if old.id != new.id || old.server.owner != new.server.owner {
        return Err(ApiError::bad(
            "Adapter ID and installation owner cannot be changed",
        ));
    }
    if state.observed()? {
        let identity = |c: &Config| json!({"team":c.slack.team_id,"app":c.slack.app_id,"user":c.slack.bot_user_id,"bot":c.slack.bot_id,"command":c.acp.command,"args":c.acp.args,"cwd":c.acp.cwd,"env":c.acp.env,"secret_env":c.acp.secret_env});
        if identity(old) != identity(new) {
            return Err(ApiError::bad(
                "This connection already has conversation history. Install a fresh adapter to connect a different agent or Slack app",
            ));
        }
    }
    Ok(())
}
pub fn replace_config(
    state: &AppState,
    expected: u64,
    mut config: Config,
    actor: &str,
) -> ApiResult<Value> {
    config
        .validate()
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if config.enabled {
        config
            .validate_ready()
            .map_err(|e| ApiError::bad(e.to_string()))?;
        config
            .secrets(&state.home)
            .map_err(|_| ApiError::bad("Connect and verify Slack credentials before starting"))?;
    }
    {
        let mut old = state.config.write().unwrap();
        if old.revision != expected {
            return Err(ApiError::conflict(json!({"revision":old.revision})));
        }
        immutable_identity(state, &old, &config)?;
        if config.enabled && !old.enabled && state.check_gate.available_permits() == 0 {
            return Err(ApiError::bad(
                "Wait for the connection check to finish before starting",
            ));
        }
        config.revision = expected
            .checked_add(1)
            .ok_or_else(|| ApiError::bad("Configuration revision limit reached"))?;
        config.save(&state.home)?;
        *old = config.clone();
    }
    state.changes.send_replace(config.revision);
    state.event(
        "configuration_changed",
        json!({"revision":config.revision,"actor":actor,"server_settings":"after service restart"}),
    )?;
    Ok(json!(config))
}
async fn config_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ConfigUpdate>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    Ok(Json(replace_config(
        &state,
        body.expected_revision,
        body.config,
        &actor,
    )?))
}
fn runtime_value(state: &AppState, c: &Config) -> Value {
    if let Some(failure) = state.runtime_failure.read().unwrap().clone() {
        return failure;
    }
    std::fs::read(state.home.join("runtime.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(
            || json!({"state":if c.enabled{"connecting"}else{"stopped"},"connections":[]}),
        )
}
pub fn status_value(state: &AppState) -> ApiResult<Value> {
    let c = state.instance_config();
    let runtime = runtime_value(state, &c);
    let observations = Store::open(&state.home)?.status()?;
    let addresses = state.addresses.read().unwrap().clone();
    Ok(
        json!({"id":c.id,"label":c.label,"version":env!("CARGO_PKG_VERSION"),"enabled":c.enabled,"setup_complete":c.setup_complete,"config_revision":c.revision,"runtime":runtime,"credentials":{"slack":c.secrets(&state.home).is_ok()},"state":observations,"connections":runtime["connections"].as_array().cloned().unwrap_or_default(),"addresses":addresses,"restart_required":addresses.get("effective_server").is_some_and(|s|*s!=json!(c.server)),"media":state.get_record("checks","media")?,"client_capabilities":crate::client_tools::advertised(&c),"pending_permissions":Store::open(&state.home)?.db.query_row("SELECT COUNT(*) FROM permissions WHERE status='pending'",[],|r|r.get::<_,i64>(0))?,"uptime_ms":now_ms()-state.started_at}),
    )
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(status_value(&state)?))
}
async fn questions_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    Ok(Json(
        json!({"items":crate::questions::list(&state.home,&state.instance_config())?}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionDecision {
    action: String,
    content: Option<Value>,
}
async fn question_decision(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<QuestionDecision>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    crate::questions::decide(
        &state.home,
        &state.instance_config(),
        &id,
        &body.action,
        body.content.as_ref(),
        &actor,
    )
    .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(json!({"ok":true})))
}
async fn permissions_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    // Tool arguments can contain private source material. Observers receive
    // counts through status, while complete requests require administrator access.
    auth::check(&state, &headers, "admin", false)?;
    let s = Store::open(&state.home)?;
    crate::permissions::expire(&s, &state.instance_config())?;
    Ok(Json(json!({"items":crate::permissions::list(&s)?})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionDecision {
    option_id: Option<String>,
}
async fn permission_decision(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PermissionDecision>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    crate::permissions::resolve(
        &Store::open(&state.home)?,
        &state.instance_config(),
        &id,
        body.option_id.as_deref(),
        &actor,
        None,
    )
    .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(json!({"ok":true})))
}
async fn lifecycle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let mut c = state.instance_config();
    c.enabled = match body["action"].as_str() {
        Some("start") => true,
        Some("stop") => false,
        _ => return Err(ApiError::bad("Action must be start or stop")),
    };
    replace_config(&state, c.revision, c.clone(), &actor)?;
    state.event(
        if c.enabled {
            "adapter_started"
        } else {
            "adapter_stopped"
        },
        json!({"actor":actor}),
    )?;
    Ok(Json(status_value(&state)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackCredentials {
    app_token: String,
    bot_token: String,
    #[serde(default)]
    app_id: String,
    expected_revision: Option<u64>,
}
async fn credentials(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<SlackCredentials>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let _permit = state.check_gate.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "A connection check is already running".into(),
            None,
        )
    })?;
    let mut c = state.instance_config();
    let expected = body.expected_revision.unwrap_or(c.revision);
    if expected != c.revision {
        return Err(ApiError::conflict(json!({"revision":c.revision})));
    }
    if c.enabled {
        return Err(ApiError::bad(
            "Pause this connection before replacing Slack credentials",
        ));
    }
    if body.app_id.len() > 100 || body.app_token.len() > 4096 || body.bot_token.len() > 4096 {
        return Err(ApiError::bad("Provide valid Slack tokens"));
    }
    let s = Secrets {
        app_token: body.app_token,
        bot_token: body.bot_token,
    };
    if c.slack.loopback_test {
        if !s.app_token.starts_with("test-") || !s.bot_token.starts_with("test-") {
            return Err(ApiError::bad("Test transport requires test credentials"));
        }
    } else if !s.app_token.starts_with("xapp-") || !s.bot_token.starts_with("xoxb-") {
        return Err(ApiError::bad(
            "Use the xapp app token and xoxb bot token from Slack",
        ));
    }
    if !body.app_id.is_empty() {
        c.slack.app_id = body.app_id;
    }
    let identity=slack::verify_credentials(&c,s.clone()).await.map_err(|_|ApiError::bad("Slack could not verify these credentials. Check the tokens, app ID, installation, and Socket Mode setting"))?;
    c.slack.team_id = identity["team_id"].as_str().unwrap_or("").into();
    c.slack.bot_user_id = identity["bot_user_id"]
        .as_str()
        .or_else(|| identity["user_id"].as_str())
        .unwrap_or("")
        .into();
    c.slack.bot_id = identity["bot_id"].as_str().unwrap_or("").into();
    if c.slack.team_id.is_empty() || c.slack.bot_user_id.is_empty() || c.slack.bot_id.is_empty() {
        return Err(ApiError::bad("Slack returned an incomplete app identity"));
    }
    if let Some(app) = identity["app_id"].as_str() {
        if !c.slack.app_id.is_empty() && app != c.slack.app_id {
            return Err(ApiError::bad("These tokens belong to another Slack app"));
        }
        c.slack.app_id = app.into();
    }
    if c.slack.app_id.is_empty() {
        return Err(ApiError::bad("Slack returned no app identity"));
    }
    let media = identity["media"].clone();
    let identity = json!({"team_id":c.slack.team_id,"app_id":c.slack.app_id,"bot_user_id":c.slack.bot_user_id,"bot_id":c.slack.bot_id});
    // Hold the config write lock across the credential write and config commit,
    // so a concurrent setup save cannot attach checked tokens to another identity.
    {
        let mut old = state.config.write().unwrap();
        if old.revision != expected {
            return Err(ApiError::conflict(json!({"revision":old.revision})));
        }
        immutable_identity(&state, &old, &c)?;
        c.validate().map_err(|e| ApiError::bad(e.to_string()))?;
        let path = state.home.join("secrets/slack.json");
        let prior = std::fs::read(&path).ok();
        private_write(&path, &serde_json::to_vec(&s).map_err(anyhow::Error::from)?)?;
        c.revision = expected + 1;
        if let Err(e) = c.save(&state.home) {
            if let Some(bytes) = prior {
                let _ = private_write(&path, &bytes);
            } else {
                let _ = std::fs::remove_file(&path);
            }
            return Err(e.into());
        }
        *old = c.clone();
    }
    state.changes.send_replace(c.revision);
    state.set_record("checks", "media", media)?;
    state.event(
        "slack_credentials_connected",
        json!({"identity":identity,"actor":actor}),
    )?;
    Ok(Json(
        json!({"connected":true,"identity":identity,"config_revision":c.revision}),
    ))
}
fn active_socket_health(state: &AppState, checked: &Config) -> ApiResult<Value> {
    let current = state.instance_config();
    let runtime = runtime_value(state, &current);
    // Read the same connection audit used by Store::status, together with its
    // timestamp in one snapshot so a reconnect cannot mix two observations.
    let observed: Option<(i64, String)> = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT time,data FROM audit WHERE kind='connection' ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let connected_at = observed.as_ref().map(|(at, _)| *at);
    let connection = observed
        .and_then(|(_, raw)| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    // A successful auth.test only validates credentials. It says nothing about
    // the socket that actually delivers messages to this adapter process.
    let same_revision = current.revision == checked.revision
        && runtime["config_revision"].as_u64() == Some(current.revision);
    let current_process = runtime["updated_at"]
        .as_i64()
        .is_some_and(|at| at >= state.started_at);
    let current_connection = connected_at
        .zip(runtime["updated_at"].as_i64())
        .is_some_and(|(connection, runtime)| connection >= runtime);
    let healthy = current.enabled
        && same_revision
        && current_process
        && current_connection
        && runtime["state"] == "running"
        && connection["state"] == "connected";
    Ok(
        json!({"healthy":healthy,"state":runtime["state"],"config_revision":runtime["config_revision"],"expected_revision":current.revision,"socket_state":connection["state"],"identity_check_revision":checked.revision}),
    )
}

async fn monitor_runtime<F>(state: Arc<AppState>, future: F)
where
    F: std::future::Future<Output = Result<()>>,
{
    let outcome = std::panic::AssertUnwindSafe(future).catch_unwind().await;
    let category = match outcome {
        Ok(Ok(())) => "unexpected_return",
        Ok(Err(_)) => "error",
        Err(_) => "panic",
    };
    let failure = json!({"state":"failed","config_revision":state.instance_config().revision,"updated_at":now_ms(),"reason":"The connection supervisor stopped unexpectedly. Restart the adapter service to recover.","connections":[]});
    // Keep an in-memory failure as well: a disk failure must not leave the UI
    // reporting healthy state from the last successful runtime status write.
    *state.runtime_failure.write().unwrap() = Some(failure.clone());
    let recorded = private_write(
        &state.home.join("runtime.json"),
        failure.to_string().as_bytes(),
    );
    let audited = state.event("runtime_supervisor_failed", json!({"category":category}));
    if recorded.is_err() || audited.is_err() {
        eprintln!("Connection supervisor stopped; its failure could not be fully recorded.");
    }
}

async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    let _permit = state.check_gate.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "A connection check is already running".into(),
            None,
        )
    })?;
    let target = body["target"].as_str().unwrap_or("all");
    if !["acp", "slack", "all"].contains(&target) {
        return Err(ApiError::bad("Check target must be acp, slack, or all"));
    }
    let c = state.instance_config();
    let mut result = json!({"ok":true});
    if target == "acp" || target == "all" {
        result["acp"] = match tokio::time::timeout(
            Duration::from_secs(60),
            driver::check(&c, &state.home),
        )
        .await
        {
            Ok(Ok(v)) => json!({"ok":true,"result":v}),
            Ok(Err(error)) if crate::acp_auth::authentication_required(&error) => {
                result["ok"] = json!(false);
                json!({"ok":false,"code":"agent_auth_required","error":error.to_string()})
            }
            _ => {
                result["ok"] = json!(false);
                json!({"ok":false,"error":"The ACP agent could not complete its connection check. Check the command, working folder, account, and agent service."})
            }
        };
    }
    if target == "slack" || target == "all" {
        result["slack"] = match c.secrets(&state.home) {
            Ok(s) if c.enabled => match slack::Slack::new(&c, s) {
                Ok(client) => match client.verify(&c).await {
                    Ok(()) => {
                        let active = active_socket_health(&state, &c)?;
                        let media = client.media_readiness(&c);
                        state.set_record("checks", "media", media.clone())?;
                        if active["healthy"] == true {
                            json!({"ok":true,"result":{"identity_verified":true,"socket":"connected","managed_runtime":active,"media":media}})
                        } else {
                            json!({"ok":false,"error":"Slack credentials are valid, but the managed Socket Mode connection is not ready. Check connection health and wait for reconnection.","result":{"identity_verified":true,"managed_runtime":active,"media":media}})
                        }
                    }
                    Err(_) => {
                        json!({"ok":false,"error":"Slack could not verify the saved bot identity."})
                    }
                },
                Err(_) => {
                    json!({"ok":false,"error":"Slack could not initialize its connection check."})
                }
            },
            Ok(s) => match slack::verify_credentials(&c, s).await {
                Ok(v) => {
                    state.set_record("checks", "media", v["media"].clone())?;
                    json!({"ok":true,"result":v})
                }
                Err(_) => {
                    json!({"ok":false,"error":"Slack could not verify the saved credentials and Socket Mode connection."})
                }
            },
            Err(_) => json!({"ok":false,"error":"Connect your Slack app first."}),
        };
        if result["slack"]["ok"] != true {
            result["ok"] = json!(false);
        }
    }
    state.event(
        "connection_checked",
        json!({"target":target,"ok":result["ok"]}),
    )?;
    Ok(Json(result))
}
async fn inspect_media(state: &AppState, c: &Config) -> ApiResult<Value> {
    let client = slack::Slack::new(
        c,
        c.secrets(&state.home)
            .map_err(|_| ApiError::bad("Connect your Slack app first"))?,
    )?;
    client.verify(c).await.map_err(|_| {
        ApiError::bad("Slack could not verify this app. Check its saved credentials.")
    })?;
    let media = client.media_readiness(c);
    state.set_record("checks", "media", media.clone())?;
    Ok(media)
}
async fn media_check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    Ok(Json(inspect_media(&state, &state.instance_config()).await?))
}
async fn media_enable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let mut c = state.instance_config();
    let media = inspect_media(&state, &c).await?;
    if media["ready"] != true {
        return Err(ApiError::bad(
            "Slack file access is not ready. Add the missing scopes, reinstall this app, then check again.",
        ));
    }
    c.media.incoming = true;
    c.media.outgoing = true;
    let config = replace_config(&state, c.revision, c, &actor)?;
    Ok(Json(json!({"config":config,"media":media})))
}
async fn manifest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    Ok(Json(slack::manifest(&state.instance_config().label)))
}
async fn complete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let mut c = state.instance_config();
    c.setup_complete = true;
    c.enabled = true;
    Ok(Json(replace_config(&state, c.revision, c, &actor)?))
}
async fn activity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(Store::open(&state.home)?.activity()?))
}
async fn conversations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(Store::open(&state.home)?.conversations()?))
}
fn secret_path(state: &AppState, name: &str) -> ApiResult<PathBuf> {
    if !valid_secret_name(name) || ["control-token", "slack"].contains(&name) {
        return Err(ApiError::bad(
            "Choose an unreserved secret name using letters, numbers, underscores, or hyphens",
        ));
    }
    Ok(state.home.join("secrets").join(name))
}
async fn secrets(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    let mut names = std::fs::read_dir(state.home.join("secrets"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (secret_path(&state, &name).is_ok()
                && entry.file_type().ok().is_some_and(|f| f.is_file()))
            .then_some(name)
        })
        .collect::<Vec<_>>();
    names.sort();
    Ok(Json(json!({"items":names})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretWrite {
    value: String,
}
async fn secret_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<SecretWrite>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    if body.value.is_empty() || body.value.len() > 65536 || body.value.contains('\0') {
        return Err(ApiError::bad(
            "Secret values must contain 1–65536 bytes without null characters",
        ));
    }
    private_write(&secret_path(&state, &name)?, body.value.as_bytes())?;
    state.changes.send_replace(state.instance_config().revision);
    state.event("secret_updated", json!({"actor":actor,"name":name}))?;
    Ok(Json(json!({"name":name,"stored":true})))
}
async fn secret_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let path = secret_path(&state, &name)?;
    if state
        .instance_config()
        .acp
        .secret_env
        .values()
        .any(|v| v == &name)
    {
        return Err(ApiError::bad(
            "This secret is still referenced by the agent connection",
        ));
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError::missing()),
        Err(e) => return Err(e.into()),
    }
    state.event("secret_removed", json!({"actor":actor,"name":name}))?;
    Ok(Json(json!({"ok":true})))
}
async fn tokens(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    let db = state.db.lock().unwrap();
    let mut q=db.prepare("SELECT id,name,scopes,expires_at,created_at FROM tokens WHERE id!='local-control' ORDER BY created_at DESC")?;
    let rows=q.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"scopes":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(json!([])),"expires_at":r.get::<_,i64>(3)?,"created_at":r.get::<_,i64>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(json!({"items":rows})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenCreate {
    name: String,
    scopes: Vec<String>,
    #[serde(default = "token_lifetime")]
    expires_in_seconds: u64,
}
fn token_lifetime() -> u64 {
    30 * 86400
}
async fn token_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<TokenCreate>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    let token = auth::issue_token(&state, &body.name, &body.scopes, body.expires_in_seconds)?;
    state.event(
        "access_token_created",
        json!({"actor":actor,"id":token["id"],"scopes":token["scopes"]}),
    )?;
    Ok(Json(token))
}
async fn token_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "admin", true)?.actor;
    if id == "local-control" {
        return Err(ApiError::bad(
            "The local owner credential cannot be revoked through this API",
        ));
    }
    let changed = state
        .db
        .lock()
        .unwrap()
        .execute("DELETE FROM tokens WHERE id=?1", [&id])?;
    if changed == 0 {
        return Err(ApiError::missing());
    }
    state.event("access_token_revoked", json!({"actor":actor,"id":id}))?;
    Ok(Json(json!({"ok":true})))
}
fn operations() -> Value {
    json!([
        {"path":"config","methods":["GET","PUT"],"scope":"admin"},{"path":"status","methods":["GET"],"scope":"observe"},{"path":"lifecycle","methods":["POST"],"scope":"admin"},{"path":"credentials/slack","methods":["POST"],"scope":"admin"},{"path":"check","methods":["POST"],"scope":"admin"},{"path":"manifest","methods":["GET"],"scope":"admin"},{"path":"media/readiness","methods":["POST"],"scope":"admin"},{"path":"media/enable","methods":["POST"],"scope":"admin"},{"path":"setup/complete","methods":["POST"],"scope":"admin"},{"path":"questions","methods":["GET"],"scope":"admin"},{"path":"questions/{id}/decision","methods":["POST"],"scope":"admin"},{"path":"permissions","methods":["GET"],"scope":"admin"},{"path":"permissions/{id}/decision","methods":["POST"],"scope":"admin"},{"path":"activity","methods":["GET"],"scope":"observe"},{"path":"conversations","methods":["GET"],"scope":"observe"},{"path":"secrets","methods":["GET"],"scope":"admin"},{"path":"secrets/{name}","methods":["PUT","DELETE"],"scope":"admin"},{"path":"tokens","methods":["GET","POST"],"scope":"admin"},{"path":"tokens/{id}","methods":["DELETE"],"scope":"admin"},{"path":"views","methods":["GET","POST"],"scope":"ui"},{"path":"views/{id}","methods":["GET"],"scope":"ui"},{"path":"views/{id}/commands","methods":["POST"],"scope":"ui"},{"path":"views/{id}/ack","methods":["POST"],"scope":"ui"}
    ])
}
async fn descriptor(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    let c = state.instance_config();
    Ok(Json(
        json!({"api_version":"1","id":c.id,"kind":"conversation-adapter","implementation":{"name":"acp-slack-adapter","version":env!("CARGO_PKG_VERSION")},"namespace":"io.github.bkrabach.acp-slack-adapter","interfaces":{"agent":{"protocol":"ACP","role":"client","transport":"stdio"},"conversation":{"protocol":"Slack Socket Mode","role":"app"},"management":{"protocol":"acp-slack-adapter.management/1","base":"api/v1/","standard_acp":false}},"participation":{"profile":"slack-conversation-decision/1","standard_acp":false,"description":"Optional prompt convention requesting speak/pass decisions through standard ACP prompts"},"context_provider":{"id":format!("slack:{}",c.id),"query":"/context/v1/query","mcp":"/context/mcp","enabled":c.context.enabled,"standard_acp":false,"mcp_transport":"streamable-http","owner_linked":!c.context.owner_slack_user_id.is_empty()},"media":state.get_record("checks","media")?,"client_capabilities":crate::client_tools::advertised(&c),"capabilities":["config","credentials","secrets","lifecycle","observations","ui-state","conversation-context","interactive-permissions","local-client-tools"],"schema":"api/v1/schema","openapi":"api/v1/openapi.json","ui":{"entry":"./","embed_protocol":"acp-slack-adapter.ui/1","embed_origins":c.server.embed_origins,"authentication":"Owner sign-in or authenticated same-origin proxy; iframe embedding does not grant access"},"operations":operations()}),
    ))
}
async fn schema(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(json!(schemars::schema_for!(Config))))
}
pub fn openapi_value() -> Value {
    let mut schemas = json!({
        "Config":schemars::schema_for!(Config),
        "PermissionDecision":{"type":"object","additionalProperties":false,"properties":{"option_id":{"type":["string","null"]}},"description":"Select an advertised single-use option, or null to cancel. Uses authenticated administrator authority, never a supplied Slack identity."},
        "ConfigUpdate":{"type":"object","required":["expected_revision","config"],"properties":{"expected_revision":{"type":"integer","minimum":0},"config":{"$ref":"#/components/schemas/Config"}}},
        "SecretWrite":{"type":"object","required":["value"],"properties":{"value":{"type":"string","writeOnly":true}}},
        "Lifecycle":{"type":"object","required":["action"],"properties":{"action":{"enum":["start","stop"]}}},
        "Credentials":{"type":"object","required":["app_token","bot_token"],"properties":{"app_token":{"type":"string","writeOnly":true},"bot_token":{"type":"string","writeOnly":true},"app_id":{"type":"string"},"expected_revision":{"type":"integer"}}},
        "Check":{"type":"object","properties":{"target":{"enum":["all","acp","slack"]}}},
        "Token":{"type":"object","required":["name","scopes"],"properties":{"name":{"type":"string"},"scopes":{"type":"array","items":{"enum":["admin","observe","ui","context"]}},"expires_in_seconds":{"type":"integer","minimum":60,"maximum":31622400}}},
        "ViewCreate":{"type":"object","properties":{"label":{"type":"string"}}},
        "ViewCommands":{"type":"object","required":["expected_revision","commands"],"properties":{"expected_revision":{"type":"integer"},"commands":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"object","required":["type"],"properties":{"type":{"enum":["navigate","set_draft","set_filter","select_conversation","focus","scroll","open_modal","close_modal"]}}}}}},
        "ViewAck":{"type":"object","required":["revision"],"properties":{"revision":{"type":"integer"},"status":{"enum":["applied","error"]}}}
    });
    if let Some(defs) = schemas["Config"].as_object_mut().unwrap().remove("$defs") {
        for (name, schema) in defs.as_object().into_iter().flatten() {
            schemas
                .as_object_mut()
                .unwrap()
                .insert(name.clone(), schema.clone());
        }
    }
    let schemas: Value = serde_json::from_str(
        &schemas
            .to_string()
            .replace("#/$defs/", "#/components/schemas/"),
    )
    .unwrap();
    let mut paths = serde_json::Map::new();
    for op in operations().as_array().unwrap() {
        let path = op["path"].as_str().unwrap();
        let mut methods = serde_json::Map::new();
        for method in op["methods"].as_array().unwrap() {
            let method = method.as_str().unwrap();
            let mut entry = json!({"summary":format!("{method} {path}"),"x-required-scope":op["scope"],"responses":{"200":{"description":"Operation completed","content":{"application/json":{"schema":{"type":"object"}}}},"401":{"description":"Sign in or supply a scoped bearer token"},"409":{"description":"Revision conflict; read current state before retrying"}}});
            for name in ["id", "name"] {
                if path.contains(&format!("{{{name}}}")) {
                    entry["parameters"] = json!([{"name":name,"in":"path","required":true,"schema":{"type":"string"}}]);
                }
            }
            let body = match (method, path) {
                ("PUT", "config") => Some("ConfigUpdate"),
                ("PUT", "secrets/{name}") => Some("SecretWrite"),
                ("POST", "lifecycle") => Some("Lifecycle"),
                ("POST", "credentials/slack") => Some("Credentials"),
                ("POST", "check") => Some("Check"),
                ("POST", "permissions/{id}/decision") => Some("PermissionDecision"),
                ("POST", "tokens") => Some("Token"),
                ("POST", "views") => Some("ViewCreate"),
                ("POST", "views/{id}/commands") => Some("ViewCommands"),
                ("POST", "views/{id}/ack") => Some("ViewAck"),
                _ => None,
            };
            if let Some(schema) = body {
                entry["requestBody"] = json!({"required":true,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}}}});
            }
            methods.insert(method.to_lowercase(), entry);
        }
        paths.insert(format!("/api/v1/{path}"), json!(methods));
    }
    json!({"openapi":"3.1.0","info":{"title":"ACP Slack Adapter Management","version":env!("CARGO_PKG_VERSION"),"description":"Optional management contract, separate from ACP. Cookie mutations require X-CSRF-Token."},"security":[{"bearer":[]}],"components":{"securitySchemes":{"bearer":{"type":"http","scheme":"bearer"}},"schemas":schemas},"paths":paths})
}
async fn openapi(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "observe", false)?;
    Ok(Json(openapi_value()))
}

fn cert_names(bind: IpAddr) -> Vec<String> {
    let mut names = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if !bind.is_unspecified() {
        names.push(bind.to_string());
    }
    if let Ok(ifaces) = get_if_addrs::get_if_addrs() {
        names.extend(ifaces.into_iter().map(|i| i.ip().to_string()));
    }
    if let Ok(host) = std::fs::read_to_string("/etc/hostname") {
        let host = host.trim();
        if !host.is_empty() {
            names.push(host.into());
        }
    }
    names.sort();
    names.dedup();
    names
}
fn prepare_tls(home: &FsPath, id: &str, names: Vec<String>) -> Result<()> {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use time::{Duration as Days, OffsetDateTime};
    private_dir(&home.join("tls"))?;
    let marker = home.join("tls/managed.json");
    let old = std::fs::read(&marker)
        .ok()
        .map(|b| serde_json::from_slice::<Value>(&b))
        .transpose()?;
    let now = OffsetDateTime::now_utc();
    let created = old
        .as_ref()
        .and_then(|v| v["created_at"].as_i64())
        .map(OffsetDateTime::from_unix_timestamp)
        .transpose()?
        .unwrap_or(now);
    let ca_params = || {
        let mut p = CertificateParams::default();
        p.distinguished_name.push(
            DnType::CommonName,
            format!("Slack Adapter {} Local CA", &id[..8]),
        );
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.not_before = created - Days::days(1);
        p.not_after = created + Days::days(3650);
        p
    };
    let ca_key = if old.is_some() {
        ensure!(
            home.join("tls/root-ca.crt").exists(),
            "Local CA is missing; restore its backup"
        );
        KeyPair::from_pem(&std::fs::read_to_string(home.join("tls/root-ca.key"))?)?
    } else {
        let key = KeyPair::generate()?;
        let cert = ca_params().self_signed(&key)?;
        private_write(&home.join("tls/root-ca.crt"), cert.pem().as_bytes())?;
        private_write(
            &home.join("tls/root-ca.key"),
            key.serialize_pem().as_bytes(),
        )?;
        key
    };
    if old.as_ref().is_some_and(|v| {
        v["sans"] == json!(names)
            && v["leaf_expires_at"].as_i64().unwrap_or(0) > (now + Days::days(30)).unix_timestamp()
    }) && home.join("tls/cert.pem").exists()
        && home.join("tls/key.pem").exists()
    {
        return Ok(());
    }
    ensure!(
        now + Days::days(365) < created + Days::days(3650),
        "Local CA is nearing expiry; replace its trust before renewing"
    );
    let issuer = Issuer::new(ca_params(), ca_key);
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::new(names.clone())?;
    p.distinguished_name
        .push(DnType::CommonName, format!("Slack Adapter {}", &id[..8]));
    p.not_before = now - Days::days(1);
    p.not_after = now + Days::days(365);
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = p.signed_by(&key, &issuer)?;
    private_write(&home.join("tls/cert.pem"), cert.pem().as_bytes())?;
    private_write(&home.join("tls/key.pem"), key.serialize_pem().as_bytes())?;
    private_write(
        &marker,
        &serde_json::to_vec_pretty(
            &json!({"created_at":created.unix_timestamp(),"leaf_expires_at":p.not_after.unix_timestamp(),"sans":names}),
        )?,
    )?;
    Ok(())
}
fn https_url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(_) => format!("https://{ip}:{port}"),
        IpAddr::V6(_) => format!("https://[{ip}]:{port}"),
    }
}
fn install_redacted_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        // This standalone service handles credentials and untrusted messages.
        // Panic payloads may contain either; never forward them to the journal.
        std::panic::set_hook(Box::new(|_| {
            eprintln!("Slack adapter task stopped unexpectedly. Check connection health and restart the adapter service if recovery is required.");
        }));
    });
}
pub async fn serve(home: PathBuf) -> Result<()> {
    install_redacted_panic_hook();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let home = home.canonicalize()?;
    let state = Arc::new(AppState::open(home)?);
    let owner = std::process::Command::new("id")
        .arg("-un")
        .output()
        .context("Cannot determine installation owner")?;
    ensure!(
        owner.status.success()
            && String::from_utf8_lossy(&owner.stdout).trim()
                == state.instance_config().server.owner,
        "Run this adapter service as its configured installation owner"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.home.join("service.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("This adapter already has a running service")?;
    let c = state.instance_config();
    let bind: IpAddr = c.server.bind.parse()?;
    let listener = TcpListener::bind((bind, c.server.port))
        .context("Cannot bind the adapter's configured management port")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    if c.server.port != port {
        let mut c = state.config.write().unwrap();
        c.server.port = port;
        c.revision += 1;
        c.save(&state.home)?;
    }
    let c = state.instance_config();
    let mut names = cert_names(bind);
    if let Some(host) = c
        .server
        .public_url
        .as_deref()
        .and_then(|u| reqwest::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        names.push(host);
        names.sort();
        names.dedup();
    }
    prepare_tls(&state.home, &c.id, names)?;
    let loopback = if bind.is_unspecified() {
        if bind.is_ipv4() {
            "127.0.0.1".parse()?
        } else {
            "::1".parse()?
        }
    } else {
        bind
    };
    let base_url = https_url(loopback, port);
    let mut urls = vec![base_url.clone()];
    if bind.is_unspecified()
        && let Ok(ifaces) = get_if_addrs::get_if_addrs()
    {
        urls.extend(
            ifaces
                .into_iter()
                .map(|i| i.ip())
                .filter(|ip| bind.is_ipv6() || ip.is_ipv4())
                .map(|ip| https_url(ip, port)),
        );
    }
    if let Some(public) = &c.server.public_url {
        urls.push(public.trim_end_matches('/').to_owned());
    }
    urls.sort();
    urls.dedup();
    let cert = state.home.join("tls/cert.pem");
    let key = state.home.join("tls/key.pem");
    let addresses = json!({"base_url":base_url,"urls":urls,"certificate_file":cert,"trust_certificate_file":state.home.join("tls/root-ca.crt"),"effective_server":c.server,"started_at":now_ms(),"pid":std::process::id()});
    private_write(
        &state.home.join("endpoints.json"),
        &serde_json::to_vec_pretty(&addresses)?,
    )?;
    *state.addresses.write().unwrap() = addresses;
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
    state.event(
        "management_service_started",
        json!({"version":env!("CARGO_PKG_VERSION"),"port":port}),
    )?;
    let runtime_home = state.home.clone();
    let changes = state.changes.subscribe();
    let runtime_task = tokio::spawn(monitor_runtime(
        state.clone(),
        runtime::supervise(runtime_home, changes),
    ));
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    let signal_task = tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        shutdown.graceful_shutdown(Some(Duration::from_secs(5)));
    });
    eprintln!(
        "Slack adapter ready at {} (sign in as {}).",
        c.server.public_url.as_deref().unwrap_or(&base_url),
        c.server.owner
    );
    let result = axum_server::from_tcp_rustls(listener, tls)?
        .handle(handle)
        .serve(router(state.clone()).into_make_service())
        .await;
    runtime_task.abort();
    let _ = runtime_task.await;
    signal_task.abort();
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_ca_stays_stable_when_addresses_change() {
        let temp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        prepare_tls(
            temp.path(),
            &id,
            vec!["localhost".into(), "127.0.0.1".into()],
        )
        .unwrap();
        let root = std::fs::read(temp.path().join("tls/root-ca.crt")).unwrap();
        let leaf = std::fs::read(temp.path().join("tls/cert.pem")).unwrap();
        prepare_tls(
            temp.path(),
            &id,
            vec!["localhost".into(), "127.0.0.1".into(), "192.168.1.2".into()],
        )
        .unwrap();
        assert_eq!(
            root,
            std::fs::read(temp.path().join("tls/root-ca.crt")).unwrap()
        );
        assert_ne!(
            leaf,
            std::fs::read(temp.path().join("tls/cert.pem")).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(temp.path().join("tls/root-ca.key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }
    fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
        let temp = tempfile::tempdir().unwrap();
        Config::new("Health fixture".into(), "tester".into())
            .save(temp.path())
            .unwrap();
        let state = Arc::new(AppState::open(temp.path().to_path_buf()).unwrap());
        (temp, state)
    }
    fn running(state: &AppState, revision: u64, socket: &str) {
        private_write(&state.home.join("runtime.json"),json!({"state":"running","config_revision":revision,"updated_at":now_ms(),"connections":[]}).to_string().as_bytes()).unwrap();
        state.event("connection", json!({"state":socket})).unwrap();
    }
    #[tokio::test]
    async fn active_slack_check_requires_current_connected_socket() {
        let (_temp, state) = fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/auth.test",
            post(|| async {
                (
                    [("x-oauth-scopes", "chat:write, files:read, files:write")],
                    Json(json!({"ok":true,"team_id":"T1","user_id":"U1","bot_id":"B1"})),
                )
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let mut c = state.instance_config();
        c.enabled = true;
        c.slack.loopback_test = true;
        c.slack.api_base = format!("http://{address}/");
        c.slack.team_id = "T1".into();
        c.slack.bot_user_id = "U1".into();
        c.slack.bot_id = "B1".into();
        c.slack.app_id = "A1".into();
        *state.config.write().unwrap() = c.clone();
        private_write(
            &state.home.join("secrets/slack.json"),
            br#"{"app_token":"test-app","bot_token":"test-bot"}"#,
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!(
                "Bearer {}",
                std::fs::read_to_string(state.home.join("secrets/control-token")).unwrap()
            )
            .parse()
            .unwrap(),
        );
        let Json(media) = media_check(State(state.clone()), headers.clone())
            .await
            .unwrap();
        assert_eq!(media["ready"], true);
        assert_eq!(
            status_value(&state).unwrap()["media"]["granted_scopes"],
            json!(["chat:write", "files:read", "files:write"])
        );
        for socket in ["disconnected", "reconnecting", "connecting"] {
            running(&state, c.revision, socket);
            let Json(value) = check(
                State(state.clone()),
                headers.clone(),
                Json(json!({"target":"slack"})),
            )
            .await
            .unwrap();
            assert_eq!(value["ok"], false, "{socket} is not ready");
            assert_eq!(value["slack"]["result"]["identity_verified"], true);
        }
        running(&state, c.revision - 1, "connected");
        assert_eq!(
            check(
                State(state.clone()),
                headers.clone(),
                Json(json!({"target":"slack"}))
            )
            .await
            .unwrap()
            .0["ok"],
            false,
            "Prior runtime revision is not current"
        );
        running(&state, c.revision, "connected");
        assert_eq!(
            check(
                State(state.clone()),
                headers.clone(),
                Json(json!({"target":"slack"}))
            )
            .await
            .unwrap()
            .0["ok"],
            true,
            "Current running and connected socket succeeds"
        );
        private_write(
            &state.home.join("runtime.json"),
            json!({"state":"failed","config_revision":c.revision,"updated_at":now_ms()})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            check(State(state), headers, Json(json!({"target":"slack"})))
                .await
                .unwrap()
                .0["ok"],
            false,
            "Failed runtime with stale connected audit is not healthy"
        );
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    async fn supervisor_failure_is_observable_without_error_payloads() {
        for outcome in ["return", "error", "panic"] {
            let (_temp, state) = fixture();
            monitor_runtime(state.clone(), async move {
                match outcome {
                    "return" => Ok(()),
                    "error" => Err(anyhow::anyhow!("private-error-payload")),
                    _ => panic!("simulated supervisor failure"),
                }
            })
            .await;
            let status = status_value(&state).unwrap();
            assert_eq!(status["runtime"]["state"], "failed");
            assert!(!status.to_string().contains("private-error-payload"));
            let raw = std::fs::read_to_string(state.home.join("runtime.json")).unwrap();
            assert!(!raw.contains("private-error-payload"));
            let activity = Store::open(&state.home).unwrap().activity().unwrap();
            assert_eq!(activity["items"][0]["kind"], "runtime_supervisor_failed");
            assert!(!activity.to_string().contains("private-error-payload"));
            // A later stale file cannot hide the supervisor's in-memory failure.
            running(&state, state.instance_config().revision, "connected");
            assert_eq!(status_value(&state).unwrap()["runtime"]["state"], "failed");
        }
    }
}
