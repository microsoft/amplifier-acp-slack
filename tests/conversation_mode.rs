use acp_slack_adapter::{
    auth,
    config::Config,
    model::{Message, now_ms},
    server,
    state::AppState,
    store::Store,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

struct Fixture {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
    context: String,
    sibling: String,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let home = root.join("adapter");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let script = root.join("agent.py");
        std::fs::write(&script,r#"import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); saved=root/'modes.json'
def send(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r.get('id')
 if not m: continue
 with (root/'calls.jsonl').open('a') as f:f.write(json.dumps(r)+'\n')
 if m=='initialize':send({'id':i,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True}}})
 elif m=='session/load':
  if (root/'unsupported').exists():send({'id':i,'result':{}});continue
  send({'id':'forbidden-write','method':'fs/write_text_file','params':{'sessionId':p['sessionId'],'path':str(root/'outside'),'content':'must not write'}})
  reply=json.loads(sys.stdin.readline());assert 'error' in reply
  modes=json.loads(saved.read_text()) if saved.exists() else {}
  send({'id':i,'result':{'modes':{'currentModeId':modes.get(p['sessionId'],'ask'),'availableModes':[{'id':'ask','name':'Ask'},{'id':'workspace','name':'Workspace','description':'Local workspace work'}]}}})
 elif m=='session/set_mode':
  modes=json.loads(saved.read_text()) if saved.exists() else {};modes[p['sessionId']]=p['modeId'];saved.write_text(json.dumps(modes));send({'id':i,'result':{}})
 else:send({'id':i,'error':{'code':-32601,'message':'not supported'}})
"#).unwrap();
        let mut c = Config::new("Mode fixture".into(), "owner".into());
        c.acp.command = "python3".into();
        c.acp.args = vec![
            script.to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
        ];
        c.acp.cwd = workspace.to_string_lossy().into_owned();
        c.slack.team_id = "T1".into();
        c.slack.app_id = "A1".into();
        c.slack.bot_user_id = "UBOT".into();
        c.slack.bot_id = "B1".into();
        c.save(&home).unwrap();
        let state = Arc::new(AppState::open(home.clone()).unwrap());
        let store = Store::open(&home).unwrap();
        let mut contexts = vec![];
        for (channel, sid) in [("DONE", "session-one"), ("DTWO", "session-two")] {
            let m = Message {
                team: "T1".into(),
                channel: channel.into(),
                thread: "".into(),
                ts: "123.4".into(),
                revision: "123.4".into(),
                sender: "UOWNER".into(),
                bot: false,
                text: "An existing message".into(),
                deleted: false,
                files: vec![],
            };
            let context = m.context();
            store
                .db
                .execute("INSERT INTO contexts(id) VALUES(?1)", [&context])
                .unwrap();
            store
                .db
                .execute(
                    "INSERT INTO messages VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![
                        channel,
                        m.ts,
                        context,
                        m.revision,
                        serde_json::to_string(&m).unwrap()
                    ],
                )
                .unwrap();
            store
                .bind_session(
                    &context,
                    &auth::digest(&json!({"command":c.acp.command,"args":c.acp.args,"cwd":c.acp.cwd,"env":c.acp.env,"secret_env":c.acp.secret_env}).to_string()),
                    sid,
                    true,
                    &json!({"channel":channel,"thread":""}),
                )
                .unwrap();
            contexts.push(context);
        }
        Self {
            _temp: temp,
            state,
            context: contexts.remove(0),
            sibling: contexts.remove(0),
            root,
        }
    }
    async fn call(
        &self,
        method: &str,
        context: &str,
        body: Value,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/v1/conversations/{context}/mode"))
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = server::router(self.state.clone())
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    fn token(&self) -> String {
        std::fs::read_to_string(self.state.home.join("secrets/control-token"))
            .unwrap()
            .trim()
            .into()
    }
}

#[tokio::test]
async fn owner_changes_only_the_selected_saved_session_without_a_model_prompt() {
    let f = Fixture::new();
    let token = f.token();
    let (status, current) = f.call("GET", &f.context, json!(null), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(current["current_mode_id"], "ask");
    assert_eq!(current["available_modes"].as_array().unwrap().len(), 2);
    let (status, result) = f
        .call(
            "POST",
            &f.context,
            json!({"mode_id":"workspace","expected_session_id":"session-one"}),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["current_mode_id"], "workspace");
    assert_eq!(
        f.call("GET", &f.sibling, json!(null), Some(&token)).await.1["current_mode_id"],
        "ask"
    );
    let calls = std::fs::read_to_string(f.root.join("calls.jsonl")).unwrap();
    assert!(!calls.contains("session/prompt"));
    assert!(!calls.contains("session/new"));
    assert!(!f.root.join("outside").exists());
    let saved: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("modes.json")).unwrap()).unwrap();
    assert_eq!(saved, json!({"session-one":"workspace"}));
}

#[tokio::test]
async fn unadvertised_stale_busy_and_unauthorized_changes_fail_before_setting_mode() {
    let f = Fixture::new();
    let token = f.token();
    assert_eq!(
        f.call("GET", &f.context, json!(null), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let observer = auth::issue_token(&f.state, "observer", &["observe".into()], 300).unwrap();
    assert_eq!(
        f.call("GET", &f.context, json!(null), observer["token"].as_str())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.call(
            "POST",
            &f.context,
            json!({"mode_id":"unrestricted","expected_session_id":"session-one"}),
            Some(&token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.call(
            "POST",
            &f.context,
            json!({"mode_id":"workspace","expected_session_id":"old-session"}),
            Some(&token)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            f.state
                .home
                .join("conversation-locks")
                .join(auth::digest(&f.context)),
        )
        .unwrap();
    // Endpoint teardown is asynchronous; a fork from another parallel test
    // can briefly retain this descriptor until exec/exit. Acquire our synthetic
    // busy guard before testing that the management request is rejected.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => panic!("Could not acquire test lock: {error}"),
            }
        }
    })
    .await
    .expect("Previous mode request did not release its lock");
    assert_eq!(
        f.call("GET", &f.context, json!(null), Some(&token)).await.0,
        StatusCode::CONFLICT
    );
    drop(lock);
    f.state.db.lock().unwrap().execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES('busy',?1,'{}','running',0,0)",[&f.context]).unwrap();
    assert_eq!(
        f.call("GET", &f.context, json!(null), Some(&token)).await.0,
        StatusCode::CONFLICT
    );
    assert!(!f.root.join("modes.json").exists());
}

#[tokio::test]
async fn unsupported_modes_and_missing_csrf_do_not_mutate() {
    let f = Fixture::new();
    let token = f.token();
    std::fs::write(f.root.join("unsupported"), "").unwrap();
    let (status, result) = f.call("GET", &f.context, json!(null), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["supported"], false);
    assert_eq!(
        f.call(
            "POST",
            &f.context,
            json!({"mode_id":"workspace","expected_session_id":"session-one"}),
            Some(&token)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let cookie = "test-session";
    f.state
        .db
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO browser_sessions VALUES(?1,'owner','csrf-required',?2)",
            rusqlite::params![auth::digest(cookie), now_ms() + 60000],
        )
        .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/conversations/{}/mode", f.context))
        .header("content-type", "application/json")
        .header(
            "cookie",
            format!(
                "asa_{}={cookie}",
                f.state.instance_config().id.replace('-', "")
            ),
        )
        .body(Body::from(
            json!({"mode_id":"workspace","expected_session_id":"session-one"}).to_string(),
        ))
        .unwrap();
    assert_eq!(
        server::router(f.state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(!f.root.join("modes.json").exists());
}
