use crate::{
    config::{AuthMode, WebTransport},
    error::{ApiError, ApiResult},
    state::{AppState, now_ms},
};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::sync::Arc;

#[cfg(target_os = "linux")]
struct PasswordConversation {
    password: String,
    answered: bool,
}
#[cfg(target_os = "linux")]
impl pam_client::ConversationHandler for PasswordConversation {
    fn prompt_echo_on(&mut self, _: &CStr) -> Result<CString, pam_client::ErrorCode> {
        Err(pam_client::ErrorCode::CONV_ERR)
    }
    fn prompt_echo_off(&mut self, prompt: &CStr) -> Result<CString, pam_client::ErrorCode> {
        // This first UI supports a single ordinary password, not MFA or password changes.
        if self.answered
            || !prompt
                .to_string_lossy()
                .trim()
                .eq_ignore_ascii_case("password:")
        {
            return Err(pam_client::ErrorCode::CONV_ERR);
        }
        self.answered = true;
        CString::new(self.password.clone()).map_err(|_| pam_client::ErrorCode::CONV_ERR)
    }
    fn text_info(&mut self, _: &CStr) {}
    fn error_msg(&mut self, _: &CStr) {}
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn cookie_name(state: &AppState) -> String {
    format!("asa_{}", state.instance_config().id.replace('-', ""))
}
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        // HTTP/2 may send each cookie in its own field. Other apps on the
        // same hostname (including different ports) can precede our cookie.
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|v| {
            let (k, v) = v.trim().split_once('=')?;
            (k == name).then(|| v.to_owned())
        })
}
fn constant_equal(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |sum, (a, b)| sum | (a ^ b))
            == 0
}
#[derive(Clone)]
pub struct Principal {
    pub actor: String,
    pub csrf: Option<String>,
    pub session_hash: Option<String>,
}

