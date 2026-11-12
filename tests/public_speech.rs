use acp_slack_adapter::{
    config::Config,
    model::{Decision, Message},
    public_speech as speech,
    store::{Job, Store},
};
use amplifier_acp_connection::speech::Utterance;
use serde_json::json;

fn setup() -> (tempfile::TempDir, Store, Config, Job) {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let mut c = Config::new("Fixture".into(), "owner".into());
    c.enabled = true;
    c.attention.settle_ms = 0;
    c.attention.minimum_reply_interval_seconds = 0;
    c.attention.max_replies_per_agent_per_hour = 1;
    let m = Message {
        team: "Ttest".into(),
        channel: "Dtest".into(),
        thread: "".into(),
        ts: "1.000001".into(),
        revision: "1.000001".into(),
        sender: "Uhuman".into(),
        bot: false,
        text: "Explain your work".into(),
        deleted: false,
        files: vec![],
    };
    s.ingest("input", &m, true).unwrap();
    let j = s.claim(&c.attention).unwrap().unwrap();
    (home, s, c, j)
}
fn say(s: &Store, j: &Job, id: &str, text: &str) {
    speech::enqueue(
        s,
        &j.id,
        &Utterance {
            id: id.into(),
            text: text.into(),
        },
    )
    .unwrap();
}
fn finish(s: &mut Store, c: &Config, j: &Job) {
    s.finish(
        j,
        Ok(Decision {
            decision: "speak".into(),
            text: "Final answer.".into(),
            reason: "".into(),
            media: vec![],
        }),
        &c.attention,
    )
    .unwrap();
}
#[test]
fn separate_utterances_and_final_share_one_reply_budget_in_order() {
    let (_h, mut s, c, j) = setup();
    say(&s, &j, "one", "I found the cause.");
    say(&s, &j, "two", "I am checking the fix.");
    finish(&mut s, &c, &j);
    assert!(s.claim_outgoing(&c).unwrap().is_none());
    let first = speech::claim(&mut s, &c).unwrap().unwrap();
    assert_eq!(first.text, "I found the cause.");
    speech::settle(&s, &first.id, "sent", Some("2.0"), "ok", 0).unwrap();
    let second = speech::claim(&mut s, &c).unwrap().unwrap();
    assert_ne!(first.id, second.id);
    speech::settle(&s, &second.id, "sent", Some("3.0"), "ok", 0).unwrap();
    let final_reply = s.claim_outgoing(&c).unwrap().unwrap();
    assert_eq!(final_reply.text, "Final answer.");
    assert_eq!(
        s.db.query_row("SELECT bot_replies FROM contexts", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM reply_activity", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn lost_reply_and_restart_do_not_replay_or_overwrite_an_echo() {
    let (h, mut s, c, j) = setup();
    say(&s, &j, "one", "First permanent message.");
    let o = speech::claim(&mut s, &c).unwrap().unwrap();
    drop(s);
    let mut s = Store::open(h.path()).unwrap();
    s.recover().unwrap();
    assert!(speech::claim(&mut s, &c).unwrap().is_none());
    s.echo(&o.id, "2.0").unwrap();
    speech::settle(&s, &o.id, "uncertain", None, "lost", 0).unwrap();
    assert_eq!(
        s.db.query_row("SELECT status FROM public_speech", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "sent"
    );
    say(&s, &j, "one", "First permanent message.");
    assert!(speech::claim(&mut s, &c).unwrap().is_none());
    assert!(
        speech::enqueue(
            &s,
            &j.id,
            &Utterance {
                id: "one".into(),
                text: "Rewritten history".into()
            }
        )
        .is_err()
    );
}
#[test]
fn human_stop_withholds_unsent_speech_without_modifying_delivered_text() {
    let (_h, mut s, c, j) = setup();
    say(&s, &j, "one", "Completed finding.");
    let o = speech::claim(&mut s, &c).unwrap().unwrap();
    speech::settle(&s, &o.id, "sent", Some("2.0"), "ok", 0).unwrap();
    say(&s, &j, "two", "Another finding.");
    s.db.execute(
        "UPDATE contexts SET control_version=(SELECT MAX(version)+1 FROM jobs)",
        [],
    )
    .unwrap();
    assert!(speech::claim(&mut s, &c).unwrap().is_none());
    assert_eq!(
        s.db.query_row(
            "SELECT status FROM public_speech WHERE id=?1",
            [&o.id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "sent"
    );
    assert_eq!(
        s.db.query_row(
            "SELECT COUNT(*) FROM public_speech WHERE status='cancelled'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}
#[test]
fn rate_limit_retries_only_the_same_unsent_part_and_never_charges_twice() {
    let (_h, mut s, c, j) = setup();
    say(&s, &j, "one", "A message.");
    let o = speech::claim(&mut s, &c).unwrap().unwrap();
    speech::settle(&s, &o.id, "ready", None, "rate limit", 0).unwrap();
    let retry = speech::claim(&mut s, &c).unwrap().unwrap();
    assert_eq!(retry.id, o.id);
    assert_eq!(
        s.db.query_row("SELECT bot_replies FROM contexts", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn source_edit_and_connection_scope_are_checked_at_delivery() {
    let (_h, mut s, mut c, j) = setup();
    say(&s, &j, "one", "Not sent yet.");
    c.enabled = false;
    assert!(speech::claim(&mut s, &c).unwrap().is_none());
    c.enabled = true;
    say(&s, &j, "two", "Also not sent.");
    s.db.execute(
        "UPDATE messages SET body=json_set(body,'$.revision','edited')",
        [],
    )
    .unwrap();
    assert!(speech::claim(&mut s, &c).unwrap().is_none());
}
#[test]
fn drafts_private_thoughts_and_final_control_json_are_not_public_speech() {
    let (_h, s, _c, j) = setup();
    let mut collector = amplifier_acp_connection::speech::Commentary::default();
    for u in [
        json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"SECRET"}}),
        json!({"sessionUpdate":"agent_message_chunk","messageId":"final","content":{"type":"text","text":"{\"decision\":\"pass\"}"},"_meta":{"com.microsoft.amplifier.message":{"phase":"final_answer"}}}),
    ] {
        assert!(collector.observe(&u, true).unwrap().is_none());
    }
    assert!(collector.terminal(true).is_empty());
    let draft = json!({"sessionUpdate":"agent_message_chunk","messageId":"draft","content":{"type":"text","text":"Not complete"},"_meta":{"com.microsoft.amplifier.message":{"phase":"commentary"}}});
    assert!(collector.observe(&draft, false).unwrap().is_none());
    assert!(collector.terminal(false).is_empty());
    assert_eq!(
        s.db.query_row(
            "SELECT COUNT(*) FROM public_speech WHERE job_id=?1",
            [j.id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn actual_sdk_client_posts_only_settled_speech_before_prompt_terminal() {
    for mode in ["negotiated", "plain", "cancelled"] {
        let (home, s, mut c, j) = setup();
        let script = home.path().join("agent.py");
        std::fs::write(&script,r#"import sys,json,hashlib,pathlib,time
root=pathlib.Path(sys.argv[1]); mode=sys.argv[2]; key='com.microsoft.amplifier.message';sys.stderr=open(root/'error','w')
def send(v): print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
def update(v):send({'method':'session/update','params':{'sessionId':'session','update':v}})
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');i=r.get('id')
 if m=='initialize':
  assert r['params']['clientCapabilities']['_meta'][key]['completion']=={'version':1}
  result={'protocolVersion':1,'agentCapabilities':{}}
  if mode!='plain':result['_meta']={key:{'completion':{'version':1}}}
 elif m=='session/new': result={'sessionId':'session'}
 elif m=='session/prompt':
  text='First complete thought.'
  update({'sessionUpdate':'agent_thought_chunk','content':{'type':'text','text':'PRIVATE_THOUGHT_CANARY'}})
  update({'sessionUpdate':'agent_message_chunk','messageId':'first','content':{'type':'text','text':text},'_meta':{key:{'phase':'commentary'}}})
  marker={'sessionUpdate':'agent_message_chunk','messageId':'first','content':{'type':'text','text':''},'_meta':{key:{'completed':{'version':1,'utf8Bytes':len(text.encode()),'sha256':hashlib.sha256(text.encode()).hexdigest()}}}}
  if mode=='negotiated':update(marker);update(marker)
  (root/'paused').write_text('1')
  end=time.monotonic()+10
  while not (root/'continue').exists() and time.monotonic()<end:time.sleep(.01)
  if mode!='cancelled':update({'sessionUpdate':'agent_message_chunk','messageId':'final','content':{'type':'text','text':'Final answer.'},'_meta':{key:{'phase':'final_answer'}}})
  result={'stopReason':'cancelled' if mode=='cancelled' else 'end_turn'}
 else:result={}
 if i is not None:send({'id':i,'result':result})
"#).unwrap();
        c.context.enabled = false;
        c.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
        c.acp.cwd = home.path().to_string_lossy().into();
        c.acp.args = vec![
            script.to_string_lossy().into(),
            home.path().to_string_lossy().into(),
            mode.into(),
        ];
        c.save(home.path()).unwrap();
        let mut input = acp_slack_adapter::driver::request(&c, &j.trigger, vec![j.trigger.clone()]);
        input["job_id"] = j.id.clone().into();
        let path = home.path().to_path_buf();
        let running = tokio::spawn(acp_slack_adapter::driver::decide(c, path, input));
        let waited = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while !home.path().join("paused").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if mode == "negotiated" {
                loop {
                    let count: i64 =
                        s.db.query_row("SELECT COUNT(*) FROM public_speech", [], |r| r.get(0))
                            .unwrap();
                    if count == 1 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        })
        .await;
        if waited.is_err() {
            std::fs::write(home.path().join("continue"), "1").unwrap();
            panic!(
                "{mode}: no expected mid-turn event; paused={}, error={:?}, result={:?}",
                home.path().join("paused").exists(),
                std::fs::read_to_string(home.path().join("error")),
                running.await
            );
        }
        assert!(!running.is_finished());
        let before: i64 =
            s.db.query_row("SELECT COUNT(*) FROM public_speech", [], |r| r.get(0))
                .unwrap();
        assert_eq!(before, if mode == "negotiated" { 1 } else { 0 });
        std::fs::write(home.path().join("continue"), "1").unwrap();
        let answer = running.await.unwrap().unwrap();
        if mode != "cancelled" {
            assert_eq!(answer.text, "Final answer.");
        }
        let texts =
            s.db.prepare("SELECT text FROM public_speech")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
        assert_eq!(
            texts,
            if mode == "cancelled" {
                vec![]
            } else {
                vec!["First complete thought.".to_string()]
            }
        );
    }
}

#[tokio::test]
async fn slack_delivery_appends_spoken_messages_and_allows_other_people_between_them() {
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::{Arc, Mutex};
    async fn post_message(
        State(calls): State<Arc<Mutex<Vec<serde_json::Value>>>>,
        Json(value): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let mut calls = calls.lock().unwrap();
        calls.push(value);
        Json(json!({"ok":true,"ts":format!("{}.0001",calls.len())}))
    }
    let (home, s, mut c, j) = setup();
    let calls = Arc::new(Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    c.slack.loopback_test = true;
    c.slack.api_base = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/chat.postMessage", post(post_message))
        .with_state(calls.clone());
    let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let slack = acp_slack_adapter::slack::Slack::new(
        &c,
        acp_slack_adapter::config::Secrets {
            app_token: "fixture".into(),
            bot_token: "fixture".into(),
        },
    )
    .unwrap();
    say(&s, &j, "first", "I found the cause.");
    let store = Arc::new(Mutex::new(s));
    assert!(speech::pump(home.path(), &slack, &store, &c).await.unwrap());
    calls
        .lock()
        .unwrap()
        .push(json!({"text":"Please also check the second case.","fixture_actor":"human"}));
    say(
        &store.lock().unwrap(),
        &j,
        "second",
        "I checked the second case too.",
    );
    assert!(speech::pump(home.path(), &slack, &store, &c).await.unwrap());
    let messages = calls.lock().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["fixture_actor"], "human");
    assert_ne!(messages[0]["client_msg_id"], messages[2]["client_msg_id"]);
    assert!(
        messages[0]["markdown_text"]
            .as_str()
            .unwrap()
            .contains("I found the cause.")
    );
    assert!(
        messages[2]["markdown_text"]
            .as_str()
            .unwrap()
            .contains("I checked the second case too.")
    );
    let s = store.lock().unwrap();
    assert_eq!(
        s.db.query_row(
            "SELECT COUNT(*) FROM public_speech WHERE status='sent'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    server.abort();
}
