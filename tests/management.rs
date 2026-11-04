use acp_slack_adapter::{auth, config::Config, server, state::AppState, store::Store};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn fixture() -> (tempfile::TempDir, Arc<AppState>, String) {
    let temp = tempfile::tempdir().unwrap();
    let c = Config::new("Test connection".into(), "tester".into());
    c.save(temp.path()).unwrap();
    let state = Arc::new(AppState::open(temp.path().to_path_buf()).unwrap());
    let token = std::fs::read_to_string(temp.path().join("secrets/control-token")).unwrap();
    (temp, state, token)
}

#[test]
fn connection_status_cannot_reuse_a_previous_process_socket() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    store
        .audit("connection", json!({"state":"connected"}))
        .unwrap();
    assert_eq!(
        store.status().unwrap()["slack_connection"]["state"],
        "connected"
    );
    store.recover().unwrap();
    assert_eq!(
        store.status().unwrap()["slack_connection"]["state"],
        "connecting"
    );
    store
        .audit("connection", json!({"state":"connected"}))
        .unwrap();
    store
        .audit(
            "connection_failed",
            json!({"category":"update_coordination_unavailable"}),
        )
        .unwrap();
    assert_eq!(
        store.status().unwrap()["slack_connection"]["state"],
        "failed"
    );
    store.audit("process_started", json!({})).unwrap();
    store
        .audit("connection", json!({"state":"connected"}))
        .unwrap();
    assert_eq!(
        store.status().unwrap()["slack_connection"]["state"],
        "connected"
    );
    store.audit("process_stopped", json!({})).unwrap();
    assert_eq!(
        store.status().unwrap()["slack_connection"]["state"],
        "stopped"
    );
}
async fn request(
    state: Arc<AppState>,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = server::router(state)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn management_requires_auth_and_enforces_scopes() {
    let (_temp, state, token) = fixture();
    for route in [
        "config",
        "status",
        "descriptor",
        "schema",
        "activity",
        "conversations",
        "permissions",
        "tokens",
        "views",
    ] {
        assert_eq!(
            request(
                state.clone(),
                "GET",
                &format!("/api/v1/{route}"),
                None,
                None
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (_, session) = request(state.clone(), "GET", "/api/v1/auth/session", None, None).await;
    assert_eq!(session["authenticated"], false);
    let observer = auth::issue_token(&state, "observer", &["observe".into()], 300).unwrap();
    let observer = observer["token"].as_str().unwrap();
    assert_eq!(
        request(state.clone(), "GET", "/api/v1/status", Some(observer), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(state.clone(), "GET", "/api/v1/config", Some(observer), None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            state.clone(),
            "POST",
            "/api/v1/lifecycle",
            Some(observer),
            Some(json!({"action":"start"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            state.clone(),
            "GET",
            "/api/v1/permissions",
            Some(observer),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            state.clone(),
            "POST",
            "/api/v1/permissions/any/decision",
            Some(observer),
            Some(json!({"option_id":"yes"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, tokens) = request(state, "GET", "/api/v1/tokens", Some(&token), None).await;
    let text = tokens.to_string();
    assert!(!text.contains(observer));
    assert!(!text.contains("hash"));
}
#[tokio::test]
async fn saves_are_revision_checked_and_cannot_change_observed_identity() {
    let (_temp, state, token) = fixture();
    let mut c = state.instance_config();
    let rev = c.revision;
    c.label = "Renamed".into();
    let (status, saved) = request(
        state.clone(),
        "PUT",
        "/api/v1/config",
        Some(&token),
        Some(json!({"expected_revision":rev,"config":c})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["revision"], rev + 1);
    assert_eq!(
        request(
            state.clone(),
            "PUT",
            "/api/v1/config",
            Some(&token),
            Some(json!({"expected_revision":rev,"config":c}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut c = state.instance_config();
    Store::open(&state.home).unwrap().bind_identity(&c).unwrap();
    c.acp.command = "different-agent".into();
    let (status, error) = request(
        state.clone(),
        "PUT",
        "/api/v1/config",
        Some(&token),
        Some(json!({"expected_revision":c.revision,"config":c})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"].as_str().unwrap().contains("fresh adapter"));
    assert_eq!(
        request(
            state,
            "POST",
            "/api/v1/setup/complete",
            Some(&token),
            Some(json!({}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn ui_commands_are_atomic_and_never_enter_activity() {
    let (_temp, state, token) = fixture();
    let (_, view) = request(
        state.clone(),
        "POST",
        "/api/v1/views",
        Some(&token),
        Some(json!({"label":"Browser"})),
    )
    .await;
    let id = view["id"].as_str().unwrap();
    let path = format!("/api/v1/views/{id}/commands");
    let (status,_)=request(state.clone(),"POST",&path,Some(&token),Some(json!({"expected_revision":1,"commands":[{"type":"navigate","route":"activity"},{"type":"set_draft","path":"bot_token","value":"xoxb-private"}]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, current) = request(
        state.clone(),
        "GET",
        &format!("/api/v1/views/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(current["state"]["route"], "setup");
    assert_eq!(current["revision"], 1);
    let (_, changed) = request(
        state.clone(),
        "POST",
        &path,
        Some(&token),
        Some(json!({"expected_revision":1,"commands":[{"type":"navigate","route":"activity"}]})),
    )
    .await;
    assert_eq!(changed["revision"], 2);
    assert_eq!(
        request(
            state.clone(),
            "POST",
            &format!("/api/v1/views/{id}/ack"),
            Some(&token),
            Some(json!({"revision":2}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, activity) = request(state, "GET", "/api/v1/activity", Some(&token), None).await;
    assert_eq!(activity["items"].as_array().unwrap().len(), 0);
}
#[tokio::test]
async fn browser_login_survives_other_apps_cookies_in_separate_headers() {
    let (_temp, state, _) = fixture();
    let token = "connector-browser-session";
    let csrf = "connector-csrf";
    state
        .db
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO browser_sessions(hash,owner,csrf,expires_at) VALUES(?1,'tester',?2,?3)",
            rusqlite::params![
                auth::digest(token),
                csrf,
                acp_slack_adapter::model::now_ms() + 60000
            ],
        )
        .unwrap();
    let cookie = format!(
        "asa_{}={token}",
        state.instance_config().id.replace('-', "")
    );
    // Cookies share a hostname across ports. HTTP/2 can send one field per
    // cookie, with the main agent's cookie preceding this connector's cookie.
    for split in [false, true] {
        for (method, path) in [("GET", "/api/v1/auth/session"), ("POST", "/api/v1/views")] {
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost")
                .header("origin", "https://localhost")
                .header("content-type", "application/json")
                .header("x-csrf-token", csrf);
            req = if split {
                req.header("cookie", "ai_other=other-app-cookie")
                    .header("cookie", &cookie)
            } else {
                req.header("cookie", format!("ai_other=other-app-cookie; {cookie}"))
            };
            let response = server::router(state.clone())
                .oneshot(req.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{method} {path}, split={split}"
            );
            if method == "GET" {
                let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                let session: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(session["authenticated"], true, "split={split}");
            }
        }
    }
}

#[tokio::test]
async fn cookie_mutations_require_csrf_and_same_origin() {
    let (_temp, state, _) = fixture();
    let cookie = "browser-cookie";
    let csrf = "csrf-value";
    state
        .db
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO browser_sessions(hash,owner,csrf,expires_at) VALUES(?1,'tester',?2,?3)",
            rusqlite::params![
                auth::digest(cookie),
                csrf,
                acp_slack_adapter::model::now_ms() + 60000
            ],
        )
        .unwrap();
    let cookie = format!(
        "asa_{}={cookie}",
        state.instance_config().id.replace('-', "")
    );
    for (origin, csrf_header, expected) in [
        ("https://localhost", None, StatusCode::FORBIDDEN),
        ("https://other.example", Some(csrf), StatusCode::FORBIDDEN),
        ("https://localhost", Some(csrf), StatusCode::OK),
    ] {
        let mut r = Request::builder()
            .method("POST")
            .uri("/api/v1/views")
            .header("content-type", "application/json")
            .header("host", "localhost")
            .header("origin", origin)
            .header("cookie", &cookie);
        if let Some(csrf) = csrf_header {
            r = r.header("x-csrf-token", csrf);
        }
        assert_eq!(
            server::router(state.clone())
                .oneshot(r.body(Body::from("{} ")).unwrap())
                .await
                .unwrap()
                .status(),
            expected
        );
    }
}
#[tokio::test]
async fn credentials_remain_write_only_and_local_control_cannot_be_revoked() {
    let (_temp, state, token) = fixture();
    let secret = json!({"app_token":"not-an-app-token","bot_token":"not-a-bot-token"});
    let (status, response) = request(
        state.clone(),
        "POST",
        "/api/v1/credentials/slack",
        Some(&token),
        Some(secret),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!response.to_string().contains("not-an-app-token"));
    assert_eq!(
        request(
            state.clone(),
            "GET",
            "/api/v1/credentials/slack",
            Some(&token),
            None
        )
        .await
        .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(
            state,
            "DELETE",
            "/api/v1/tokens/local-control",
            Some(&token),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn generic_credentials_are_private_write_only_and_refs_protect_deletion() {
    let (_temp, state, token) = fixture();
    let (status, saved) = request(
        state.clone(),
        "PUT",
        "/api/v1/secrets/provider-key",
        Some(&token),
        Some(json!({"value":"private-canary-value"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!saved.to_string().contains("private-canary-value"));
    let (_, names) = request(state.clone(), "GET", "/api/v1/secrets", Some(&token), None).await;
    assert_eq!(names["items"], json!(["provider-key"]));
    assert_eq!(
        request(
            state.clone(),
            "GET",
            "/api/v1/secrets/provider-key",
            Some(&token),
            None
        )
        .await
        .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(
            state.clone(),
            "PUT",
            "/api/v1/secrets/control-token",
            Some(&token),
            Some(json!({"value":"malicious replacement"}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut c = state.instance_config();
    c.acp
        .secret_env
        .insert("PROVIDER_KEY".into(), "provider-key".into());
    server::replace_config(&state, c.revision, c, "test").unwrap();
    assert_eq!(
        request(
            state.clone(),
            "DELETE",
            "/api/v1/secrets/provider-key",
            Some(&token),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, activity) = request(state, "GET", "/api/v1/activity", Some(&token), None).await;
    assert!(!activity.to_string().contains("private-canary-value"));
}

#[test]
fn openapi_references_are_self_contained() {
    let api = server::openapi_value();
    fn visit(value: &Value, root: &Value) {
        match value {
            Value::Object(m) => {
                if let Some(reference) = m.get("$ref").and_then(Value::as_str) {
                    assert!(reference.starts_with('#'));
                    assert!(
                        root.pointer(&reference[1..]).is_some(),
                        "Unresolved schema reference {reference}"
                    );
                }
                for v in m.values() {
                    visit(v, root);
                }
            }
            Value::Array(a) => {
                for v in a {
                    visit(v, root);
                }
            }
            _ => {}
        }
    }
    visit(&api, &api);
    assert!(api["paths"]["/api/v1/secrets/{name}"]["put"]["requestBody"].is_object());
}

#[tokio::test]
async fn update_policy_requires_admin_and_cannot_grant_implicit_owner_access() {
    let (_temp, state, token) = fixture();
    assert_eq!(
        request(state.clone(), "GET", "/api/v1/updates", None, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let context = auth::issue_token(&state, "read-only context", &["context".into()], 300).unwrap()
        ["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = json!({"expected_revision":0,"enabled":true,"agent_enabled":true});
    assert_eq!(
        request(
            state.clone(),
            "PUT",
            "/api/v1/updates/policy",
            Some(&context),
            Some(body.clone())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            state.clone(),
            "POST",
            "/api/v1/updates",
            Some(&context),
            Some(json!({"action":"plan"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            state.clone(),
            "PUT",
            "/api/v1/updates/policy",
            Some(&token),
            Some(body)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, status) = request(state.clone(), "GET", "/api/v1/updates", Some(&token), None).await;
    assert_eq!(status["enabled"], false);
}