pub fn check(
    state: &AppState,
    headers: &HeaderMap,
    scope: &str,
    mutation: bool,
) -> ApiResult<Principal> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        let token = value
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        let row: Option<(String, String)> = state
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT id,scopes FROM tokens WHERE hash=?1 AND expires_at>?2",
                params![digest(token), now_ms()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (id, scopes) = row.ok_or_else(ApiError::unauthorized)?;
        let scopes: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
        if !scopes.iter().any(|s| s == "admin" || s == scope) {
            return Err(ApiError::forbidden());
        }
        return Ok(Principal {
            actor: format!("token:{id}"),
            csrf: None,
            session_hash: None,
        });
    }
    let token = cookie_value(headers, &cookie_name(state)).ok_or_else(ApiError::unauthorized)?;
    let hash = digest(&token);
    let row: Option<(String, String)> = state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT owner,csrf FROM browser_sessions WHERE hash=?1 AND expires_at>?2",
            params![hash, now_ms()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (owner, csrf) = row.ok_or_else(ApiError::unauthorized)?;
    if mutation {
        same_origin(state, headers)?;
        if !headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| constant_equal(v, &csrf))
        {
            return Err(ApiError::forbidden());
        }
    }
    Ok(Principal {
        actor: owner,
        csrf: Some(csrf),
        session_hash: Some(hash),
    })
}
pub fn same_origin(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(ApiError::forbidden)?;
        let scheme = if state.instance_config().server.web_transport == WebTransport::LocalHttp {
            "http"
        } else {
            "https"
        };
        if origin.to_str().ok() != Some(format!("{scheme}://{host}").as_str()) {
            return Err(ApiError::forbidden());
        }
    }
    Ok(())
}
pub fn issue_token(
    state: &AppState,
    name: &str,
    scopes: &[String],
    seconds: u64,
) -> ApiResult<Value> {
    if name.trim().is_empty()
        || name.len() > 200
        || scopes.is_empty()
        || scopes
            .iter()
            .any(|s| !["admin", "observe", "ui", "context"].contains(&s.as_str()))
        || !(60..=366 * 86400).contains(&seconds)
    {
        return Err(ApiError::bad(
            "Supply a name, scopes (admin/observe/ui/context), and a lifetime between 60 seconds and 366 days",
        ));
    }
    let token = random_token();
    let id = uuid::Uuid::new_v4().to_string();
    let at = now_ms();
    let expires = at + seconds as i64 * 1000;
    state.db.lock().unwrap().execute(
        "INSERT INTO tokens(hash,id,name,scopes,expires_at,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            digest(&token),
            id,
            name,
            serde_json::to_string(scopes).unwrap(),
            expires,
            at
        ],
    )?;
    Ok(
        json!({"id":id,"name":name,"scopes":scopes,"expires_at":expires,"created_at":at,"token":token}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    username: String,
    password: String,
}
pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(login): Json<Login>,
) -> ApiResult<Response> {
    same_origin(&state, &headers)?;
    if login.username.len() > 100 || login.password.len() > 1024 || login.password.is_empty() {
        return Err(ApiError::unauthorized());
    }
    let permit = state.login_gate.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Please wait before trying to sign in again".into(),
            None,
        )
    })?;
    let config = state.instance_config();
    {
        let db = state.db.lock().unwrap();
        let locked: Option<i64> = db
            .query_row(
                "SELECT locked_until FROM login_attempts WHERE key='owner'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if locked.is_some_and(|until| until > now_ms()) {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "Please wait before trying to sign in again".into(),
                None,
            ));
        }
        db.execute(
            "DELETE FROM browser_sessions WHERE expires_at<=?1",
            [now_ms()],
        )?;
    }
    let owner = config.server.owner.clone();
    let mode = config.server.auth_mode;
    let service = config.server.pam_service;
    let password_path = state.home.join("secrets/local-password");
    let success = if login.username == owner {
        let worker =
            tokio::task::spawn_blocking(move || {
                // Keep the permit with expensive or stalled authentication work.
                let _permit = permit;
                match mode {
                    AuthMode::LocalPassword => std::fs::read_to_string(password_path)
                        .ok()
                        .is_some_and(|encoded| {
                            amplifier_native_platform::local_auth::verify_password(
                                encoded.trim(),
                                &login.password,
                            )
                        }),
                    AuthMode::Pam => pam_authenticate(&service, &login.username, login.password),
                }
            });
        tokio::time::timeout(std::time::Duration::from_secs(20), worker)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    } else {
        false
    };
    if !success {
        state.db.lock().unwrap().execute("INSERT INTO login_attempts(key,failures,locked_until) VALUES('owner',1,0) ON CONFLICT(key) DO UPDATE SET failures=CASE WHEN locked_until>0 AND locked_until<?1 THEN 1 ELSE failures+1 END, locked_until=CASE WHEN failures>=4 THEN ?1+60000 ELSE 0 END",[now_ms()])?;
        state.event("auth.failed", json!({"method":mode}))?;
        return Err(ApiError::unauthorized());
    }
    signed_in(&state, &owner, mode)
}
fn signed_in(state: &AppState, owner: &str, mode: AuthMode) -> ApiResult<Response> {
    let token = random_token();
    let csrf = random_token();
    {
        let db = state.db.lock().unwrap();
        db.execute("DELETE FROM login_attempts WHERE key='owner'", [])?;
        db.execute(
            "INSERT INTO browser_sessions(hash,owner,csrf,expires_at) VALUES(?1,?2,?3,?4)",
            params![digest(&token), owner, csrf, now_ms() + 8 * 3600 * 1000],
        )?;
    }
    state.event("auth.signed_in", json!({"actor":owner,"method":mode}))?;
    let cookie = format!(
        "{}={}; {}HttpOnly; SameSite=Strict; Path=/; Max-Age=28800",
        cookie_name(state),
        token,
        secure_cookie(state)
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"authenticated":true,"owner":owner,"csrf":csrf})),
    )
        .into_response())
}
pub async fn session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let config = state.instance_config();
    let first_account_needed = config.server.auth_mode == AuthMode::LocalPassword
        && !state.home.join("secrets/local-password").exists();
    match check(&state, &headers, "observe", false) {
        Ok(p) => Ok(Json(
            json!({"authenticated":true,"owner":state.instance_config().server.owner,"csrf":p.csrf,"auth_mode":config.server.auth_mode,"first_account_needed":first_account_needed}),
        )),
        Err(e) if e.0 == StatusCode::UNAUTHORIZED => Ok(Json(
            json!({"authenticated":false,"owner":state.instance_config().server.owner,"csrf":null,"auth_mode":config.server.auth_mode,"first_account_needed":first_account_needed}),
        )),
        Err(e) => Err(e),
    }
}
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    let p = check(&state, &headers, "observe", true)?;
    if let Some(hash) = p.session_hash {
        state
            .db
            .lock()
            .unwrap()
            .execute("DELETE FROM browser_sessions WHERE hash=?1", [hash])?;
    }
    state.event("auth.signed_out", json!({"actor":p.actor}))?;
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "{}=; {}HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
                cookie_name(&state),
                secure_cookie(&state)
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use pam_client::ConversationHandler;
    #[test]
    fn password_is_not_reused_for_another_authentication_challenge() {
        let mut c = PasswordConversation {
            password: "test".into(),
            answered: false,
        };
        assert!(c.prompt_echo_off(c"Verification code:").is_err());
        assert!(c.prompt_echo_off(c"Password:").is_ok());
        assert!(c.prompt_echo_off(c"Password:").is_err());
        assert!(c.prompt_echo_on(c"Username:").is_err());
    }
}

