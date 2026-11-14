use acp_slack_adapter::{
    config::{Attention, Config},
    model::{Decision, Message},
    server,
    state::AppState,
    store::Store,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
fn message(ts: &str) -> Message {
    Message {
        team: "T".into(),
        channel: "C".into(),
        thread: "root".into(),
        ts: ts.into(),
        revision: "1".into(),
        sender: "U".into(),
        bot: false,
        text: "PRIVATE SOURCE".into(),
        deleted: false,
        files: vec![],
    }
}
async fn inspect(state: Arc<AppState>, token: Option<&str>, source: Value) -> (StatusCode, Value) {
    let mut r = Request::builder()
        .method("POST")
        .uri("/api/v1/submissions/inspect")
        .header("host", "localhost")
        .header("content-type", "application/json");
    if let Some(t) = token {
        r = r.header("authorization", format!("Bearer {t}"));
    }
    let response = server::router(state)
        .oneshot(
            r.body(Body::from(json!({"sources":[source]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn source(m: &Message) -> Value {
    json!({"team":m.team,"channel":m.channel,"thread":m.thread,"ts":m.ts,"revision":m.revision,"sender":m.sender})
}
#[tokio::test]
async fn inspect_original_submission_is_exact_authorized_and_not_a_recent_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Config::new("Test".into(), "tester".into());
    c.slack.team_id = "T".into();
    c.slack.channels = vec!["C".into()];
    c.save(dir.path()).unwrap();
    let state = Arc::new(AppState::open(dir.path().into()).unwrap());
    let token = std::fs::read_to_string(dir.path().join("secrets/control-token")).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let m = message("1.001");
    let a = Attention {
        settle_ms: 0,
        minimum_reply_interval_seconds: 0,
        ..Attention::default()
    };
    store.ingest("original", &m, true).unwrap();
    let job = store.claim(&a).unwrap().unwrap();
    store
        .finish(
            &job,
            Ok(Decision {
                decision: "speak".into(),
                text: "PRIVATE ANSWER".into(),
                reason: "".into(),
                media: vec![],
            }),
            &a,
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE outbox SET status='uncertain' WHERE job_id=?1",
            [&job.id],
        )
        .unwrap();
    for n in 2..20 {
        let next = message(&format!("{n}.001"));
        store.ingest(&format!("event-{n}"), &next, true).unwrap();
        let j = store.claim(&a).unwrap().unwrap();
        store
            .finish(
                &j,
                Ok(Decision {
                    decision: "pass".into(),
                    text: "".into(),
                    reason: "".into(),
                    media: vec![],
                }),
                &a,
            )
            .unwrap();
    }
    let before: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        inspect(state.clone(), None, source(&m)).await.0,
        StatusCode::UNAUTHORIZED
    );
    let ui = acp_slack_adapter::auth::issue_token(&state, "ui-only", &["ui".into()], 300).unwrap();
    assert_eq!(
        inspect(state.clone(), ui["token"].as_str(), source(&m))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let observer =
        acp_slack_adapter::auth::issue_token(&state, "observer", &["observe".into()], 300).unwrap();
    assert_eq!(
        inspect(state.clone(), observer["token"].as_str(), source(&m))
            .await
            .0,
        StatusCode::OK
    );
    let (status, v) = inspect(state.clone(), Some(&token), source(&m)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["items"][0]["attempts"][0]["job_id"], job.id);
    assert_eq!(
        v["items"][0]["attempts"][0]["publication"]["state"],
        "uncertain"
    );
    assert_eq!(v["items"][0]["completion"], "not_established");
    assert!(!v.to_string().contains("PRIVATE"));
    for field in ["sender", "channel", "thread", "revision", "team"] {
        let mut wrong = source(&m);
        wrong[field] = json!("different");
        let (_, v) = inspect(state.clone(), Some(&token), wrong).await;
        assert_eq!(v["items"][0]["state"], "unknown");
        assert_eq!(v["items"][0]["attempts"], json!([]));
    }
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn negotiated_ordinary_source_receipts_preserve_multiple_work_refs_without_replaying() {
    for mode in ["supported", "plain", "lost"] {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let script = dir.path().join("source.py");
        std::fs::write(&script,r#"import json,sys,pathlib
root=pathlib.Path(sys.argv[1]); mode=sys.argv[2]
metadata=None
def out(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r.get('id')
 if m=='initialize':
  result={'protocolVersion':1,'agentCapabilities':{}}
  if mode!='plain':result['_meta']={'io.github.bkrabach.agent-instance':{'inputContinuity':{'sourceDelivery':{'version':1,'metadata':'com.microsoft.amplifier.source-delivery','ordinaryPrompts':True,'workReferences':True,'receiptsMethod':'_amplifier/source_receipts'}}}}
 elif m=='session/new':result={'sessionId':'source-session'}
 elif m=='session/prompt':
  metadata=p.get('_meta',{}).get('com.microsoft.amplifier.source-delivery')
  assert (metadata is not None)==(mode!='plain')
  if metadata:assert metadata['inputId'] and metadata['sources'][0]['revision']=='1'
  with (root/'prompts').open('a') as f:f.write('1\n')
  if mode=='lost':sys.exit(0)
  out({'method':'session/update','params':{'sessionId':'source-session','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'Done inspecting.'}}}})
  result={'stopReason':'end_turn'}
 elif m=='_amplifier/source_receipts':
  assert mode=='supported';assert p['sources']==metadata['sources'];s=p['sources'][0]
  result={'version':1,'coverage':{'delivery':'recent_session_records','work':'retained_session_records','missing':'unknown'},'receipts':[{'sourceId':s['id'],'revision':s['revision'],'occurredAt':s['occurredAt'],'inputId':metadata['inputId'],'state':'incorporated','incorporated':True,'response':{'kind':'result','stopReason':'end_turn'},'completion':'not_established','work':[{'id':'work-one','state':'accepted'},{'id':'work-two','state':'proposed'}],'workCoverage':'retained_session_records'}]}
 else:result={}
 if i is not None:out({'id':i,'result':result})
"#).unwrap();
        let mut c = Config::new("Test".into(), "tester".into());
        c.context.enabled = false;
        c.slack.team_id = "T".into();
        c.slack.channels = vec!["Dwork".into()];
        c.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
        c.acp.args = vec![
            script.to_string_lossy().into(),
            dir.path().to_string_lossy().into(),
            mode.into(),
        ];
        c.acp.cwd = dir.path().to_string_lossy().into();
        let mut store = Store::open(&home).unwrap();
        let mut m = message(&format!(
            "{}.001",
            acp_slack_adapter::model::now_ms() / 1000
        ));
        m.channel = "Dwork".into();
        let a = Attention {
            settle_ms: 0,
            ..Attention::default()
        };
        store.ingest("event", &m, true).unwrap();
        let job = store.claim(&a).unwrap().unwrap();
        let mut input = acp_slack_adapter::driver::request(&c, &m, vec![m.clone()]);
        input["job_id"] = json!(job.id);
        let result = acp_slack_adapter::driver::decide(c.clone(), home, input).await;
        assert_eq!(result.is_ok(), mode != "lost");
        let projection = acp_slack_adapter::submissions::inspect(
            &store,
            &c,
            &serde_json::from_value(json!({"sources":[source(&m)]})).unwrap(),
        )
        .unwrap();
        let instance = &projection["items"][0]["attempts"][0]["instance"];
        assert_eq!(
            instance["state"],
            if mode == "supported" {
                "observed"
            } else if mode == "plain" {
                "unsupported"
            } else {
                "unknown"
            }
        );
        if mode == "supported" {
            assert_eq!(instance["work"].as_array().unwrap().len(), 2);
            assert_eq!(instance["receipt"]["completion"], "not_established");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompts")).unwrap(),
            "1\n"
        );
    }
}
