use acp_slack_adapter::{
    auth,
    config::{AuthMode, Config, WebTransport},
    server,
    state::AppState,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new("Native connector".into(), "local-owner".into());
    config.server.auth_mode = AuthMode::LocalPassword;
    config.server.web_transport = WebTransport::LocalHttp;
    config.server.port = 21873;
    config.server.public_url = Some("http://127.0.0.1:21873".into());
    config.save(dir.path()).unwrap();
    amplifier_native_platform::private_write(
        &dir.path().join("secrets/first-account"),
        &serde_json::to_vec(
            &json!({"token":"a".repeat(64),"expires_at":acp_slack_adapter::model::now_ms()+60000}),
        )
        .unwrap(),
    )
    .unwrap();
    let state = Arc::new(AppState::open(dir.path().to_owned()).unwrap());
    (dir, state)
}
async fn call(
    state: &Arc<AppState>,
    method: &str,
    path: &str,
    body: Value,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut r = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:21873")
        .header("origin", "http://127.0.0.1:21873")
        .header("content-type", "application/json");
    if let Some(cookie) = cookie {
        r = r.header("cookie", cookie);
    }
    if let Some(csrf) = csrf {
        r = r.header("x-csrf-token", csrf);
    }
    let response = server::router(state.clone())
        .oneshot(r.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}
#[test]
fn legacy_defaults_and_loopback_constraints() {
    let c = Config::new("Existing".into(), "owner".into());
    let mut v = serde_json::to_value(c).unwrap();
    v["server"].as_object_mut().unwrap().remove("auth_mode");
    v["server"].as_object_mut().unwrap().remove("web_transport");
    let mut c: Config = serde_json::from_value(v).unwrap();
    assert_eq!(c.server.auth_mode, AuthMode::Pam);
    assert_eq!(c.server.web_transport, WebTransport::Https);
    c.server.web_transport = WebTransport::LocalHttp;
    assert!(c.validate().is_err());
    c.server.auth_mode = AuthMode::LocalPassword;
    assert!(c.validate().is_ok());
    c.server.bind = "0.0.0.0".into();
    assert!(c.validate().is_err());
    c.server.bind = "127.0.0.1".into();
    c.server.public_url = Some("http://example.org".into());
    assert!(c.validate().is_err());
}
#[tokio::test]
async fn first_account_requires_private_link_and_never_overwrites_password() {
    let (dir, state) = fixture();
    let before = state.instance_config();
    let (_, _, session) = call(&state, "GET", "/api/v1/auth/session", json!({}), None, None).await;
    assert_eq!(session["first_account_needed"], true);
    for input in [
        json!({"setup_token":"b".repeat(64),"password":"correct long password"}),
        json!({"setup_token":"a".repeat(64),"password":"short"}),
    ] {
        let (status, _, _) = call(
            &state,
            "POST",
            "/api/v1/auth/first-account",
            input,
            None,
            None,
        )
        .await;
        assert!(status.is_client_error());
    }
    let (status, headers, body) = call(
        &state,
        "POST",
        "/api/v1/auth/first-account",
        json!({"setup_token":"a".repeat(64),"password":"correct long password"}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["authenticated"], true);
    let cookie = headers["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(!cookie.contains("Secure;"));
    let encoded = std::fs::read(dir.path().join("secrets/local-password")).unwrap();
    assert!(String::from_utf8_lossy(&encoded).starts_with("$argon2id$"));
    assert!(!String::from_utf8_lossy(&encoded).contains("correct long"));
    let (repeat, _, _) = call(
        &state,
        "POST",
        "/api/v1/auth/first-account",
        json!({"setup_token":"a".repeat(64),"password":"different password"}),
        None,
        None,
    )
    .await;
    assert!(repeat.is_client_error());
    assert_eq!(
        encoded,
        std::fs::read(dir.path().join("secrets/local-password")).unwrap()
    );
    let (_, _, session) = call(
        &state,
        "GET",
        "/api/v1/auth/session",
        json!({}),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(session["first_account_needed"], false);
    assert_eq!(session["authenticated"], true);
    let (forbidden, _, _) = call(
        &state,
        "POST",
        "/api/v1/auth/logout",
        json!({}),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(forbidden, StatusCode::FORBIDDEN);
    let (logout, _, _) = call(
        &state,
        "POST",
        "/api/v1/auth/logout",
        json!({}),
        Some(cookie),
        body["csrf"].as_str(),
    )
    .await;
    assert_eq!(logout, StatusCode::OK);
    let (login, _, _) = call(
        &state,
        "POST",
        "/api/v1/auth/login",
        json!({"username":"local-owner","password":"correct long password"}),
        None,
        None,
    )
    .await;
    assert_eq!(login, StatusCode::OK);
    let after = state.instance_config();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after.clone()).unwrap()
    );
    assert!(!after.enabled);
    assert!(after.acp.command.is_empty());
    assert!(!after.setup_complete);
}
#[tokio::test]
async fn local_listener_rejects_dns_rebinding_and_cross_origin_requests() {
    let (_, state) = fixture();
    for (host, origin) in [
        ("evil.example:21873", "http://evil.example:21873"),
        ("127.0.0.1:21873", "https://elsewhere.example"),
    ] {
        let request = Request::builder()
            .uri("/api/v1/auth/session")
            .header("host", host)
            .header("origin", origin)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            server::router(state.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
}
#[tokio::test]
async fn proof_is_correlated_and_auth_modes_cannot_be_switched_by_config() {
    let (dir, state) = fixture();
    let nonce = "b".repeat(64);
    let (status, _, proof) = call(
        &state,
        "GET",
        &format!("/api/v1/local-proof?nonce={nonce}"),
        json!({}),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secret = std::fs::read_to_string(dir.path().join("secrets/control-token")).unwrap();
    let config = state.instance_config();
    assert!(auth::verify_local_proof(
        &config,
        &nonce,
        secret.trim(),
        &proof
    ));
    assert!(!auth::verify_local_proof(
        &config,
        &"c".repeat(64),
        secret.trim(),
        &proof
    ));
    let mut replacement = config.clone();
    replacement.server.auth_mode = AuthMode::Pam;
    replacement.server.web_transport = WebTransport::Https;
    replacement.server.public_url = None;
    assert!(server::replace_config(&state, config.revision, replacement, "owner").is_err());
}
#[tokio::test]
async fn spoofed_local_http_listener_never_receives_control_credential() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (dir, state) = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut c = state.instance_config();
    c.server.port = port;
    c.server.public_url = Some(format!("http://127.0.0.1:{port}"));
    c.save(dir.path()).unwrap();
    let fake = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 8192];
        let n = stream.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..n]).into_owned();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
        request
    });
    assert!(
        acp_slack_adapter::desktop::local_api(dir.path(), "GET", "status", None)
            .await
            .is_err()
    );
    let request = fake.await.unwrap();
    assert!(request.starts_with("GET /api/v1/local-proof?nonce="));
    assert!(!request.to_lowercase().contains("authorization"));
}