fn secure_cookie(state: &AppState) -> &'static str {
    if state.instance_config().server.web_transport == WebTransport::LocalHttp {
        ""
    } else {
        "Secure; "
    }
}
#[cfg(target_os = "linux")]
fn pam_authenticate(service: &str, username: &str, password: String) -> bool {
    let conversation = PasswordConversation {
        password,
        answered: false,
    };
    let Ok(mut context) = pam_client::Context::new(service, Some(username), conversation) else {
        return false;
    };
    context.authenticate(pam_client::Flag::NONE).is_ok()
        && context.acct_mgmt(pam_client::Flag::NONE).is_ok()
}
#[cfg(not(target_os = "linux"))]
fn pam_authenticate(_: &str, _: &str, _: String) -> bool {
    false
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstAccount {
    setup_token: String,
    password: String,
    #[serde(default)]
    start_at_signin: bool,
}
pub async fn first_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<FirstAccount>,
) -> ApiResult<Response> {
    same_origin(&state, &headers)?;
    let config = state.instance_config();
    if config.server.auth_mode != AuthMode::LocalPassword
        || config.server.web_transport != WebTransport::LocalHttp
    {
        return Err(ApiError::forbidden());
    }
    if state.home.join("secrets/local-password").exists() {
        return Err(ApiError::conflict(
            json!({"account_created":true,"next":"Sign in with the password you already chose"}),
        ));
    }
    let _permit = state
        .login_gate
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::bad("Sign-in is busy; try again shortly"))?;
    let capability: Value = serde_json::from_slice(
        &std::fs::read(state.home.join("secrets/first-account"))
            .map_err(|_| ApiError::forbidden())?,
    )
    .map_err(|_| ApiError::forbidden())?;
    if input.setup_token.len() != 64
        || !capability["token"]
            .as_str()
            .is_some_and(|token| constant_equal(token, &input.setup_token))
        || capability["expires_at"].as_i64().unwrap_or(0) <= now_ms()
    {
        return Err(ApiError::forbidden());
    }
    amplifier_native_platform::local_auth::validate_password(&input.password)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    let encoded = tokio::task::spawn_blocking(move || {
        amplifier_native_platform::local_auth::hash_password(&input.password)
    })
    .await
    .map_err(|_| ApiError::bad("Could not prepare sign-in; try again"))??;
    let path = state.home.join("secrets/local-password");
    if let Err(error) = amplifier_native_platform::private_create(&path, encoded.as_bytes()) {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
        {
            return Err(ApiError::conflict(
                json!({"account_created":true,"next":"Sign in with the password you already chose"}),
            ));
        }
        return Err(error.into());
    }
    // The successful create-only credential is the authority. Cleanup failure cannot recreate it.
    let _ = std::fs::remove_file(state.home.join("secrets/first-account"));
    state.event("auth.account_created", json!({"owner":config.server.owner}))?;
    let startup_error = if input.start_at_signin {
        crate::desktop::set_startup(&state.home, true)
            .err()
            .map(|e| e.to_string())
    } else {
        None
    };
    let mut response = signed_in(&state, &config.server.owner, AuthMode::LocalPassword)?;
    if startup_error.is_some() {
        response
            .headers_mut()
            .insert("x-startup-warning", "not-installed".parse().unwrap());
    }
    // Startup state is readable after sign-in, independent of account creation.
    Ok(response)
}

pub const PROOF_DOMAIN: &str = "acp-slack-adapter.local-proof/1";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofQuery {
    nonce: String,
}
pub async fn local_proof(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProofQuery>,
) -> ApiResult<Json<Value>> {
    let config = state.instance_config();
    if config.server.web_transport != WebTransport::LocalHttp {
        return Err(ApiError::missing());
    }
    let origin = config
        .server
        .public_url
        .as_deref()
        .ok_or_else(ApiError::forbidden)?;
    let secret = std::fs::read_to_string(state.home.join("secrets/control-token"))?;
    let proof = amplifier_native_platform::local_auth::listener_proof(
        PROOF_DOMAIN,
        &config.id,
        origin,
        &query.nonce,
        secret.trim(),
    )
    .map_err(|_| ApiError::bad("Invalid local proof challenge"))?;
    Ok(Json(
        json!({"schema":1,"id":config.id,"origin":origin,"nonce":query.nonce,"proof":proof}),
    ))
}
pub fn verify_local_proof(
    config: &crate::config::Config,
    nonce: &str,
    secret: &str,
    value: &Value,
) -> bool {
    let Some(origin) = config.server.public_url.as_deref() else {
        return false;
    };
    value["schema"] == 1
        && value["id"] == config.id
        && value["origin"] == origin
        && value["nonce"] == nonce
        && value["proof"].as_str().is_some_and(|proof| {
            amplifier_native_platform::local_auth::verify_listener_proof(
                PROOF_DOMAIN,
                &config.id,
                origin,
                nonce,
                secret,
                proof,
            )
            .is_ok()
        })
}
