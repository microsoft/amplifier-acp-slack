use acp_slack_adapter::{
    config::{Config, private_write},
    identity,
};
use axum::{
    Form, Json, Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn portrait(color: u8, kind: &str) -> Value {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(512, 512, image::Rgba([color, 10, 20, 255]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    let bytes = out.into_inner();
    json!({"schema":"agent-identity/1","agent_id":"test-agent","display_name":"Test teammate","revision":color.to_string(),"avatar":{"kind":kind,"media_type":"image/png","data":STANDARD.encode(&bytes),"sha256":format!("{:x}",Sha256::digest(&bytes))}})
}
#[test]
fn refuses_remote_urls_corruption_and_oversized_portraits() {
    let mut doc = portrait(100, "uploaded");
    assert!(identity::portrait(&doc).is_ok());
    doc["avatar"]["sha256"] = "wrong".into();
    assert!(identity::portrait(&doc).is_err());
    doc = portrait(100, "uploaded");
    doc["avatar"].as_object_mut().unwrap().remove("data");
    doc["avatar"]["url"] = "http://localhost/private".into();
    assert!(identity::portrait(&doc).is_err());
    doc = portrait(100, "uploaded");
    doc["avatar"]["data"] = "A".repeat(2 * 1024 * 1024).into();
    assert!(identity::portrait(&doc).is_err());
}

#[tokio::test]
async fn syncs_changes_clearing_and_restart_without_sessions_or_public_urls() {
    let count = Arc::new(AtomicUsize::new(0));
    let rotations = Arc::new(AtomicUsize::new(0));
    let rotation_error = Arc::new(AtomicUsize::new(0));
    async fn set_icon(
        State(count): State<Arc<AtomicUsize>>,
        headers: HeaderMap,
        bytes: Bytes,
    ) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer test-rotated-access");
        assert!(bytes.windows(16).any(|s| s == b"agent-avatar.png"));
        assert!(!bytes.windows(9).any(|s| s == b"name=\"url"));
        count.fetch_add(1, Ordering::SeqCst);
        Json(json!({"ok":true}))
    }
    let router = Router::new()
        .route("/tooling.tokens.rotate", post({
            let rotations = rotations.clone();
            let rotation_error = rotation_error.clone();
            move |Form(fields): Form<std::collections::HashMap<String, String>>| {
                let rotations = rotations.clone();
                let rotation_error = rotation_error.clone();
                async move {
                    assert_eq!(fields.len(), 1);
                    let attempt = rotations.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(fields["refresh_token"], if attempt == 0 { "test-refresh+with&special=characters" } else { "test-rotated-refresh" });
                    match rotation_error.load(Ordering::SeqCst) {
                        1 => Json(json!({"ok":false,"error":"invalid_arguments"})),
                        2 => Json(json!({"ok":false,"error":"invalid_refresh_token"})),
                        _ => Json(json!({"ok":true,"token":"test-rotated-access","refresh_token":"test-rotated-refresh","exp":acp_slack_adapter::state::now_ms()/1000+43200,"team_id":"TTEST"})),
                    }
                }
            }
        }))
        .route(
            "/auth.test",
            post(|| async {
                Json(json!({"ok":true,"team_id":"TTEST","user_id":"UTEST","bot_id":"BTEST"}))
            }),
        )
        .route(
            "/bots.info",
            get(|| async { Json(json!({"ok":true,"bot":{"app_id":"ATEST"}})) }),
        )
        .route("/apps.icon.set", post(set_icon))
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(home.join("secrets")).unwrap();
    let peer = home.join("peer.py");
    std::fs::write(&peer,r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 if r['method']=='initialize': result={'protocolVersion':1,'_meta':{'com.microsoft.amplifier.identity':{'schema':'agent-identity/1','method':'_amplifier/identity/get'}}}
 elif r['method']=='_amplifier/identity/get': result=json.load(open(sys.argv[1]))
 else: raise Exception('No sessions or prompts allowed')
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':result}),flush=True)
"#).unwrap();
    let source = home.join("portrait.json");
    std::fs::write(&source, portrait(50, "uploaded").to_string()).unwrap();
    let mut config = Config::new("Portrait test".into(), "tester".into());
    config.acp.command = "/usr/bin/python3".into();
    config.acp.cwd = home.to_string_lossy().into();
    config.acp.args = vec![
        peer.to_string_lossy().into(),
        source.to_string_lossy().into(),
    ];
    config.slack.app_id = "ATEST".into();
    config.slack.bot_id = "BTEST".into();
    config.slack.bot_user_id = "UTEST".into();
    config.slack.team_id = "TTEST".into();
    config.slack.loopback_test = true;
    config.slack.api_base = format!("http://{address}/");
    config.save(home).unwrap();
    private_write(
        &home.join("secrets/slack.json"),
        br#"{"app_token":"test-app","bot_token":"test-bot"}"#,
    )
    .unwrap();
    private_write(
        &home.join("secrets/portrait-configuration.json"),
        br#"{"token":"test-configuration-token","refresh_token":"test-refresh+with&special=characters","expires_at":0}"#,
    )
    .unwrap();
    assert!(
        acp_slack_adapter::driver::identity(&config, home)
            .await
            .expect("identity connection")
            .is_some()
    );
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "synced"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(rotations.load(Ordering::SeqCst), 1);
    let mut secret: Value = serde_json::from_slice(
        &std::fs::read(home.join("secrets/portrait-configuration.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(secret["token"], "test-rotated-access");
    assert_eq!(secret["refresh_token"], "test-rotated-refresh");
    assert!(secret["expires_at"].as_i64().unwrap() > acp_slack_adapter::state::now_ms() / 1000);
    fn allow_retry(home: &std::path::Path) {
        let mut s = identity::status(home);
        s["retry_at"] = 0.into();
        private_write(&home.join("portrait-sync.json"), s.to_string().as_bytes()).unwrap();
    }
    allow_retry(home);
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "synced"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    std::fs::write(&source, portrait(90, "uploaded").to_string()).unwrap();
    allow_retry(home);
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "synced"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    std::fs::write(&source, portrait(10, "generated").to_string()).unwrap();
    allow_retry(home);
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "synced"
    );
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert_eq!(rotations.load(Ordering::SeqCst), 1);
    // An unconfigured source leaves an existing manual Slack icon alone.
    std::fs::remove_file(home.join("portrait-sync.json")).unwrap();
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "waiting_for_image"
    );
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(
        !identity::status(home)
            .to_string()
            .contains("test-configuration-token")
    );
    // Protocol errors are not evidence that the user supplied bad credentials.
    std::fs::write(&source, portrait(120, "uploaded").to_string()).unwrap();
    secret["expires_at"] = 0.into();
    private_write(
        &home.join("secrets/portrait-configuration.json"),
        secret.to_string().as_bytes(),
    )
    .unwrap();
    rotation_error.store(1, Ordering::SeqCst);
    let failure = identity::sync(home, &config).await.unwrap();
    assert_eq!(failure["state"], "needs_attention");
    assert!(
        failure["message"]
            .as_str()
            .unwrap()
            .contains("do not need to re-enter")
    );
    assert!(failure["retry_at"].as_i64().unwrap() > acp_slack_adapter::state::now_ms());
    identity::sync(home, &config).await.unwrap();
    assert_eq!(rotations.load(Ordering::SeqCst), 2); // Back off, including on manual retry.
    allow_retry(home);
    rotation_error.store(2, Ordering::SeqCst);
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "needs_access"
    );
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &std::fs::read(home.join("secrets/portrait-configuration.json")).unwrap()
        )
        .unwrap(),
        secret
    );
    allow_retry(home);
    let old = std::fs::read_to_string(&peer).unwrap();
    std::fs::write(
        &peer,
        old.replace("com.microsoft.amplifier.identity", "example.unsupported"),
    )
    .unwrap();
    assert_eq!(
        identity::sync(home, &config).await.unwrap()["state"],
        "unsupported"
    );
    assert_eq!(count.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn observation_deduplicates_and_legacy_peers_are_closed_after_one_snapshot() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let router = Router::new()
        .route(
            "/auth.test",
            post(|| async {
                Json(json!({"ok":true,"team_id":"TTEST","user_id":"UTEST","bot_id":"BTEST"}))
            }),
        )
        .route(
            "/bots.info",
            get(|| async { Json(json!({"ok":true,"bot":{"app_id":"ATEST"}})) }),
        )
        .route(
            "/apps.icon.set",
            post(move || {
                let count = requests.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"ok":true}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(home.join("secrets")).unwrap();
    let source = home.join("source.json");
    std::fs::write(&source, portrait(40, "uploaded").to_string()).unwrap();
    let peer = home.join("peer.py");
    let starts = home.join("starts");
    std::fs::write(&peer,r#"import sys,json,time
with open(sys.argv[2],'a') as f: f.write('start\n')
ns='com.microsoft.amplifier.identity'
for line in sys.stdin:
 r=json.loads(line)
 if r['method']=='initialize':
  assert r['params']['_meta'][ns]['mode']=='observe'
  cap={'schema':'agent-identity/1','method':'_amplifier/identity/get'}
  if sys.argv[3]=='events': cap.update({'observation_only':True,'observation':{'subscribe':'_amplifier/identity/subscribe','changed':'_amplifier/identity/changed'}})
  result={'protocolVersion':1,'_meta':{ns:cap}}
 elif r['method'] in ('_amplifier/identity/get','_amplifier/identity/subscribe'): result=json.load(open(sys.argv[1]))
 else: raise Exception('No session or model work allowed')
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':result}),flush=True)
 if r['method']=='_amplifier/identity/subscribe':
  # Repeated equivalent notifications never cause duplicate Slack uploads.
  for n in range(5):
   print(json.dumps({'jsonrpc':'2.0','method':'_amplifier/identity/changed','params':result}),flush=True)
   time.sleep(.03)
"#).unwrap();
    let mut c = Config::new("Observer".into(), "tester".into());
    c.acp.command = "/usr/bin/python3".into();
    c.acp.cwd = home.to_string_lossy().into();
    c.acp.args = vec![
        peer.to_string_lossy().into(),
        source.to_string_lossy().into(),
        starts.to_string_lossy().into(),
        "events".into(),
    ];
    c.slack.app_id = "ATEST".into();
    c.slack.bot_id = "BTEST".into();
    c.slack.bot_user_id = "UTEST".into();
    c.slack.team_id = "TTEST".into();
    c.slack.loopback_test = true;
    c.slack.api_base = format!("http://{addr}/");
    c.save(home).unwrap();
    private_write(
        &home.join("secrets/slack.json"),
        br#"{"app_token":"test-app","bot_token":"test-bot"}"#,
    )
    .unwrap();
    private_write(
        &home.join("secrets/portrait-configuration.json"),
        br#"{"token":"test-configuration-token","expires_at":0}"#,
    )
    .unwrap();
    let run = tokio::spawn(identity::run(home.to_owned(), c.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while count.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(&starts).unwrap().lines().count(), 1);
    assert_eq!(identity::status(home)["update_mode"], "events");
    assert!(identity::status(home).get("poll_seconds").is_none());
    run.abort();
    let _ = run.await;
    // Restart takes a snapshot and retains the upload checksum.
    c.acp.args[3] = "legacy".into();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        identity::run(home.to_owned(), c),
    )
    .await
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read_to_string(&starts).unwrap().lines().count(), 2);
    assert_eq!(identity::status(home)["update_mode"], "manual");
    server.abort();
}
