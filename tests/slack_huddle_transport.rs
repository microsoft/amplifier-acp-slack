//! Exercise the real Slack client against HTTP responses, without live Slack,
//! production credentials, sessions or a model.
use acp_slack_adapter::{
    config::{Config, Secrets},
    slack::Slack,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

type Calls = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;
async fn api(
    State(calls): State<Calls>,
    Path(method): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    calls.lock().unwrap().push((method.clone(), q.clone()));
    Json(match method.as_str() {
        "conversations.members" if q.get("cursor").is_none_or(String::is_empty) => {
            let members = (0..200).map(|i| format!("UOTHER{i}")).collect::<Vec<_>>();
            json!({"ok":true,"members":members,"response_metadata":{"next_cursor":"page-two"}})
        }
        "conversations.members" => {
            assert_eq!(q["cursor"], "page-two");
            json!({"ok":true,"members":["WBOT","UPEER","UHUMAN"],"response_metadata":{"next_cursor":""}})
        }
        "users.info" => {
            json!({"ok":true,"user":{"id":q["user"],"is_bot":q["user"]!="UHUMAN","deleted":false}})
        }
        "chat.getPermalink" => {
            assert_eq!(q["channel"], "CROOM");
            let link = match q["message_ts"].as_str() {
                "badhost" => "https://team.slack.com.example.invalid/archives/CROOM/p123",
                "plaintext" => "http://team.slack.com/archives/CROOM/p123",
                _ => "https://team.slack.com/archives/CROOM/p123",
            };
            json!({"ok":true,"permalink":link})
        }
        "conversations.history" | "conversations.replies" => {
            assert_eq!(q["include_all_metadata"], "true");
            let mut messages = vec![
                json!({"ts":"1.1","user":"UOTHER","bot_id":"BOTHER","client_msg_id":"delivery-id"}),
                json!({"ts":"1.2","user":"UOTHER","bot_id":"BOTHER","metadata":{"event_type":"acp_coordination_thread","event_payload":{"id":"delivery-id","version":1}}}),
                json!({"ts":"1.3","user":"USELF","bot_id":"BSELF","client_msg_id":"different-id"}),
            ];
            if q["channel"] == "COWNUSER" {
                messages.push(json!({"ts":"2.1","user":"USELF","client_msg_id":"delivery-id"}));
            }
            if q["channel"] == "COWNBOT" {
                messages.push(json!({"ts":"2.2","bot_id":"BSELF","metadata":{"event_payload":{"id":"delivery-id"}}}));
            }
            json!({"ok":true,"messages":messages})
        }
        _ => panic!("Unexpected Slack method {method}"),
    })
}
async fn fixture() -> (Slack, Calls, tokio::task::JoinHandle<()>) {
    let calls = Calls::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut c = Config::new("Transport test".into(), "owner".into());
    c.slack.loopback_test = true;
    c.slack.api_base = format!("http://{}/", listener.local_addr().unwrap());
    c.slack.bot_user_id = "USELF".into();
    c.slack.bot_id = "BSELF".into();
    let app = Router::new()
        .route("/{method}", get(api))
        .with_state(calls.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Slack::new(
        &c,
        Secrets {
            app_token: "fixture-app".into(),
            bot_token: "fixture-bot".into(),
        },
    )
    .unwrap();
    (client, calls, server)
}
#[tokio::test]
async fn bot_membership_finds_user_and_workspace_ids_beyond_first_page() {
    let (client, calls, server) = fixture().await;
    client
        .authorize_bot_members("CROOM", &["WBOT".into(), "UPEER".into()])
        .await
        .unwrap();
    let calls = calls.lock().unwrap();
    let pages = calls
        .iter()
        .filter(|(method, _)| method == "conversations.members")
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[1].1["cursor"], "page-two");
    let people = calls
        .iter()
        .filter(|(method, _)| method == "users.info")
        .map(|(_, q)| q["user"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(people, vec!["WBOT", "UPEER"]);
    server.abort();
}
#[tokio::test]
async fn channel_membership_does_not_turn_human_into_agent_peer() {
    let (client, _calls, server) = fixture().await;
    assert!(
        client
            .authorize_bot_members("CROOM", &["UHUMAN".into()])
            .await
            .is_err()
    );
    assert!(
        client
            .authorize_bot_members("CROOM", &["UMISSING".into()])
            .await
            .is_err()
    );
    server.abort();
}
#[tokio::test]
async fn permalink_uses_actual_get_query_and_rejects_non_slack_or_insecure_link() {
    let (client, calls, server) = fixture().await;
    assert_eq!(
        client.permalink("CROOM", "12.3").await.unwrap(),
        "https://team.slack.com/archives/CROOM/p123"
    );
    assert!(client.permalink("CROOM", "badhost").await.is_err());
    assert!(client.permalink("CROOM", "plaintext").await.is_err());
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, "chat.getPermalink");
    assert_eq!(calls[0].1["message_ts"], "12.3");
    server.abort();
}
#[tokio::test]
async fn history_confirmation_ignores_copied_foreign_receipts_then_accepts_own_author() {
    let (client, calls, server) = fixture().await;
    assert_eq!(
        client
            .find_message("CFOREIGN", "", "delivery-id", 0)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        client
            .find_message("COWNUSER", "", "delivery-id", 0)
            .await
            .unwrap(),
        Some("2.1".into())
    );
    assert_eq!(
        client
            .find_message("COWNBOT", "root.1", "delivery-id", 0)
            .await
            .unwrap(),
        Some("2.2".into())
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].0, "conversations.history");
    assert_eq!(calls[2].0, "conversations.replies");
    assert_eq!(calls[2].1["ts"], "root.1");
    server.abort();
}
