use acp_slack_adapter::{
    auth,
    config::{Config, private_write},
    context,
    state::AppState,
    store::Store,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

#[derive(Default)]
struct FakeSlack {
    info: HashMap<String, Value>,
    members: HashMap<String, Vec<String>>,
    history: HashMap<String, Vec<Value>>,
    replies: HashMap<(String, String), Vec<Value>>,
    calls: Vec<(String, String)>,
    unavailable_members: bool,
    unavailable_replies: bool,
    reaction_writes: Vec<Value>,
    reaction_error: Option<String>,
}

fn message(ts: &str, actor: &str, text: &str) -> Value {
    json!({"ts":ts,"user":actor,"text":text})
}

async fn slack_api(
    State(fake): State<Arc<Mutex<FakeSlack>>>,
    Path(method): Path<String>,
    Query(args): Query<HashMap<String, String>>,
) -> Json<Value> {
    let channel = args.get("channel").cloned().unwrap_or_default();
    let mut fake = fake.lock().unwrap();
    fake.calls.push((method.clone(), channel.clone()));
    let limit = args
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100);
    let cursor = args
        .get("cursor")
        .and_then(|v| v.strip_prefix("page:"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let value = match method.as_str() {
        "reactions.get" => {
            let item = fake
                .history
                .get(&channel)
                .into_iter()
                .flatten()
                .find(|m| m["ts"].as_str() == args.get("timestamp").map(String::as_str))
                .cloned();
            match item {
                Some(message) => json!({"ok":true,"type":"message","message":message}),
                None => json!({"ok":false,"error":"message_not_found"}),
            }
        }
        "conversations.info" => match fake.info.get(&channel) {
            Some(info) => json!({"ok":true,"channel":info}),
            None => json!({"ok":false,"error":"channel_not_found"}),
        },
        "conversations.members" => {
            if fake.unavailable_members {
                json!({"ok":false,"error":"service_unavailable"})
            } else {
                let all = fake.members.get(&channel).cloned().unwrap_or_default();
                let selected: Vec<_> = all.iter().skip(cursor).take(limit).cloned().collect();
                let next = if cursor + selected.len() < all.len() {
                    format!("page:{}", cursor + selected.len())
                } else {
                    String::new()
                };
                json!({"ok":true,"members":selected,"response_metadata":{"next_cursor":next}})
            }
        }
        "conversations.list" => {
            let mut channels: Vec<_> = fake.info.values().cloned().collect();
            channels.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            let selected: Vec<_> = channels.iter().skip(cursor).take(limit).cloned().collect();
            let next = if cursor + selected.len() < channels.len() {
                format!("page:{}", cursor + selected.len())
            } else {
                String::new()
            };
            json!({"ok":true,"channels":selected,"response_metadata":{"next_cursor":next}})
        }
        "conversations.replies" if fake.unavailable_replies => {
            json!({"ok":false,"error":"ratelimited"})
        }
        "conversations.history" | "conversations.replies" => {
            let mut messages = if method == "conversations.history" {
                fake.history.get(&channel).cloned().unwrap_or_default()
            } else {
                fake.replies
                    .get(&(channel.clone(), args.get("ts").cloned().unwrap_or_default()))
                    .cloned()
                    .unwrap_or_default()
            };
            let included_parent =
                if method == "conversations.replies" && args.contains_key("oldest") {
                    messages
                        .iter()
                        .find(|m| m["ts"].as_str() == args.get("ts").map(String::as_str))
                        .cloned()
                } else {
                    None
                };
            if let Some(oldest) = args.get("oldest") {
                messages.retain(|m| m["ts"].as_str().unwrap_or("") >= oldest.as_str());
            }
            if let Some(latest) = args.get("latest") {
                messages.retain(|m| m["ts"].as_str().unwrap_or("") <= latest.as_str());
            }
            messages.sort_by(|a, b| a["ts"].as_str().cmp(&b["ts"].as_str()));
            if method == "conversations.history" {
                messages.reverse();
            }
            let selected: Vec<_> = messages.iter().skip(cursor).take(limit).cloned().collect();
            let more = cursor + selected.len() < messages.len();
            let next = if more {
                format!("page:{}", cursor + selected.len())
            } else {
                String::new()
            };
            let mut selected = selected;
            if let Some(parent) = included_parent {
                selected.insert(0, parent);
            }
            json!({"ok":true,"messages":selected,"has_more":more,"response_metadata":{"next_cursor":next}})
        }
        _ => json!({"ok":false,"error":"unknown_method"}),
    };
    Json(value)
}

struct Fixture {
    home: tempfile::TempDir,
    state: Arc<AppState>,
    token: String,
    fake: Arc<Mutex<FakeSlack>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn slack_write(
    State(fake): State<Arc<Mutex<FakeSlack>>>,
    Path(method): Path<String>,
    Json(args): Json<Value>,
) -> Json<Value> {
    assert!(matches!(
        method.as_str(),
        "reactions.add" | "reactions.remove"
    ));
    let mut fake = fake.lock().unwrap();
    fake.reaction_writes
        .push(json!({"method":method,"args":args}));
    Json(match &fake.reaction_error {
        Some(error) => json!({"ok":false,"error":error}),
        None => json!({"ok":true}),
    })
}

impl Fixture {
    async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fake = Arc::new(Mutex::new(FakeSlack::default()));
        {
            let mut fake = fake.lock().unwrap();
            for (id, direct, other, members) in [
                ("CSHARED", false, "", vec!["UBOT", "UOWNER", "UPEER"]),
                ("CPRIVATE", false, "", vec!["UBOT", "UPEER"]),
                ("DOWNER", true, "UOWNER", vec!["UBOT", "UOWNER"]),
                ("DOTHER", true, "UPEER", vec!["UBOT", "UPEER"]),
                ("DSPOOF", false, "UOWNER", vec!["UBOT", "UOWNER", "UPEER"]),
            ] {
                fake.info.insert(id.into(),json!({"id":id,"is_im":direct,"is_mpim":id=="DSPOOF","user":other,"name":format!("name-{id}")}));
                fake.members
                    .insert(id.into(), members.into_iter().map(str::to_owned).collect());
                fake.history.insert(
                    id.into(),
                    vec![message(
                        "1000.000001",
                        "UOWNER",
                        &format!("History of {id}"),
                    )],
                );
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/api/{method}", get(slack_api).post(slack_write))
            .with_state(fake.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut config = Config::new("Test context provider".into(), "tester".into());
        config.context.enabled = true;
        config.context.owner_slack_user_id = "UOWNER".into();
        config.server.public_url = Some("https://localhost:18444".into());
        config.slack.team_id = "TTEST".into();
        config.slack.bot_user_id = "UBOT".into();
        config.slack.loopback_test = true;
        config.slack.api_base = format!("http://{address}/api/");
        config.save(home.path()).unwrap();
        private_write(
            &home.path().join("secrets/slack.json"),
            br#"{"app_token":"test-app","bot_token":"test-bot"}"#,
        )
        .unwrap();
        let state = Arc::new(AppState::open(home.path().to_path_buf()).unwrap());
        let token = auth::issue_token(&state, "context provider", &["context".into()], 300)
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            home,
            state,
            token,
            fake,
            task,
        }
    }

    fn input(channel: &str, thread: &str, actor: &str, bot: bool) -> Value {
        json!({"conversation":{"channel":channel,"thread":thread},"trigger":{"sender":actor,"bot":bot}})
    }

    fn grant(&self, channel: &str, thread: &str, actor: &str, bot: bool) -> String {
        context::issue_grant(
            &self.state.instance_config(),
            self.home.path(),
            &Self::input(channel, thread, actor, bot),
        )
        .unwrap()
    }

    async fn call(&self, endpoint: &str, token: Option<&str>, value: Value) -> (StatusCode, Value) {
        let router = Router::new()
            .route("/context/v1/query", post(context::query))
            .route("/context/mcp", post(context::mcp))
            .with_state(self.state.clone());
        let mut request = Request::builder()
            .method("POST")
            .uri(endpoint)
            .header("content-type", "application/json")
            .header("host", "localhost");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .oneshot(request.body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn query(&self, value: Value) -> (StatusCode, Value) {
        self.call("/context/v1/query", Some(&self.token), value)
            .await
    }
}

#[tokio::test]
async fn thread_only_grants_deny_parent_siblings_and_other_rooms_before_reading() {
    let f = Fixture::new().await;
    f.state.config.write().unwrap().context.thread_only_grants = true;
    f.fake.lock().unwrap().replies.insert(
        ("CSHARED".into(), "1000.000002".into()),
        vec![message("1000.000002", "UPEER", "Allowed thread")],
    );
    let grant = f.grant("CSHARED", "1000.000002", "UPEER", false);
    let access = json!({"mode":"grant","grant":grant});
    let (_, authorization) = f
        .query(json!({"operation":"authorize","access":access}))
        .await;
    assert_eq!(authorization["scope"], "thread");
    for operation in ["read", "orientation", "search"] {
        let mut request = json!({"operation":operation,"access":access});
        if operation == "search" {
            request["query"] = json!("Allowed");
        }
        let (status, result) = f.query(request.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["messages"][0]["text"], "Allowed thread");
        assert!(result.get("parent_context").is_none());
        request["conversation_id"] = json!("CSHARED");
        assert_eq!(f.query(request.clone()).await.0, StatusCode::FORBIDDEN);
        request["thread_id"] = json!("1000.000003");
        assert_eq!(f.query(request.clone()).await.0, StatusCode::FORBIDDEN);
        request["conversation_id"] = json!("DOWNER");
        request["thread_id"] = json!("1000.000002");
        assert_eq!(f.query(request).await.0, StatusCode::FORBIDDEN);
    }
    let (_, denied) = f.call("/context/mcp", Some(&grant), json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slack_context_orientation","arguments":{"conversation_id":"CSHARED"}}})).await;
    assert_eq!(denied["result"]["isError"], true);
    let orientation = context::orientation_for(
        &f.state.instance_config(),
        f.home.path(),
        &Fixture::input("CSHARED", "1000.000002", "UPEER", false),
    )
    .await
    .unwrap();
    assert_eq!(orientation["messages"][0]["text"], "Allowed thread");
    assert!(
        !f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "conversations.history")
    );
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|id| id != "UBOT");
    assert_eq!(
        f.query(json!({"operation":"read","access":access})).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn thread_only_policy_preserves_room_and_verified_owner_authority() {
    let f = Fixture::new().await;
    f.state.config.write().unwrap().context.thread_only_grants = true;
    let room_grant = f.grant("CSHARED", "", "UPEER", false);
    let (status, room) = f
        .query(json!({"operation":"read","access":{"mode":"grant","grant":room_grant}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(room["messages"][0]["text"], "History of CSHARED");
    let owner_grant = f.grant("DOWNER", "1000.000002", "UOWNER", false);
    let access = json!({"mode":"grant","grant":owner_grant});
    let (_, authorization) = f
        .query(json!({"operation":"authorize","access":access}))
        .await;
    assert_eq!(authorization["scope"], "owner_shared_membership");
    assert_eq!(
        f.query(json!({"operation":"read","access":access,"conversation_id":"CSHARED"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.query(json!({"operation":"read","access":access,"conversation_id":"DOTHER"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn owner_discovery_and_reads_require_shared_membership_and_exclude_other_dms() {
    let f = Fixture::new().await;
    let (status, list) = f
        .query(json!({"operation":"list","access":{"mode":"owner"}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<_> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["conversation_id"].as_str())
        .collect();
    assert!(ids.contains(&"CSHARED") && ids.contains(&"DOWNER"));
    assert!(!ids.contains(&"CPRIVATE") && !ids.contains(&"DOTHER"));
    assert_eq!(list["complete"], true);
    for id in ["CPRIVATE", "DOTHER"] {
        let status = f
            .query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":id}))
            .await
            .0;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert!(
        !f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, id)| method == "conversations.history"
                && ["CPRIVATE", "DOTHER"].contains(&id.as_str()))
    );
    let (status, read) = f
        .query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["messages"][0]["text"], "History of CSHARED");
    assert!(
        read["messages"][0]["url"]
            .as_str()
            .unwrap()
            .contains("TTEST/CSHARED")
    );
}

#[tokio::test]
async fn group_grants_are_channel_bound_even_when_the_actor_is_the_owner() {
    let f = Fixture::new().await;
    for (channel, actor) in [
        ("CSHARED", "UPEER"),
        ("CSHARED", "UOWNER"),
        ("DSPOOF", "UOWNER"),
    ] {
        let grant = f.grant(channel, "", actor, false);
        let access = json!({"mode":"grant","grant":grant});
        let (status, authorization) = f
            .query(json!({"operation":"authorize","access":access}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(authorization["owner"], false);
        assert_eq!(authorization["conversation_kind"], "group");
        let (_, list) = f.query(json!({"operation":"list","access":access})).await;
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert_eq!(list["items"][0]["conversation_id"], channel);
        assert_eq!(
            f.query(json!({"operation":"read","access":access,"conversation_id":"DOWNER"}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn owner_grant_requires_verified_one_to_one_metadata_and_nonbot_owner_trigger() {
    let f = Fixture::new().await;
    let owner = f.grant("DOWNER", "", "UOWNER", false);
    let (status, authorization) = f
        .query(json!({"operation":"authorize","access":{"mode":"grant","grant":owner}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(authorization["owner"], true);
    assert_eq!(authorization["actor_id"], "UOWNER");
    assert_eq!(authorization["conversation_kind"], "direct");
    assert_eq!(f.query(json!({"operation":"read","access":{"mode":"grant","grant":owner},"conversation_id":"CSHARED"})).await.0,StatusCode::OK);
    let bot = f.grant("DOWNER", "", "UOWNER", true);
    let (_, authorization) = f
        .query(json!({"operation":"authorize","access":{"mode":"grant","grant":bot}}))
        .await;
    assert_eq!(authorization["owner"], false);
    let other = f.grant("DOTHER", "", "UPEER", false);
    let (_, authorization) = f
        .query(json!({"operation":"authorize","access":{"mode":"grant","grant":other}}))
        .await;
    assert_eq!(authorization["owner"], false);
    f.fake.lock().unwrap().info.get_mut("DOWNER").unwrap()["is_mpim"] = true.into();
    let (_, authorization) = f
        .query(json!({"operation":"authorize","access":{"mode":"grant","grant":owner}}))
        .await;
    assert_eq!(
        authorization["owner"], false,
        "a previous owner grant does not cache its audience authority"
    );
}

#[tokio::test]
async fn revocation_and_upstream_failure_do_not_fall_back_to_cached_context() {
    let f = Fixture::new().await;
    let grant = f.grant("CSHARED", "", "UPEER", false);
    let body = json!({"operation":"read","access":{"mode":"grant","grant":grant},"conversation_id":"CSHARED"});
    assert_eq!(f.query(body.clone()).await.0, StatusCode::OK);
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|id| id != "UPEER");
    assert_eq!(f.query(body.clone()).await.0, StatusCode::FORBIDDEN);
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .push("UPEER".into());
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|id| id != "UBOT");
    assert_eq!(f.query(body.clone()).await.0, StatusCode::FORBIDDEN);
    f.fake.lock().unwrap().unavailable_members = true;
    assert_eq!(f.query(body).await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn grants_are_hashed_expire_and_are_invalidated_by_reconfiguration() {
    let f = Fixture::new().await;
    let grant = f.grant("DOWNER", "", "UOWNER", false);
    let db = Store::open(f.home.path()).unwrap().db;
    let (hash, value): (String, String) = db
        .query_row("SELECT hash,value FROM context_grants", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_ne!(hash, grant);
    assert!(!value.contains(&grant));
    assert_eq!(hash, auth::digest(&grant));
    db.execute("UPDATE context_grants SET expires_at=0", [])
        .unwrap();
    assert_eq!(
        f.query(json!({"operation":"authorize","access":{"mode":"grant","grant":grant}}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let fresh = f.grant("DOWNER", "", "UOWNER", false);
    f.state.config.write().unwrap().revision += 1;
    assert_eq!(
        f.query(json!({"operation":"authorize","access":{"mode":"grant","grant":fresh}}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        !Store::open(f.home.path())
            .unwrap()
            .activity()
            .unwrap()
            .to_string()
            .contains(&grant)
    );
}

#[tokio::test]
async fn thread_orientation_contains_parent_predecessor_latest_parent_and_thread_index() {
    let f = Fixture::new().await;
    {
        let mut fake = f.fake.lock().unwrap();
        let mut root = message("1000.000002", "UBOT", "The answer that opened this thread");
        root["reply_count"] = 2.into();
        root["latest_reply"] = "1000.000005".into();
        let mut roots = vec![
            message(
                "1000.000001",
                "UOWNER",
                "The question that prompted the parent reply",
            ),
            root.clone(),
        ];
        for n in 10..70 {
            roots.push(message(
                &format!("1000.{n:06}"),
                "UOWNER",
                &format!("Later parent message {n}"),
            ));
        }
        fake.history.insert("DOWNER".into(), roots);
        fake.replies.insert(
            ("DOWNER".into(), "1000.000002".into()),
            vec![
                root,
                message(
                    "1000.000004",
                    "UOWNER",
                    "Can you inspect the parent discussion?",
                ),
                message("1000.000005", "UBOT", "This is the child thread"),
            ],
        );
    }
    let config = f.state.instance_config();
    let value = context::orientation_for(
        &config,
        f.home.path(),
        &Fixture::input("DOWNER", "1000.000002", "UOWNER", false),
    )
    .await
    .unwrap();
    let text = value.to_string();
    assert!(text.contains("The question that prompted the parent reply"));
    assert!(text.contains("Later parent message 69"));
    assert!(text.contains("This is the child thread"));
    assert_eq!(value["conversation_kind"], "direct");
    assert_eq!(value["threads"][0]["thread_id"], "1000.000002");
    assert_eq!(value["complete"], false);
    assert_eq!(value["coverage"]["other_conversations_read"], false);
    f.fake.lock().unwrap().calls.clear();
    context::orientation_for(
        &config,
        f.home.path(),
        &Fixture::input("DOWNER", "", "UOWNER", false),
    )
    .await
    .unwrap();
    assert!(
        !f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "conversations.replies")
    );
}

#[tokio::test]
async fn mcp_requires_grant_and_exposes_scoped_tools_with_current_permissions() {
    let f = Fixture::new().await;
    let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}});
    assert_eq!(
        f.call("/context/mcp", Some(&f.token), request.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let grant = f.grant("CSHARED", "", "UPEER", false);
    let (status, init) = f.call("/context/mcp", Some(&grant), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let (_, tools) = f
        .call(
            "/context/mcp",
            Some(&grant),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 7);
    let search = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "slack_context_search")
        .unwrap();
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    assert_eq!(search["annotations"]["readOnlyHint"], true);
    assert!(!tools.to_string().contains(&grant));
    let (_,read) = f.call("/context/mcp",Some(&grant),json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"slack_context_read","arguments":{"conversation_id":"CSHARED"}}})).await;
    assert_eq!(read["result"]["isError"], false);
    assert!(read["result"]["structuredContent"]["messages"].is_array());
    let (_,denied) = f.call("/context/mcp",Some(&grant),json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"slack_context_read","arguments":{"conversation_id":"DOWNER"}}})).await;
    assert_eq!(denied["result"]["isError"], true);
    assert!(!denied.to_string().contains("History of DOWNER"));
    let (_, searched) = f.call("/context/mcp", Some(&grant), json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"slack_context_search","arguments":{"query":"history"}}})).await;
    assert_eq!(searched["result"]["isError"], false);
    assert_eq!(
        searched["result"]["structuredContent"]["messages"][0]["text"],
        "History of CSHARED"
    );
    let (_, missing_query) = f.call("/context/mcp", Some(&grant), json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"slack_context_search","arguments":{}}})).await;
    assert_eq!(missing_query["error"]["code"], -32602);
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|id| id != "UPEER");
    assert_eq!(
        f.call(
            "/context/mcp",
            Some(&grant),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/list"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn search_preserves_pagination_and_unchanged_source_messages() {
    let f = Fixture::new().await;
    f.fake.lock().unwrap().history.insert(
        "CSHARED".into(),
        vec![
            message("1000.000001", "UOWNER", "Older NEEDLE."),
            message("1000.000002", "UPEER", "No match in the newest window"),
        ],
    );
    let request = json!({"operation":"search","access":{"mode":"owner"},"conversation_id":"CSHARED","query":"needle","limit":1});
    let (status, first) = f.query(request.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["messages"], json!([]));
    assert_eq!(first["complete"], false);
    assert_eq!(first["next_cursor"], "page:1");
    assert_eq!(first["coverage"]["search"]["scanned_messages"], 1);
    assert_eq!(first["coverage"]["search"]["exhaustive_archive"], false);
    let mut next = request;
    next["cursor"] = first["next_cursor"].clone();
    let (_, second) = f.query(next).await;
    assert_eq!(second["messages"][0]["text"], "Older NEEDLE.");
    assert_eq!(second["messages"][0]["message_id"], "1000.000001");
    assert!(
        second["messages"][0]["url"]
            .as_str()
            .unwrap()
            .contains("TTEST/CSHARED")
    );
    assert_eq!(second["complete"], true);
    assert!(second["next_cursor"].is_null());
    let (_, read) = f
        .query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED"}))
        .await;
    assert_eq!(read["messages"].as_array().unwrap().len(), 2);
    assert_eq!(second["messages"][0], read["messages"][0]);
}

#[tokio::test]
async fn search_is_literal_and_does_not_claim_to_search_truncated_text_or_other_threads() {
    let f = Fixture::new().await;
    {
        let mut fake = f.fake.lock().unwrap();
        fake.replies.insert(
            ("CSHARED".into(), "1000.000001".into()),
            vec![
                message(
                    "1000.000001",
                    "UPEER",
                    "Use [a-z]+ literally; NEEDLE is the example.",
                ),
                message(
                    "1000.000002",
                    "UPEER",
                    &format!("{}hidden-needle", "x".repeat(2000)),
                ),
            ],
        );
        fake.replies.insert(
            ("CSHARED".into(), "1000.000003".into()),
            vec![message(
                "1000.000003",
                "UOWNER",
                "[a-z]+ from another thread",
            )],
        );
    }
    let grant = f.grant("CSHARED", "1000.000001", "UPEER", false);
    let request =
        json!({"operation":"search","access":{"mode":"grant","grant":grant},"query":"[a-z]+"});
    let (status, found) = f.query(request.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["thread_id"], "1000.000001");
    assert_eq!(found["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        found["messages"][0]["text"],
        "Use [a-z]+ literally; NEEDLE is the example."
    );
    assert_eq!(found["complete"], false);
    assert_eq!(found["coverage"]["scope"], "thread");
    assert_eq!(found["coverage"]["text_truncated"], true);
    assert_eq!(
        found["coverage"]["search"]["matches_returned_text_only"],
        true
    );
    let mut hidden = request;
    hidden["query"] = "hidden-needle".into();
    let (_, hidden) = f.query(hidden).await;
    assert_eq!(hidden["messages"], json!([]));
    assert_eq!(hidden["complete"], false);
    assert_eq!(hidden["coverage"]["search"]["scanned_messages"], 2);
}

#[tokio::test]
async fn search_cannot_bypass_room_scope_or_revoked_membership() {
    let f = Fixture::new().await;
    let grant = f.grant("CSHARED", "", "UOWNER", false);
    let access = json!({"mode":"grant","grant":grant});
    for conversation in ["DOWNER", "DOTHER", "CPRIVATE"] {
        let request = json!({"operation":"search","access":access,"conversation_id":conversation,"query":"History"});
        assert_eq!(f.query(request).await.0, StatusCode::FORBIDDEN);
    }
    assert!(
        !f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "conversations.history")
    );
    let request = json!({"operation":"search","access":access,"query":"History"});
    assert_eq!(f.query(request.clone()).await.0, StatusCode::OK);
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|id| id != "UBOT");
    assert_eq!(f.query(request).await.0, StatusCode::FORBIDDEN);
    let (status, _) = f.call("/context/mcp", Some(&grant), json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slack_context_search","arguments":{"query":"History"}}})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn search_rejects_missing_blank_or_oversized_query_before_retrieval() {
    let f = Fixture::new().await;
    for query in [
        Value::Null,
        json!(""),
        json!("  \n"),
        json!("x".repeat(201)),
    ] {
        assert_eq!(f.query(json!({"operation":"search","access":{"mode":"owner"},"conversation_id":"CSHARED","query":query})).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(f.query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED","query":"ignored?"})).await.0, StatusCode::BAD_REQUEST);
    assert!(f.fake.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn coverage_reports_pagination_and_text_truncation_and_scope_changes_apply_immediately() {
    let f = Fixture::new().await;
    {
        let mut fake = f.fake.lock().unwrap();
        let large = "x".repeat(20_000);
        fake.history.insert(
            "CSHARED".into(),
            (1..=15)
                .map(|n| message(&format!("1000.{n:06}"), "UOWNER", &large))
                .collect(),
        );
    }
    let (_,read) = f.query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED","limit":2})).await;
    assert_eq!(read["complete"], false);
    assert!(read["next_cursor"].is_string());
    assert_eq!(read["coverage"]["text_truncated"], true);
    assert!(
        read["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["text"].as_str().unwrap().len() <= 2000)
    );
    f.state.config.write().unwrap().slack.channels = vec!["DOWNER".into()];
    assert_eq!(
        f.query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    f.state.config.write().unwrap().context.enabled = false;
    assert_eq!(
        f.query(json!({"operation":"list","access":{"mode":"owner"}}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn query_requires_bearer_context_scope_and_does_not_accept_arbitrary_arguments() {
    let f = Fixture::new().await;
    let body = json!({"operation":"list","access":{"mode":"owner"}});
    assert_eq!(
        f.call("/context/v1/query", None, body.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    let observer =
        auth::issue_token(&f.state, "observer", &["observe".into()], 300).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned();
    assert_eq!(
        f.call("/context/v1/query", Some(&observer), body).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.query(
            json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"../../private"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.query(
            json!({"operation":"publish","access":{"mode":"owner"},"text":"not a supported action"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert!(f.fake.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn unlinked_owner_can_use_current_conversation_but_not_owner_scope() {
    let f = Fixture::new().await;
    f.state
        .config
        .write()
        .unwrap()
        .context
        .owner_slack_user_id
        .clear();
    let grant = f.grant("DOWNER", "", "UOWNER", false);
    let (_, authorized) = f
        .query(json!({"operation":"authorize","access":{"mode":"grant","grant":grant}}))
        .await;
    assert_eq!(authorized["owner"], false);
    assert_eq!(authorized["conversation_kind"], "direct");
    assert_eq!(
        f.query(json!({"operation":"read","access":{"mode":"grant","grant":grant}}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.query(json!({"operation":"list","access":{"mode":"owner"}}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn discovery_has_a_bounded_page_budget_and_preserves_a_continuation_after_filtering() {
    let f = Fixture::new().await;
    {
        let mut fake = f.fake.lock().unwrap();
        fake.info.clear();
        fake.members.clear();
        for n in 0..12 {
            let id = format!("C{n:03}");
            fake.info.insert(
                id.clone(),
                json!({"id":id,"name":format!("room-{n}"),"is_im":false}),
            );
            fake.members.insert(
                id,
                if n < 6 {
                    vec!["UBOT".into(), "UPEER".into()]
                } else {
                    vec!["UBOT".into(), "UOWNER".into()]
                },
            );
        }
    }
    let (_, first) = f
        .query(json!({"operation":"list","access":{"mode":"owner"},"limit":2}))
        .await;
    assert!(first["items"].as_array().unwrap().is_empty());
    assert_eq!(first["complete"], false);
    assert_eq!(first["next_cursor"], "page:6");
    assert_eq!(
        f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(method, _)| method == "conversations.list")
            .count(),
        3
    );
    let (_,second) = f.query(json!({"operation":"list","access":{"mode":"owner"},"limit":2,"cursor":first["next_cursor"]})).await;
    assert_eq!(second["items"][0]["conversation_id"], "C006");
    assert_eq!(second["items"][1]["conversation_id"], "C007");
    assert_eq!(second["complete"], false);
    assert_eq!(second["next_cursor"], "page:8");
}

#[tokio::test]
async fn scoped_thread_defaults_to_itself_but_can_navigate_its_parent_and_siblings() {
    let f = Fixture::new().await;
    {
        let mut fake = f.fake.lock().unwrap();
        fake.replies.insert(
            ("CSHARED".into(), "1000.000002".into()),
            vec![message("1000.000002", "UPEER", "Current thread content")],
        );
        fake.replies.insert(
            ("CSHARED".into(), "1000.000003".into()),
            vec![message("1000.000003", "UOWNER", "Sibling thread content")],
        );
    }
    let grant = f.grant("CSHARED", "1000.000002", "UPEER", false);
    let access = json!({"mode":"grant","grant":grant});
    let (_, current) = f.query(json!({"operation":"read","access":access})).await;
    assert_eq!(current["thread_id"], "1000.000002");
    assert_eq!(current["messages"][0]["text"], "Current thread content");
    let (_, parent) = f
        .query(json!({"operation":"read","access":access,"conversation_id":"CSHARED"}))
        .await;
    assert!(parent["thread_id"].is_null());
    assert_eq!(parent["messages"][0]["text"], "History of CSHARED");
    let (_,sibling) = f.query(json!({"operation":"read","access":access,"conversation_id":"CSHARED","thread_id":"1000.000003"})).await;
    assert_eq!(sibling["messages"][0]["text"], "Sibling thread content");
}

#[tokio::test]
async fn mcp_rejects_cross_origin_requests_before_reading_slack() {
    let f = Fixture::new().await;
    let grant = f.grant("CSHARED", "", "UPEER", false);
    let router = Router::new()
        .route("/context/mcp", post(context::mcp))
        .with_state(f.state.clone());
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/context/mcp")
                .header("content-type", "application/json")
                .header("host", "localhost")
                .header("origin", "https://another-origin.invalid")
                .header("authorization", format!("Bearer {grant}"))
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(f.fake.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn connector_updates_require_explicit_policy_and_verified_owner_dm() {
    let f = Fixture::new().await;
    let owner = f.grant("DOWNER", "", "UOWNER", false);
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    let (_, before) = f.call("/context/mcp", Some(&owner), list.clone()).await;
    let names = before["result"]["tools"].as_array().unwrap();
    assert!(
        names
            .iter()
            .any(|v| v["name"] == "slack_connector_update_status")
    );
    assert!(
        !names
            .iter()
            .any(|v| v["name"] == "slack_connector_update_apply")
    );
    let apply = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"slack_connector_update_apply","arguments":{"plan_id":"00000000-0000-0000-0000-000000000000"}}});
    assert_eq!(
        f.call("/context/mcp", Some(&owner), apply.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    acp_slack_adapter::config::private_dir(&f.home.path().join("updates")).unwrap();
    private_write(&f.home.path().join("updates/policy.json"),json!({"schema":1,"adapter_id":f.state.instance_config().id,"revision":1,"enabled":true,"agent_enabled":true,"owner_slack_user_id":"UOWNER"}).to_string().as_bytes()).unwrap();
    for (room, actor, bot) in [
        ("CSHARED", "UOWNER", false),
        ("DSPOOF", "UOWNER", false),
        ("DOTHER", "UPEER", false),
        ("DOWNER", "UOWNER", true),
    ] {
        let grant = f.grant(room, "", actor, bot);
        assert_eq!(
            f.call("/context/mcp", Some(&grant), apply.clone()).await.0,
            StatusCode::FORBIDDEN
        );
        let (_, listed) = f.call("/context/mcp", Some(&grant), list.clone()).await;
        assert!(
            !listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "slack_connector_update_apply")
        );
    }
    assert_eq!(
        f.call("/context/mcp", Some(&f.token), apply.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, listed) = f.call("/context/mcp", Some(&owner), list).await;
    let supported = acp_slack_adapter::updates::status(f.home.path()).unwrap()["supported"] == true;
    assert_eq!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "slack_connector_update_apply"),
        supported
    );
    if supported {
        let (_,invalid)=f.call("/context/mcp",Some(&owner),json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"slack_connector_update_plan","arguments":{"command":"do anything"}}})).await;
        assert_eq!(invalid["error"]["code"], -32602);
    }
}

fn record_reply(f: &Fixture, channel: &str, thread: &str, ts: &str, text: &str) {
    let message = acp_slack_adapter::model::Message {
        team: "TTEST".into(),
        channel: channel.into(),
        thread: thread.into(),
        ts: ts.into(),
        revision: ts.into(),
        sender: "UOWNER".into(),
        bot: false,
        text: text.into(),
        deleted: false,
        files: vec![],
    };
    Store::open(f.home.path())
        .unwrap()
        .ingest(&format!("{channel}-{ts}"), &message, false)
        .unwrap();
}

#[tokio::test]
async fn channel_catchup_finds_new_outcome_in_old_thread_and_refreshes_cached_text() {
    let f = Fixture::new().await;
    let root = "1000.000001";
    for n in 2..=80 {
        record_reply(
            &f,
            "CSHARED",
            root,
            &format!("1001.{n:06}"),
            "STALE-CACHED-CONTENT",
        );
    }
    record_reply(
        &f,
        "DOTHER",
        "1000.000009",
        "9999.000001",
        "PRIVATE-CACHED-CONTENT",
    );
    {
        let mut fake = f.fake.lock().unwrap();
        fake.history.insert(
            "CSHARED".into(),
            (1..=80)
                .map(|n| {
                    message(
                        &format!("1000.{n:06}"),
                        "UOWNER",
                        "Older main-channel status: awaiting review",
                    )
                })
                .collect(),
        );
        let mut replies: Vec<_> = (2..80)
            .map(|n| message(&format!("1001.{n:06}"), "UBOT", "Review discussion"))
            .collect();
        replies.insert(
            0,
            message(root, "UOWNER", "Original task, before the outcome"),
        );
        replies.push(message(
            "1001.000080",
            "UOWNER",
            "Completion receipt: reviewed version merged as abc123; accepted.",
        ));
        fake.replies
            .insert(("CSHARED".into(), root.into()), replies);
    }
    let grant = f.grant("CSHARED", "", "UPEER", false);
    let (status, value) = f
        .query(json!({"operation":"orientation","access":{"mode":"grant","grant":grant}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(value.to_string().contains("merged as abc123"));
    assert!(!value.to_string().contains("STALE-CACHED-CONTENT"));
    assert!(!value.to_string().contains("PRIVATE-CACHED-CONTENT"));
    assert_eq!(
        value["recent_thread_activity"]["items"][0]["thread_id"],
        root
    );
    assert_eq!(
        value["recent_thread_activity"]["items"][0]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(value["recent_thread_activity"]["complete"], false);
    assert!(
        value["recent_thread_activity"]["items"][0]["messages"][3]["url"]
            .as_str()
            .unwrap()
            .contains("1001.000080")
    );
    assert!(
        !f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(_, channel)| channel == "DOTHER")
    );
}

#[tokio::test]
async fn recent_thread_windows_are_bounded_and_never_replay_deleted_cached_replies() {
    let f = Fixture::new().await;
    for room in 1..=6 {
        let root = format!("1000.{room:06}");
        for n in 1..=8 {
            let ts = format!("100{room}.{n:06}");
            record_reply(&f, "CSHARED", &root, &ts, "DELETED-CACHED-TEXT");
            if room != 6 {
                f.fake
                    .lock()
                    .unwrap()
                    .replies
                    .entry(("CSHARED".into(), root.clone()))
                    .or_default()
                    .push(message(&ts, "UBOT", &"x".repeat(5000)));
            }
        }
    }
    let value = context::orientation_for(
        &f.state.instance_config(),
        f.home.path(),
        &Fixture::input("CSHARED", "", "UPEER", false),
    )
    .await
    .unwrap();
    let items = value["recent_thread_activity"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(items[0]["messages"].as_array().unwrap().is_empty());
    assert!(!value.to_string().contains("DELETED-CACHED-TEXT"));
    let chars: usize = items
        .iter()
        .flat_map(|window| window["messages"].as_array().unwrap())
        .map(|m| m["text"].as_str().unwrap().chars().count())
        .sum();
    assert!(chars <= 6000);
    assert!(
        items
            .iter()
            .all(|window| window["messages"].as_array().unwrap().len() <= 4)
    );
    assert_eq!(
        f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(method, _)| method == "conversations.replies")
            .count(),
        6
    );
}

#[tokio::test]
async fn recent_thread_discovery_respects_live_membership_and_thread_only_grants() {
    let f = Fixture::new().await;
    record_reply(
        &f,
        "CSHARED",
        "1000.000001",
        "1001.000001",
        "Private sibling",
    );
    f.state.config.write().unwrap().context.thread_only_grants = true;
    let value = context::orientation_for(
        &f.state.instance_config(),
        f.home.path(),
        &Fixture::input("CSHARED", "1000.000002", "UPEER", false),
    )
    .await
    .unwrap();
    assert!(value.get("recent_thread_activity").is_none());
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|member| member != "UPEER");
    f.fake.lock().unwrap().calls.clear();
    assert!(
        context::orientation_for(
            &f.state.instance_config(),
            f.home.path(),
            &Fixture::input("CSHARED", "", "UPEER", false)
        )
        .await
        .is_err()
    );
    assert!(
        !f.fake
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(method, _)| method == "conversations.replies")
    );
}

#[tokio::test]
async fn unavailable_thread_refresh_keeps_parent_context_without_cached_reply_text() {
    let f = Fixture::new().await;
    record_reply(
        &f,
        "CSHARED",
        "1000.000001",
        "1001.000001",
        "CACHED-OUTCOME-MUST-NOT-LEAK",
    );
    f.fake.lock().unwrap().unavailable_replies = true;
    let value = context::orientation_for(
        &f.state.instance_config(),
        f.home.path(),
        &Fixture::input("CSHARED", "", "UPEER", false),
    )
    .await
    .unwrap();
    assert!(value.to_string().contains("History of CSHARED"));
    assert!(!value.to_string().contains("CACHED-OUTCOME-MUST-NOT-LEAK"));
    assert_eq!(
        value["recent_thread_activity"]["items"][0]["available"],
        false
    );
    assert_eq!(
        value["recent_thread_activity"]["items"][0]["complete"],
        false
    );
}

#[tokio::test]
async fn controls_do_not_displace_requested_messages_or_enter_search_and_thread_candidates() {
    let f = Fixture::new().await;
    let mut raw: Vec<Value> = (1..=4)
        .map(|n| {
            message(
                &format!("1000.00000{n}"),
                "UPEER",
                &format!("Real proposal {n}"),
            )
        })
        .collect();
    for (n, kind) in ["acp_work_status", "acp_permission", "acp_question"]
        .iter()
        .enumerate()
    {
        let mut item = message(&format!("1000.00000{}", n + 5), "UBOT", "PRIVATE CONTROL");
        item["metadata"] = json!({"event_type":kind});
        item["reply_count"] = 9.into();
        raw.push(item);
    }
    // A known owned control remains excluded if Slack omits metadata later.
    raw.push(message(
        "1000.000008",
        "UBOT",
        "PRIVATE CONTROL WITHOUT METADATA",
    ));
    Store::open(f.home.path())
        .unwrap()
        .db
        .execute(
            "INSERT INTO ignored_activity VALUES('CSHARED','1000.000008',0)",
            [],
        )
        .unwrap();
    f.fake.lock().unwrap().history.insert("CSHARED".into(), raw);
    let (status,read)=f.query(json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED","limit":4})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["messages"].as_array().unwrap().len(), 4);
    assert_eq!(read["complete"], true);
    assert!(!read.to_string().contains("PRIVATE CONTROL"));
    let (_,search)=f.query(json!({"operation":"search","access":{"mode":"owner"},"conversation_id":"CSHARED","query":"CONTROL","limit":4})).await;
    assert_eq!(search["messages"], json!([]));
    assert_eq!(search["coverage"]["search"]["scanned_messages"], 4);
    let orientation = context::orientation_for(
        &f.state.instance_config(),
        f.home.path(),
        &Fixture::input("CSHARED", "", "UPEER", false),
    )
    .await
    .unwrap();
    assert!(!orientation.to_string().contains("PRIVATE CONTROL"));
    assert!(!orientation.to_string().contains("1000.000005"));
}

#[tokio::test]
async fn control_backfill_is_bounded_and_keeps_exact_continuation() {
    let f = Fixture::new().await;
    let mut raw = vec![message("1000.000001", "UPEER", "Real proposal")];
    for n in 2..=8 {
        let mut item = message(&format!("1000.00000{n}"), "UBOT", "Thinking");
        item["metadata"] = json!({"event_type":"acp_work_status"});
        raw.push(item);
    }
    f.fake.lock().unwrap().history.insert("CSHARED".into(), raw);
    let request =
        json!({"operation":"read","access":{"mode":"owner"},"conversation_id":"CSHARED","limit":1});
    let (_, first) = f.query(request.clone()).await;
    assert_eq!(first["messages"], json!([]));
    assert_eq!(first["complete"], false);
    assert_eq!(first["next_cursor"], "page:4");
    let mut next = request;
    next["cursor"] = first["next_cursor"].clone();
    let (_, last) = f.query(next).await;
    assert_eq!(last["messages"][0]["text"], "Real proposal");
    assert_eq!(last["complete"], true);
}

#[tokio::test]
async fn referenced_error_root_remains_visible_but_unrelated_controls_do_not() {
    let f = Fixture::new().await;
    let mut root = message(
        "1000.000002",
        "UBOT",
        "The agent finished but its answer could not be read.",
    );
    root["metadata"] = json!({"event_type":"acp_work_status"});
    let mut unrelated = message("1000.000003", "UBOT", "UNRELATED STATUS");
    unrelated["metadata"] = json!({"event_type":"acp_work_status"});
    f.fake.lock().unwrap().replies.insert(
        ("CSHARED".into(), "1000.000002".into()),
        vec![
            root,
            unrelated,
            message("1000.000004", "UPEER", "Explain this error"),
        ],
    );
    let value = context::orientation_for(
        &f.state.instance_config(),
        f.home.path(),
        &Fixture::input("CSHARED", "1000.000002", "UPEER", false),
    )
    .await
    .unwrap();
    assert!(value.to_string().contains("answer could not be read"));
    assert!(!value.to_string().contains("UNRELATED STATUS"));
    let grant = f.grant("CSHARED", "1000.000002", "UPEER", false);
    let (_,read)=f.call("/context/mcp",Some(&grant),json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slack_context_read","arguments":{}}})).await;
    assert_eq!(
        read["result"]["structuredContent"]["root"]["connector_notice"],
        true
    );
}
#[tokio::test]
async fn reactions_are_current_scoped_and_writes_only_use_the_bots_identity() {
    let f = Fixture::new().await;
    f.fake.lock().unwrap().history.get_mut("CSHARED").unwrap()[0]["reactions"] =
        json!([{"name":"eyes","count":3,"users":["UPEER"]}]);
    let grant = f.grant("CSHARED", "", "UPEER", false);
    let call = |name: &str, args: Value| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
    let args = json!({"message_id":"1000.000001"});
    let (_, r) = f
        .call(
            "/context/mcp",
            Some(&grant),
            call("slack_reactions_get", args.clone()),
        )
        .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert!(r.to_string().contains("eyes"));
    let (_, r) = f
        .call(
            "/context/mcp",
            Some(&grant),
            call(
                "slack_reactions_add",
                json!({"message_id":"1000.000001","name":"white_check_mark"}),
            ),
        )
        .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(
        f.fake.lock().unwrap().reaction_writes[0]["args"],
        json!({"channel":"CSHARED","timestamp":"1000.000001","name":"white_check_mark"})
    );
    for args in [
        json!({"message_id":"1000.000001","name":"eyes","conversation_id":"CPRIVATE"}),
        json!({"message_id":"1000.000001","name":"eyes","user":"UOWNER"}),
    ] {
        let (_, r) = f
            .call(
                "/context/mcp",
                Some(&grant),
                call("slack_reactions_add", args),
            )
            .await;
        assert!(
            r["result"]["isError"] == true || r.get("error").is_some(),
            "{r}"
        );
    }
    f.fake
        .lock()
        .unwrap()
        .members
        .get_mut("CSHARED")
        .unwrap()
        .retain(|u| u != "UPEER");
    let (_, r) = f
        .call(
            "/context/mcp",
            Some(&grant),
            call(
                "slack_reactions_remove",
                json!({"message_id":"1000.000001","name":"eyes"}),
            ),
        )
        .await;
    assert!(
        r["result"]["isError"] == true || r.get("error").is_some(),
        "{r}"
    );
    assert_eq!(f.fake.lock().unwrap().reaction_writes.len(), 1);
}
