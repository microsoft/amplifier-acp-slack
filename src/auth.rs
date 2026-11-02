use crate::{
    error::{ApiError, ApiResult},
    state::{AppState, now_ms},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString};
use std::sync::Arc;

struct PasswordConversation {
    password: String,
    answered: bool,
}
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
        same_origin(headers)?;
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
pub fn same_origin(headers: &HeaderMap) -> ApiResult<()> {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(ApiError::forbidden)?;
        if origin.to_str().ok() != Some(format!("https://{host}").as_str()) {
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
    same_origin(&headers)?;
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
    let service = config.server.pam_service;
    let success = if login.username == owner {
        let worker = tokio::task::spawn_blocking(move || {
            // The permit stays with a timed-out PAM call, bounding blocked workers too.
            let _permit = permit;
            let username = login.username;
            let conversation = PasswordConversation {
                password: login.password,
                answered: false,
            };
            let Ok(mut context) = pam_client::Context::new(&service, Some(&username), conversation)
            else {
                return false;
            };
            context.authenticate(pam_client::Flag::NONE).is_ok()
                && context.acct_mgmt(pam_client::Flag::NONE).is_ok()
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
        state.event("auth.failed", json!({"method":"pam"}))?;
        return Err(ApiError::unauthorized());
    }
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
    state.event("auth.signed_in", json!({"actor":owner,"method":"pam"}))?;
    let cookie = format!(
        "{}={}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=28800",
        cookie_name(&state),
        token
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
    match check(&state, &headers, "observe", false) {
        Ok(p) => Ok(Json(
            json!({"authenticated":true,"owner":state.instance_config().server.owner,"csrf":p.csrf}),
        )),
        Err(e) if e.0 == StatusCode::UNAUTHORIZED => Ok(Json(
            json!({"authenticated":false,"owner":state.instance_config().server.owner,"csrf":null}),
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
                "{}=; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
                cookie_name(&state)
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}

#[cfg(test)]
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
