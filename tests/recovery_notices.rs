//! Connector diagnostics are human UI, never another agent's work request.
use acp_slack_adapter::{
    config::{Config, Secrets},
    model::Message,
    slack::{self, Slack},
    steering,
    store::{Job, Store},
    work_status,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

async fn api(
    State(calls): State<Calls>,
    Path(method): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut calls = calls.lock().unwrap();
    calls.push((method, body.clone()));
    Json(
        json!({"ok":true,"ts":body.get("ts").cloned().unwrap_or(json!(format!("100.{}",calls.len())))}),
    )
}

async fn fixture() -> (Config, Slack, Calls, tokio::task::JoinHandle<()>) {
    let calls = Calls::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut c = Config::new("Fixture".into(), "owner".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.attention.settle_ms = 0;
    c.attention.minimum_reply_interval_seconds = 0;
    c.slack.team_id = "Ttest".into();
    c.slack.bot_user_id = "Uself".into();
    c.slack.bot_id = "Bself".into();
    c.slack.channels = vec!["Croom".into()];
    c.slack.loopback_test = true;
    c.slack.api_base = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/{method}", post(api))
        .with_state(calls.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let slack = Slack::new(
        &c,
        Secrets {
            app_token: "test-app".into(),
            bot_token: "test-bot".into(),
        },
    )
    .unwrap();
    (c, slack, calls, task)
}

fn start(s: &mut Store, c: &Config, ts: &str) -> Job {
    let trigger: Message = serde_json::from_value(json!({"team":"Ttest","channel":"Croom","thread":"","ts":ts,"revision":ts,"sender":"Uhuman","bot":false,"text":"Please try the work","deleted":false})).unwrap();
    s.ingest(ts, &trigger, true).unwrap();
    s.claim(&c.attention).unwrap().unwrap()
}

#[tokio::test]
async fn failure_notice_is_delivered_once_and_cannot_trigger_peer_jobs_or_awareness() {
    let (c, slack, calls, task) = fixture().await;
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let job = start(&mut s, &c, "1.0");
    steering::recovery_required(&s, &job.id).unwrap();
    steering::recovery_required(&s, &job.id).unwrap();
    let outgoing = s.claim_outgoing(&c).unwrap().unwrap();
    assert!(s.claim_outgoing(&c).unwrap().is_none());
    let source = Arc::new(Mutex::new(s));
    let ts = slack.publish_durable(&outgoing, &source).await.unwrap();
    let body = calls.lock().unwrap()[0].1.clone();
    assert_eq!(body["metadata"]["event_type"], "acp_connector_notice");
    assert_eq!(body["client_msg_id"], outgoing.id);

    let notice = json!({"type":"message","channel":"Croom","user":"Uself","bot_id":"Bself","ts":ts,"text":body["markdown_text"],"metadata":body["metadata"],"client_msg_id":outgoing.id});
    for busy in [false, true] {
        let peer_home = tempfile::tempdir().unwrap();
        let mut peer = Store::open(peer_home.path()).unwrap();
        let mut peer_c = c.clone();
        peer_c.slack.bot_user_id = "Upeer".into();
        peer_c.slack.bot_id = "Bpeer".into();
        if busy {
            start(&mut peer, &peer_c, "2.0");
        }
        slack::receive(
            &peer_c,
            &mut peer,
            &json!({"event_id":"failure","event":notice}),
        )
        .unwrap();
        assert!(peer.message("Croom", &ts).unwrap().is_none());
        let jobs: i64 = peer
            .db
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jobs, i64::from(busy));
        let awareness: i64 = peer
            .db
            .query_row("SELECT COUNT(*) FROM steering_inputs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(awareness, 0);
    }

    // Reconcile our own echo before excluding it from conversation history.
    let mut source = source.lock().unwrap();
    let mut own_echo = notice;
    own_echo.as_object_mut().unwrap().remove("metadata");
    slack::receive(
        &c,
        &mut source,
        &json!({"event_id":"own-echo","event":own_echo}),
    )
    .unwrap();
    let state: String = source
        .db
        .query_row("SELECT status FROM outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "sent");
    assert!(source.message("Croom", &ts).unwrap().is_none());
    assert!(slack::known_control(&source, "Croom", &ts).unwrap());
    task.abort();
}

#[test]
fn upgrade_classifies_prior_owned_notices_without_matching_visible_text() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Config::new("fixture".into(), "owner".into());
    c.attention.settle_ms = 0;
    let mut s = Store::open(home.path()).unwrap();
    let job = start(&mut s, &c, "1.0");
    steering::recovery_required(&s, &job.id).unwrap();
    s.db.execute("UPDATE outbox SET slack_ts='100.1',status='sent'", [])
        .unwrap();
    s.db.execute("ALTER TABLE outbox DROP COLUMN connector_notice", [])
        .unwrap();
    drop(s);
    let s = Store::open(home.path()).unwrap();
    assert!(slack::known_control(&s, "Croom", "100.1").unwrap());
    let text: String =
        s.db.query_row("SELECT text FROM outbox", [], |r| r.get(0))
            .unwrap();
    let human =
        json!({"type":"message","channel":"Croom","user":"Uhuman","ts":"101.0","text":text});
    assert!(slack::normalize("Ttest", "Croom", &human, None).is_some());
}

#[tokio::test]
async fn detailed_failure_notice_replaces_the_work_indicator_without_a_second_error() {
    let (c, slack, calls, task) = fixture().await;
    let home = tempfile::tempdir().unwrap();
    c.save(home.path()).unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let job = start(&mut s, &c, "1.0");
    work_status::track(home.path(), &job).unwrap();
    work_status::fallback_reconcile(home.path(), &slack, false)
        .await
        .unwrap();
    steering::recovery_required(&s, &job.id).unwrap();
    s.db.execute("UPDATE response_indicators SET retry_at=0", [])
        .unwrap();
    work_status::fallback_reconcile(home.path(), &slack, false)
        .await
        .unwrap();
    let out = s.claim_outgoing(&c).unwrap().unwrap();
    let source = Arc::new(Mutex::new(s));
    let ts = slack.publish_durable(&out, &source).await.unwrap();
    {
        let s = source.lock().unwrap();
        s.delivery(&out.id, "sent", Some(&ts), "fixture", 0)
            .unwrap();
        s.db.execute("UPDATE response_indicators SET retry_at=0", [])
            .unwrap();
    }
    work_status::fallback_reconcile(home.path(), &slack, false)
        .await
        .unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(method, body)| method == "chat.postMessage"
                && body["metadata"]["event_type"] == "acp_connector_notice")
            .count(),
        1
    );
    assert!(
        !calls
            .iter()
            .any(|(_, body)| body.to_string().contains("couldn’t confirm")),
        "{calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| method == "chat.delete")
            .count(),
        1
    );
    task.abort();
}
