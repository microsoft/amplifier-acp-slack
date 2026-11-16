//! A connected agent is in no channel until a person invites it. The connector
//! reports that from the conversation listing it already makes after each
//! verified connection, without live Slack, credentials or a model.
use acp_slack_adapter::{
    config::{Config, Secrets},
    reconnect,
    slack::{self, Slack},
    store::Store,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Rooms = Arc<Mutex<Vec<Value>>>;
async fn api(State(rooms): State<Rooms>, Path(method): Path<String>) -> Json<Value> {
    Json(match method.as_str() {
        "users.conversations" => json!({"ok":true,"channels":*rooms.lock().unwrap()}),
        "conversations.history" => json!({"ok":true,"messages":[]}),
        _ => panic!("Unexpected Slack method {method}"),
    })
}
fn in_channel(store: &Arc<Mutex<Store>>) -> Value {
    store.lock().unwrap().status().unwrap()["in_channel"].clone()
}
fn message(channel: &str, kind: &str, ts: &str) -> Value {
    json!({"event_id":format!("event-{ts}"),"event":{"type":"message","channel":channel,"channel_type":kind,"ts":ts,"user":"UHUMAN","text":"hello"}})
}

#[tokio::test]
async fn status_says_when_the_agent_is_in_no_channel_yet() {
    let home = tempfile::tempdir().unwrap();
    let rooms = Rooms::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut c = Config::new("Membership test".into(), "owner".into());
    c.slack.loopback_test = true;
    c.slack.api_base = format!("http://{}/", listener.local_addr().unwrap());
    c.slack.team_id = "TTEST".into();
    c.slack.bot_user_id = "USELF".into();
    c.slack.bot_id = "BSELF".into();
    let app = Router::new()
        .route("/{method}", get(api))
        .with_state(rooms.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Slack::new(
        &c,
        Secrets {
            app_token: "fixture-app".into(),
            bot_token: "fixture-bot".into(),
        },
    )
    .unwrap();
    let store = Arc::new(Mutex::new(Store::open(home.path()).unwrap()));
    let since = reconnect::checkpoint(&store.lock().unwrap()).unwrap();

    // Nothing is claimed before Slack has listed this agent's conversations.
    assert_eq!(in_channel(&store), Value::Null);

    // Direct and group direct messages are not channels.
    *rooms.lock().unwrap() = vec![
        json!({"id":"DOWNER","is_im":true}),
        json!({"id":"GGROUP","is_mpim":true}),
    ];
    reconnect::recover(&client, &c, store.clone(), since)
        .await
        .unwrap();
    assert_eq!(in_channel(&store), json!(false));
    slack::receive(
        &c,
        &mut store.lock().unwrap(),
        &message("DOWNER", "im", "1.000001"),
    )
    .unwrap();
    assert_eq!(in_channel(&store), json!(false));

    // Slack sends channel events only to members, so the first one after an
    // invitation is enough; no second listing is needed.
    slack::receive(
        &c,
        &mut store.lock().unwrap(),
        &message("CROOM", "channel", "2.000001"),
    )
    .unwrap();
    assert_eq!(in_channel(&store), json!(true));

    // The next connection's listing is authoritative in both directions.
    reconnect::recover(&client, &c, store.clone(), since)
        .await
        .unwrap();
    assert_eq!(in_channel(&store), json!(false));
    *rooms.lock().unwrap() = vec![
        json!({"id":"DOWNER","is_im":true}),
        json!({"id":"GPRIVATE","is_group":true,"is_private":true}),
    ];
    reconnect::recover(&client, &c, store.clone(), since)
        .await
        .unwrap();
    assert_eq!(in_channel(&store), json!(true));
    server.abort();
}
