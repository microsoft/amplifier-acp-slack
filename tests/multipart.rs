use acp_slack_adapter::{
    config::Attention,
    config::{Config, Secrets},
    model::{Decision, Message},
    slack::{ApiError, Slack},
    store::{Outgoing, Store},
};
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fixture {
    calls: Vec<Value>,
    fail: usize,
    uncertain: bool,
    delete_source: Option<Arc<Mutex<Store>>>,
}
async fn message(State(state): State<Arc<Mutex<Fixture>>>, Json(body): Json<Value>) -> Json<Value> {
    let mut s = state.lock().unwrap();
    s.calls.push(body);
    let i = s.calls.len();
    if i == 1
        && let Some(store) = &s.delete_source
    {
        store.lock().unwrap().db.execute(
            "UPDATE messages SET body=json_set(body,'$.deleted',json('true')) WHERE channel='Dtest' AND ts='1.000000'",
            [],
        ).unwrap();
    }
    if i == s.fail {
        return Json(if s.uncertain {
            json!({"ok":false,"error":"request_timeout"})
        } else {
            json!({"ok":false,"error":"ratelimited"})
        });
    }
    Json(json!({"ok":true,"ts":format!("{i}.000001")}))
}
async fn fixture(uncertain: bool) -> (Slack, Arc<Mutex<Fixture>>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Mutex::new(Fixture {
        fail: 2,
        uncertain,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/chat.postMessage", post(message))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut c = Config::new("Fixture".into(), "owner".into());
    c.slack.loopback_test = true;
    c.slack.api_base = format!("http://127.0.0.1:{port}/");
    let slack = Slack::new(
        &c,
        Secrets {
            app_token: "test-app".into(),
            bot_token: "test-bot".into(),
        },
    )
    .unwrap();
    (slack, state, task)
}
fn queued(store: &mut Store) -> (Outgoing, String) {
    let trigger = Message {
        team: "Ttest".into(),
        channel: "Dtest".into(),
        thread: String::new(),
        ts: "1.000000".into(),
        revision: "1.000000".into(),
        sender: "Uhuman".into(),
        bot: false,
        text: "Please explain".into(),
        deleted: false,
        files: vec![],
    };
    store.ingest("request", &trigger, true).unwrap();
    let attention = Attention {
        settle_ms: 0,
        minimum_reply_interval_seconds: 0,
        ..Default::default()
    };
    let job = store.claim(&attention).unwrap().unwrap();
    let text="A paragraph with **formatting**, `code`, and [evidence](https://example.org/result). 🌟\n\n".repeat(350);
    store
        .finish(
            &job,
            Ok(Decision {
                decision: "speak".into(),
                text,
                reason: "Direct reply".into(),
                media: vec![],
            }),
            &attention,
        )
        .unwrap();
    let mut c = Config::new("Fixture".into(), "owner".into());
    c.enabled = true;
    (store.claim_outgoing(&c).unwrap().unwrap(), job.context)
}
#[tokio::test]
async fn retry_keeps_confirmed_parts_and_receipts_without_duplicate_messages() {
    let (slack, state, task) = fixture(false).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let (out, context) = queued(&mut store);
    let store = Arc::new(Mutex::new(store));
    assert!(matches!(
        slack.publish_durable(&out, &store).await,
        Err(ApiError::RateLimited(_))
    ));
    let first = state.lock().unwrap().calls[0]["client_msg_id"].clone();
    let receipt = store
        .lock()
        .unwrap()
        .response_delivery(&context, "future")
        .unwrap();
    assert_eq!(receipt["items"][0]["status"], "partially_delivered");
    assert_eq!(
        receipt["items"][0]["parts"][0]["url"],
        "https://app.slack.com/client/Ttest/Dtest?message_ts=1.000001"
    );
    assert_eq!(
        slack.publish_durable(&out, &store).await.unwrap(),
        "1.000001"
    );
    let calls = &state.lock().unwrap().calls;
    assert_eq!(
        calls.iter().filter(|p| p["client_msg_id"] == first).count(),
        1
    );
    assert_eq!(calls[1]["client_msg_id"], calls[2]["client_msg_id"]);
    assert!(
        calls
            .iter()
            .all(|p| p["markdown_text"].as_str().unwrap().chars().count() <= 10_000)
    );
    task.abort();
}
#[tokio::test]
async fn uncertain_part_is_not_replayed_after_restart() {
    let (slack, state, task) = fixture(true).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let (out, context) = queued(&mut store);
    let store = Arc::new(Mutex::new(store));
    assert!(matches!(
        slack.publish_durable(&out, &store).await,
        Err(ApiError::Uncertain)
    ));
    store.lock().unwrap().recover().unwrap();
    let reopened = Arc::new(Mutex::new(Store::open(home.path()).unwrap()));
    assert!(matches!(
        slack.publish_durable(&out, &reopened).await,
        Err(ApiError::Uncertain)
    ));
    assert_eq!(state.lock().unwrap().calls.len(), 2);
    let facts = reopened
        .lock()
        .unwrap()
        .response_delivery(&context, "future")
        .unwrap();
    assert_eq!(facts["items"][0]["parts"][0]["status"], "sent");
    assert_eq!(facts["items"][0]["parts"][1]["status"], "uncertain");
    task.abort();
}

#[tokio::test]
async fn source_deleted_during_delivery_stops_remaining_parts() {
    let (slack, state, task) = fixture(false).await;
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let (out, _) = queued(&mut store);
    let store = Arc::new(Mutex::new(store));
    state.lock().unwrap().delete_source = Some(store.clone());
    assert!(matches!(
        slack.publish_durable(&out, &store).await,
        Err(ApiError::Rejected(reason)) if reason == "reply_source_changed"
    ));
    assert_eq!(state.lock().unwrap().calls.len(), 1);
    task.abort();
}
