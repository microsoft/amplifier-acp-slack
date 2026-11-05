//! Slack conversation context and own reactions, with fresh audience checks at every use.
//!
//! The provider never reads agent history or publishes Slack messages. Reactions
//! are explicit, current-conversation-only bot actions through separate MCP tools. A grant is
//! a short-lived reference to an adapter-observed trigger, not a cached permission.

use crate::{
    auth,
    config::Config,
    error::{ApiError, ApiResult},
    state::{AppState, now_ms},
    store::Store,
};
use anyhow::{Result, ensure};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

const GRANT_LIFETIME_MS: i64 = 30 * 60 * 1000;
const MAX_MESSAGES: usize = 40;
const MAX_TEXT_CHARS: usize = 16_000;
const MAX_MESSAGE_CHARS: usize = 2_000;
const MAX_LIST_PAGES: usize = 3;
const MAX_MEMBER_PAGES: usize = 5;

#[derive(Clone, Deserialize, Serialize)]
struct Grant {
    adapter_id: String,
    revision: u64,
    team: String,
    bot_user: String,
    configured_owner: String,
    channel: String,
    thread: String,
    actor: String,
    actor_bot: bool,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Access {
    Owner,
    Grant { grant: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    operation: String,
    access: Access,
    conversation_id: Option<String>,
    thread_id: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    query: Option<String>,
}

#[derive(Clone)]
struct Conversation {
    id: String,
    label: String,
    direct: bool,
    other_user: Option<String>,
}

struct Authority {
    owner: bool,
    actor: String,
    origin: Option<Conversation>,
    thread: String,
}

fn unavailable() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Slack context is unavailable; no access or completeness was assumed".into(),
        None,
    )
}

fn enabled(c: &Config) -> ApiResult<()> {
    if !c.context.enabled
        || (!c.context.owner_slack_user_id.is_empty()
            && !identifier(&c.context.owner_slack_user_id))
        || !identifier(&c.slack.bot_user_id)
        || !identifier(&c.slack.team_id)
    {
        return Err(ApiError::forbidden());
    }
    Ok(())
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 100 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn timestamp(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && value.bytes().any(|b| b.is_ascii_digit())
        && value.bytes().filter(|b| *b == b'.').count() <= 1
}

fn grant_store(home: &Path) -> Result<Store> {
    let store = Store::open(home)?;
    store.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS context_grants(
            hash TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            expires_at INTEGER NOT NULL
         );",
    )?;
    Ok(store)
}

fn grant_from_input(c: &Config, input: &Value) -> Result<Grant> {
    ensure!(enabled(c).is_ok(), "Conversation context is not enabled");
    let channel = input["conversation"]["channel"].as_str().unwrap_or("");
    let thread = input["conversation"]["thread"].as_str().unwrap_or("");
    let actor = input["trigger"]["sender"].as_str().unwrap_or("");
    let actor_bot = input["trigger"]["bot"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("Missing trigger actor type"))?;
    ensure!(
        identifier(channel)
            && identifier(actor)
            && (thread.is_empty() || timestamp(thread))
            && c.allows(channel, ""),
        "The trigger is outside the configured context scope"
    );
    Ok(Grant {
        adapter_id: c.id.clone(),
        revision: c.revision,
        team: c.slack.team_id.clone(),
        bot_user: c.slack.bot_user_id.clone(),
        configured_owner: c.context.owner_slack_user_id.clone(),
        channel: channel.into(),
        thread: thread.into(),
        actor: actor.into(),
        actor_bot,
    })
}

/// Mint a reference to an observed trigger. Permissions are verified when used.
/// Only the digest is stored; neither audit records nor responses contain it.
pub fn issue_grant(c: &Config, home: &Path, input: &Value) -> Result<String> {
    let grant = grant_from_input(c, input)?;
    let store = grant_store(home)?;
    store.db.execute(
        "DELETE FROM context_grants WHERE expires_at<=?1",
        [now_ms()],
    )?;
    let token = auth::random_token();
    store.db.execute(
        "INSERT INTO context_grants(hash,value,expires_at) VALUES(?1,?2,?3)",
        params![
            auth::digest(&token),
            serde_json::to_string(&grant)?,
            now_ms() + GRANT_LIFETIME_MS
        ],
    )?;
    Ok(token)
}

fn load_grant(c: &Config, home: &Path, token: &str) -> ApiResult<Grant> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::unauthorized());
    }
    let store = grant_store(home)?;
    let value: Option<String> = store
        .db
        .query_row(
            "SELECT value FROM context_grants WHERE hash=?1 AND expires_at>?2",
            params![auth::digest(token), now_ms()],
            |row| row.get(0),
        )
        .optional()?;
    let grant: Grant = serde_json::from_str(&value.ok_or_else(ApiError::unauthorized)?)
        .map_err(|_| ApiError::unauthorized())?;
    if grant.adapter_id != c.id
        || grant.revision != c.revision
        || grant.team != c.slack.team_id
        || grant.bot_user != c.slack.bot_user_id
        || grant.configured_owner != c.context.owner_slack_user_id
    {
        return Err(ApiError::unauthorized());
    }
    Ok(grant)
}

struct SlackReader {
    client: reqwest::Client,
    base: String,
    token: String,
    home: std::path::PathBuf,
}

impl SlackReader {
    fn new(c: &Config, home: &Path) -> ApiResult<Self> {
        // Config validation restricts this endpoint to Slack, or an explicit
        // loopback test server which may only use test credentials.
        c.validate().map_err(|_| unavailable())?;
        let secrets = c.secrets(home).map_err(|_| unavailable())?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| unavailable())?,
            base: c.slack.api_base.clone(),
            token: secrets.bot_token,
            home: home.into(),
        })
    }

    async fn get(&self, method: &str, mut args: Value) -> ApiResult<Value> {
        if !matches!(method, "conversations.history" | "conversations.replies") {
            return self.raw_get(method, args).await;
        }
        args["include_all_metadata"] = true.into();
        let limit = args["limit"].as_u64().unwrap_or(40).min(100) as usize;
        let channel = args["channel"].as_str().unwrap_or("").to_owned();
        let store = Store::open(&self.home)?;
        let mut items = Vec::new();
        let mut removed = 0;
        // Consume whole upstream pages so a continuation cursor never skips
        // real messages. Bound backfill even in a room full of UI controls.
        for page_number in 0..4 {
            args["limit"] = (limit.saturating_sub(items.len()).max(1)).into();
            let mut page = self.raw_get(method, args.clone()).await?;
            let raw = page["messages"].as_array().ok_or_else(unavailable)?;
            for message in raw {
                let referenced_root =
                    method == "conversations.replies" && message["ts"] == args["ts"];
                if !referenced_root
                    && (crate::slack::control_message(message)
                        || crate::slack::known_control(
                            &store,
                            &channel,
                            message["ts"].as_str().unwrap_or(""),
                        )?)
                {
                    removed += 1;
                } else {
                    items.push(message.clone());
                }
            }
            let cursor = next_cursor(&page);
            if items.len() >= limit || cursor.is_empty() || page_number == 3 {
                page["messages"] = items.into();
                page["filtered_controls"] = removed.into();
                return Ok(page);
            }
            if args["cursor"].as_str() == Some(cursor.as_str()) {
                return Err(unavailable());
            }
            args["cursor"] = cursor.into();
        }
        unreachable!()
    }

    async fn raw_get(&self, method: &str, args: Value) -> ApiResult<Value> {
        let mut response = self
            .client
            .get(format!("{}{method}", self.base))
            .bearer_auth(&self.token)
            .query(&args)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        // Bound even a malformed or unexpected upstream response.
        if response
            .content_length()
            .is_some_and(|size| size > 2_000_000)
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len() + chunk.len() > 2_000_000 {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        if value["ok"] != true {
            return Err(match value["error"].as_str() {
                Some(
                    "channel_not_found"
                    | "not_in_channel"
                    | "access_denied"
                    | "user_not_in_channel",
                ) => ApiError::forbidden(),
                _ => unavailable(),
            });
        }
        Ok(value)
    }

    async fn conversation(
        &self,
        c: &Config,
        channel: &str,
        actor: &str,
        owner: bool,
    ) -> ApiResult<Conversation> {
        if !identifier(channel) || !identifier(actor) || !c.allows(channel, "") {
            return Err(ApiError::forbidden());
        }
        let result = self
            .get("conversations.info", json!({"channel":channel}))
            .await?;
        let info = &result["channel"];
        if info["id"].as_str() != Some(channel) {
            return Err(unavailable());
        }
        let direct = info["is_im"] == true && info["is_mpim"] != true;
        if !c.allows(channel, if direct { "im" } else { "channel" }) {
            return Err(ApiError::forbidden());
        }
        let other = info["user"].as_str().map(str::to_owned);
        // A DM's audience is established from Slack metadata, never its ID prefix.
        if direct && other.as_deref() != Some(actor) {
            return Err(ApiError::forbidden());
        }
        if owner && direct && other.as_deref() != Some(c.context.owner_slack_user_id.as_str()) {
            return Err(ApiError::forbidden());
        }
        let mut cursor = String::new();
        let mut found_actor = false;
        let mut found_bot = false;
        for _ in 0..MAX_MEMBER_PAGES {
            let mut args = json!({"channel":channel,"limit":200});
            if !cursor.is_empty() {
                args["cursor"] = cursor.clone().into();
            }
            let members = self.get("conversations.members", args).await?;
            let ids = members["members"].as_array().ok_or_else(unavailable)?;
            found_actor |= ids.iter().any(|value| value.as_str() == Some(actor));
            found_bot |= ids
                .iter()
                .any(|value| value.as_str() == Some(c.slack.bot_user_id.as_str()));
            if found_actor && found_bot {
                break;
            }
            cursor = next_cursor(&members);
            if cursor.is_empty() {
                break;
            }
        }
        if !found_actor || !found_bot {
            return Err(ApiError::forbidden());
        }
        let label = if direct {
            "Direct conversation".into()
        } else {
            info["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(200).collect())
                .unwrap_or_else(|| {
                    if info["is_mpim"] == true {
                        "Group direct conversation".into()
                    } else {
                        channel.into()
                    }
                })
        };
        Ok(Conversation {
            id: channel.into(),
            label,
            direct,
            other_user: other,
        })
    }

    async fn authorize_grant(&self, c: &Config, grant: &Grant) -> ApiResult<Authority> {
        let origin = self
            .conversation(c, &grant.channel, &grant.actor, false)
            .await?;
        let owner = !grant.actor_bot
            && grant.actor == c.context.owner_slack_user_id
            && origin.direct
            && origin.other_user.as_deref() == Some(c.context.owner_slack_user_id.as_str());
        Ok(Authority {
            owner,
            actor: grant.actor.clone(),
            origin: Some(origin),
            thread: grant.thread.clone(),
        })
    }
}

fn next_cursor(value: &Value) -> String {
    value["response_metadata"]["next_cursor"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(2_000)
        .collect()
}

fn source_url(c: &Config, channel: &str, message: Option<&str>) -> String {
    let mut url = format!("https://app.slack.com/client/{}/{channel}", c.slack.team_id);
    if let Some(ts) = message {
        url.push_str(&format!("?message_ts={ts}"));
    }
    url
}

fn summary(c: &Config, conversation: &Conversation) -> Value {
    json!({
        "id":conversation.id,"conversation_id":conversation.id,"label":conversation.label,
        "surface":"slack","conversation_kind":if conversation.direct {"direct"} else {"group"},
        "url":source_url(c,&conversation.id,None),"provider_id":format!("slack:{}",c.id)
    })
}

fn authorized_value(c: &Config, auth: &Authority) -> Value {
    json!({
        "owner":auth.owner,"actor_id":auth.actor,"surface":"slack",
        "provider_id":format!("slack:{}",c.id),
        "conversation_id":auth.origin.as_ref().map(|v| &v.id),
        "thread_id":if auth.thread.is_empty() {None} else {Some(&auth.thread)},
        "scope":if auth.owner {"owner_shared_membership"} else if c.context.thread_only_grants && !auth.thread.is_empty() {"thread"} else {"conversation"},
        "label":auth.origin.as_ref().map(|v| &v.label),
        "conversation_kind":auth.origin.as_ref().map(|v| if v.direct {"direct"} else {"group"}),
        "observed_at":now_ms()
    })
}

async fn authority(
    c: &Config,
    home: &Path,
    slack: &SlackReader,
    access: &Access,
) -> ApiResult<Authority> {
    match access {
        Access::Owner => {
            if c.context.owner_slack_user_id.is_empty() {
                return Err(ApiError::forbidden());
            }
            Ok(Authority {
                owner: true,
                actor: c.context.owner_slack_user_id.clone(),
                origin: None,
                thread: String::new(),
            })
        }
        Access::Grant { grant } => slack.authorize_grant(c, &load_grant(c, home, grant)?).await,
    }
}

async fn target(
    c: &Config,
    slack: &SlackReader,
    auth: &Authority,
    requested: Option<&str>,
) -> ApiResult<Conversation> {
    let channel = requested
        .or_else(|| auth.origin.as_ref().map(|v| v.id.as_str()))
        .ok_or_else(|| ApiError::bad("Choose a conversation_id"))?;
    if !auth.owner && auth.origin.as_ref().map(|v| v.id.as_str()) != Some(channel) {
        return Err(ApiError::forbidden());
    }
    if let Some(origin) = &auth.origin
        && origin.id == channel
    {
        return Ok(origin.clone());
    }
    slack
        .conversation(c, channel, &auth.actor, auth.owner)
        .await
}

async fn list(
    c: &Config,
    slack: &SlackReader,
    auth: &Authority,
    request: &Query,
) -> ApiResult<Value> {
    if !auth.owner {
        let origin = auth.origin.as_ref().ok_or_else(ApiError::forbidden)?;
        return Ok(
            json!({"items":[summary(c,origin)],"complete":true,"next_cursor":null,
            "coverage":{"scope":"grant_conversation","checked":1},"observed_at":now_ms()}),
        );
    }
    let limit = request.limit.unwrap_or(10).clamp(1, 30);
    let mut cursor = request.cursor.clone().unwrap_or_default();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut checked = 0usize;
    let mut complete = false;
    for _ in 0..MAX_LIST_PAGES {
        let page_limit = (limit - items.len()).min(10);
        let mut args = json!({"types":"public_channel,private_channel,im,mpim","limit":page_limit,"exclude_archived":false});
        if !cursor.is_empty() {
            args["cursor"] = cursor.clone().into();
        }
        let page = slack.get("conversations.list", args).await?;
        let channels = page["channels"].as_array().ok_or_else(unavailable)?;
        if channels.len() > page_limit {
            return Err(unavailable());
        }
        for info in channels {
            let Some(id) = info["id"].as_str() else {
                return Err(unavailable());
            };
            if !seen.insert(id.to_owned()) {
                continue;
            }
            checked += 1;
            match slack.conversation(c, id, &auth.actor, true).await {
                Ok(conversation) => items.push(summary(c, &conversation)),
                Err(error) if error.0 == StatusCode::FORBIDDEN => {}
                Err(error) => return Err(error),
            }
        }
        let next = next_cursor(&page);
        complete = next.is_empty() && page["has_more"] != true;
        if next == cursor && !next.is_empty() {
            return Err(unavailable());
        }
        cursor = next;
        if complete || cursor.is_empty() || items.len() >= limit {
            break;
        }
    }
    Ok(
        json!({"items":items,"complete":complete,"next_cursor":if cursor.is_empty(){None}else{Some(cursor)},
        "coverage":{"scope":"owner_and_agent_membership","checked":checked,"bounded":true},"observed_at":now_ms()}),
    )
}

fn messages(
    c: &Config,
    channel: &str,
    page: &Value,
    limit: usize,
) -> ApiResult<(Vec<Value>, bool)> {
    let mut budget = MAX_TEXT_CHARS;
    let mut truncated = false;
    let raw = page["messages"].as_array().ok_or_else(unavailable)?;
    let mut result = Vec::new();
    for message in raw.iter().take(limit) {
        let Some(ts) = message["ts"].as_str().filter(|s| timestamp(s)) else {
            continue;
        };
        let content = message["text"].as_str().unwrap_or("");
        let text: String = content
            .chars()
            .take(budget.min(MAX_MESSAGE_CHARS))
            .collect();
        let text_truncated = text.chars().count() < content.chars().count();
        budget -= text.chars().count();
        truncated |= text_truncated;
        let actor = message["user"]
            .as_str()
            .or_else(|| message["bot_id"].as_str())
            .unwrap_or("unknown");
        let thread = message["thread_ts"].as_str().filter(|s| timestamp(s));
        result.push(json!({"id":ts,"message_id":ts,"conversation_id":channel,"text":text,
            "actor":actor,"actor_id":actor,"at":ts,"url":source_url(c,channel,Some(ts)),
            "thread_id":thread,"reply_count":message["reply_count"].as_u64().unwrap_or(0),
            "latest_reply":message["latest_reply"].as_str().filter(|s|timestamp(s)),"truncated":text_truncated,
            "reactions":crate::reactions::projection(message),"connector_notice":crate::slack::control_message(message)}));
    }
    truncated |= raw.len() > limit;
    result.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
    Ok((result, truncated))
}

async fn read_page(
    c: &Config,
    slack: &SlackReader,
    conversation: &Conversation,
    thread: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
    latest: Option<&str>,
) -> ApiResult<Value> {
    let limit = limit.clamp(1, MAX_MESSAGES);
    let mut args = json!({"channel":conversation.id,"limit":limit});
    let method = if let Some(thread) = thread {
        if !timestamp(thread) {
            return Err(ApiError::bad("Invalid thread_id"));
        }
        args["ts"] = thread.into();
        "conversations.replies"
    } else {
        "conversations.history"
    };
    if let Some(cursor) = cursor.filter(|s| !s.is_empty()) {
        args["cursor"] = cursor.into();
    }
    if let Some(latest) = latest {
        args["latest"] = latest.into();
        args["inclusive"] = true.into();
    }
    let page = slack.get(method, args).await?;
    let (messages, truncated) = messages(c, &conversation.id, &page, limit)?;
    let cursor = next_cursor(&page);
    let root = if let Some(thread) = thread {
        if let Some(root) = messages.iter().find(|m| m["id"] == thread) {
            root.clone()
        } else {
            thread_root(c, slack, &conversation.id, thread).await
        }
    } else {
        Value::Null
    };
    Ok(
        json!({"root":root,"conversation_id":conversation.id,"thread_id":thread,"label":conversation.label,
        "messages":messages,"complete":cursor.is_empty()&&page["has_more"]!=true&&!truncated,
        "next_cursor":if cursor.is_empty(){None}else{Some(cursor)},"observed_at":now_ms(),
        "coverage":{"scope":if thread.is_some(){"thread"}else{"conversation_roots"},"bounded":true,
            "text_truncated":truncated,"requested_limit":limit,"has_more":page["has_more"]==true}}),
    )
}

async fn thread_root(c: &Config, slack: &SlackReader, channel: &str, thread: &str) -> Value {
    let result = slack
        .get(
            "conversations.replies",
            json!({"channel":channel,"ts":thread,"limit":1}),
        )
        .await;
    if let Ok(page) = result
        && let Ok((items, _)) = messages(c, channel, &page, 1)
        && let Some(item) = items.into_iter().find(|m| m["id"] == thread)
    {
        return item;
    }
    json!({"id":thread,"available":false,"url":source_url(c,channel,Some(thread)),"notice":"Thread root unavailable; no text was assumed."})
}

fn thread_index(pages: &[&Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for page in pages {
        for item in page["messages"].as_array().into_iter().flatten() {
            if item["reply_count"].as_u64().unwrap_or(0) == 0 {
                continue;
            }
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            if !seen.insert(id.to_owned()) {
                continue;
            }
            result.push(json!({"thread_id":id,"root_message_id":id,"reply_count":item["reply_count"],
                "latest_reply":item["latest_reply"],"url":item["url"],
                "preview":item["text"].as_str().unwrap_or("").chars().take(180).collect::<String>()}));
        }
    }
    result
}

// Stored timestamps are discovery hints only. Content is re-read from Slack
// after the current audience has been authorized, so deleted or edited replies
// are never resurrected from our local message cache.
fn observed_thread_hints(
    c: &Config,
    home: &Path,
    channel: &str,
) -> ApiResult<Vec<(String, String, String)>> {
    let store = Store::open(home)?;
    let mut statement = store.db.prepare(
        "SELECT json_extract(body,'$.thread'),ts FROM messages
         WHERE channel=?1 AND json_extract(body,'$.team')=?2
           AND json_extract(body,'$.thread')!='' AND json_extract(body,'$.deleted')=0
         ORDER BY ts DESC LIMIT 120",
    )?;
    let rows = statement.query_map(params![channel, c.slack.team_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut hints: Vec<(String, String, String)> = Vec::new();
    let mut counts = Vec::new();
    for row in rows {
        let (thread, ts) = row?;
        if !timestamp(&thread) || !timestamp(&ts) {
            continue;
        }
        if let Some(index) = hints.iter().position(|h| h.0 == thread) {
            if counts[index] < 4 {
                hints[index].1 = ts;
                counts[index] += 1;
            }
        } else if hints.len() < 3 {
            hints.push((thread, ts.clone(), ts));
            counts.push(1);
        }
    }
    Ok(hints)
}

async fn recent_thread_activity(
    c: &Config,
    home: &Path,
    slack: &SlackReader,
    conversation: &Conversation,
    recent: &Value,
) -> ApiResult<Value> {
    let mut hints = observed_thread_hints(c, home, &conversation.id)?;
    for root in thread_index(&[recent]) {
        let (Some(thread), Some(latest)) =
            (root["thread_id"].as_str(), root["latest_reply"].as_str())
        else {
            continue;
        };
        if let Some(hint) = hints.iter_mut().find(|h| h.0 == thread) {
            // A newer server-side reply may be beyond our observed event window.
            if latest > hint.2.as_str() {
                hint.1 = latest.into();
                hint.2 = latest.into();
            }
        } else {
            hints.push((thread.into(), latest.into(), latest.into()));
        }
    }
    hints.sort_by(|a, b| b.2.cmp(&a.2));
    hints.truncate(3);
    let mut windows = Vec::new();
    let mut budget: usize = 6000;
    for (thread, oldest, _) in hints {
        let page = tokio::time::timeout(Duration::from_secs(2), slack.get("conversations.replies", json!({"channel":conversation.id,"ts":thread,"oldest":oldest,"inclusive":true,"limit":4}))).await.unwrap_or_else(|_| Err(unavailable()));
        let root = thread_root(c, slack, &conversation.id, &thread).await;
        let window = match page {
            Ok(mut page) => {
                // Slack can include the parent in addition to the requested reply
                // limit, even when it predates oldest. Do not let that extra root
                // displace the newest reply from our bounded window.
                if let Some(raw) = page["messages"].as_array_mut() {
                    raw.retain(|message| {
                        message["ts"]
                            .as_str()
                            .is_some_and(|ts| ts != thread && ts >= oldest.as_str())
                    });
                }
                let (mut items, mut truncated) = messages(c, &conversation.id, &page, 4)?;
                for item in &mut items {
                    let original = item["text"].as_str().unwrap_or("");
                    let text: String = original.chars().take(budget.min(1000)).collect();
                    if text.chars().count() < original.chars().count() {
                        item["truncated"] = true.into();
                        truncated = true;
                    }
                    budget -= text.chars().count();
                    item["text"] = text.into();
                }
                json!({"root":root,"thread_id":thread,"url":source_url(c,&conversation.id,Some(&thread)),"messages":items,
                    "complete":false,"next_cursor":next_cursor(&page),"available":true,
                    "coverage":{"scope":"recent_thread_window","oldest":oldest,"has_more":page["has_more"]==true,"text_truncated":truncated}})
            }
            Err(_) => {
                json!({"thread_id":thread,"available":false,"complete":false,"messages":[],"notice":"Recent thread activity could not be refreshed; no cached content was substituted."})
            }
        };
        windows.push(window);
    }
    Ok(
        json!({"items":windows,"complete":false,"coverage":{"scope":"recent_known_threads_in_this_conversation","max_threads":3,"max_messages_per_thread":4,"max_text_chars":6000,"unknown_older_threads_included":false}}),
    )
}

async fn orientation(
    c: &Config,
    home: &Path,
    slack: &SlackReader,
    auth: &Authority,
    conversation: &Conversation,
    thread: Option<&str>,
) -> ApiResult<Value> {
    let recent = read_page(c, slack, conversation, None, 10, None, None).await?;
    let mut value = summary(c, conversation);
    value["actor_id"] = auth.actor.clone().into();
    value["owner"] = auth.owner.into();
    value["thread_id"] = json!(thread);
    value["observed_at"] = now_ms().into();
    value["complete"] = false.into();
    value["next_cursor"] = Value::Null;
    value["coverage"] = json!({"scope":"current_conversation_orientation","bounded":true,"other_conversations_read":false});
    value["recent_messages"] = recent["messages"].clone();
    value["recent_coverage"] = recent["coverage"].clone();
    value["recent_next_cursor"] = recent["next_cursor"].clone();
    value["threads"] = json!(thread_index(&[&recent]));
    if let Some(thread) = thread {
        let around = read_page(c, slack, conversation, None, 6, None, Some(thread)).await?;
        let replies = read_page(c, slack, conversation, Some(thread), 12, None, None).await?;
        value["parent_context"] = json!({"conversation_id":conversation.id,"label":conversation.label,
            "before_and_at_root":around["messages"],"before_root_next_cursor":around["next_cursor"],
            "recent_messages":recent["messages"],"recent_next_cursor":recent["next_cursor"],"complete":false});
        value.as_object_mut().unwrap().remove("recent_messages");
        value["threads"] = json!(thread_index(&[&recent, &around]));
        value["thread"] = replies;
    } else {
        value["recent_thread_activity"] =
            recent_thread_activity(c, home, slack, conversation, &recent).await?;
    }
    Ok(value)
}

fn parse_query(value: Value) -> ApiResult<Query> {
    let query: Query =
        serde_json::from_value(value).map_err(|_| ApiError::bad("Invalid context request"))?;
    if !["authorize", "list", "read", "orientation", "search"].contains(&query.operation.as_str())
        || query
            .conversation_id
            .as_deref()
            .is_some_and(|s| !identifier(s))
        || query.thread_id.as_deref().is_some_and(|s| !timestamp(s))
        || query.cursor.as_ref().is_some_and(|s| s.len() > 2000)
        || query.limit.is_some_and(|n| n == 0 || n > 200)
        || query
            .query
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.chars().count() > 200)
        || (query.operation == "search" && query.query.is_none())
        || (query.operation != "search" && query.query.is_some())
    {
        return Err(ApiError::bad("Invalid context operation or bounds"));
    }
    Ok(query)
}

async fn execute(c: &Config, home: &Path, request: Query) -> ApiResult<Value> {
    enabled(c)?;
    let slack = SlackReader::new(c, home)?;
    let auth = authority(c, home, &slack, &request.access).await?;
    dispatch(c, home, &slack, &auth, request).await
}

async fn dispatch(
    c: &Config,
    home: &Path,
    slack: &SlackReader,
    auth: &Authority,
    request: Query,
) -> ApiResult<Value> {
    match request.operation.as_str() {
        "authorize" => Ok(authorized_value(c, auth)),
        "list" => list(c, slack, auth, &request).await,
        "read" | "orientation" | "search" => {
            let conversation = target(c, slack, auth, request.conversation_id.as_deref()).await?;
            let thread = request.thread_id.as_deref().or_else(|| {
                (request.conversation_id.is_none() && !auth.thread.is_empty())
                    .then_some(auth.thread.as_str())
            });
            let thread_only =
                !auth.owner && c.context.thread_only_grants && !auth.thread.is_empty();
            if thread_only && thread != Some(auth.thread.as_str()) {
                return Err(ApiError::forbidden());
            }
            if request.operation == "orientation" && !thread_only {
                orientation(c, home, slack, auth, &conversation, thread).await
            } else {
                let mut page = read_page(
                    c,
                    slack,
                    &conversation,
                    thread,
                    request.limit.unwrap_or(20),
                    request.cursor.as_deref(),
                    None,
                )
                .await?;
                if let Some(query) = request.query {
                    let needle = query.to_lowercase();
                    let messages = page["messages"].as_array().ok_or_else(unavailable)?;
                    let scanned = messages.len();
                    let matches: Vec<_> = messages
                        .iter()
                        .filter(|message| {
                            message["text"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&needle)
                        })
                        .cloned()
                        .collect();
                    page["messages"] = json!(matches);
                    // Keep the source cursor and truncation evidence: an empty
                    // match set is not proof that the whole archive was searched.
                    page["coverage"]["search"] = json!({
                        "mode":"literal_case_insensitive", "scanned_messages":scanned,
                        "bounded_window":true, "exhaustive_archive":false,
                        "matches_returned_text_only":true
                    });
                }
                Ok(page)
            }
        }
        _ => Err(ApiError::bad("Unknown context operation")),
    }
}

/// Authenticated management-provider API. Browser cookies are deliberately not accepted.
pub async fn query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    if !headers.contains_key(header::AUTHORIZATION) {
        return Err(ApiError::unauthorized());
    }
    auth::check(&state, &headers, "context", true)?;
    let request = parse_query(body)?;
    let config = state.instance_config();
    let result = tokio::time::timeout(
        Duration::from_secs(45),
        execute(&config, &state.home, request),
    )
    .await
    .map_err(|_| unavailable())??;
    Ok(Json(result))
}

/// Orientation for the runtime's already-observed trigger, without issuing or exposing a grant.
pub async fn orientation_for(c: &Config, home: &Path, input: &Value) -> Result<Value> {
    let grant = grant_from_input(c, input)?;
    let result = async {
        let slack = SlackReader::new(c, home)?;
        let auth = slack.authorize_grant(c, &grant).await?;
        let conversation = auth.origin.as_ref().ok_or_else(ApiError::forbidden)?;
        if c.context.thread_only_grants && !auth.owner && !grant.thread.is_empty() {
            return read_page(c, &slack, conversation, Some(&grant.thread), 12, None, None).await;
        }
        orientation(
            c,
            home,
            &slack,
            &auth,
            conversation,
            if grant.thread.is_empty() {
                None
            } else {
                Some(grant.thread.as_str())
            },
        )
        .await
    };
    tokio::time::timeout(Duration::from_secs(25), result)
        .await
        .map_err(|_| anyhow::anyhow!("Slack orientation is unavailable"))?
        .map_err(|_| anyhow::anyhow!("Slack orientation is unavailable"))
}

fn mcp_tools() -> Value {
    let properties = json!({"conversation_id":{"type":"string","description":"Slack conversation ID from discovery or the current conversation"},
        "thread_id":{"type":"string","description":"Optional Slack root-message timestamp"},"limit":{"type":"integer","minimum":1,"maximum":40},
        "cursor":{"type":"string","description":"Opaque next_cursor from a previous result"}});
    let mut tools:Vec<Value> = [
        ("slack_context_list","Find conversations this identity and audience may access; never publishes messages."),
        ("slack_context_read","Read a bounded conversation or thread with source references; never publishes messages."),
        ("slack_context_orientation","Inspect the relationship between this thread and its surrounding conversation."),
        ("slack_context_search","Find case-insensitive literal text in one authorized conversation or thread window. Follow next_cursor for older windows; no exhaustive archive search is assumed."),
    ].iter().map(|(name,description)| {
        let mut schema = json!({"type":"object","properties":properties,"additionalProperties":false});
        if *name == "slack_context_search" {
            schema["properties"]["query"] = json!({"type":"string","minLength":1,"maxLength":200,
                "description":"Literal text to match, case-insensitively; no regular expressions or Slack search operators"});
            schema["required"] = json!(["query"]);
        }
        json!({"name":name,"description":description,"inputSchema":schema,
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":true}})
    }).collect();
    for (name, description, read_only) in [
        (
            "slack_reactions_get",
            "Read current reactions on a message in this conversation; listed actors may be incomplete.",
            true,
        ),
        (
            "slack_reactions_add",
            "Add this agent's own emoji reaction to a message in this conversation. A reaction is not approval or a work request.",
            false,
        ),
        (
            "slack_reactions_remove",
            "Remove this agent's own emoji reaction from a message in this conversation.",
            false,
        ),
    ] {
        let mut schema = json!({"type":"object","properties":{"message_id":{"type":"string"},"conversation_id":{"type":"string"}},"required":["message_id"],"additionalProperties":false});
        if !read_only {
            schema["properties"]["name"] = json!({"type":"string","description":"Emoji name without wrapping colons, for example thumbsup"});
            schema["required"] = json!(["message_id", "name"]);
        }
        tools.push(json!({"name":name,"description":description,"inputSchema":schema,"annotations":{"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}}));
    }
    json!({"tools":tools})
}

fn rpc(id: Value, result: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}

/// Stateless MCP Streamable HTTP with JSON responses. Grants are the only credential.
pub async fn mcp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    auth::same_origin(&headers)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let config = state.instance_config();
    enabled(&config)?;
    let grant = load_grant(&config, &state.home, token)?;
    let slack = SlackReader::new(&config, &state.home)?;
    // Even initialize and tools/list must fail if the granting audience changed.
    let authority = tokio::time::timeout(
        Duration::from_secs(25),
        slack.authorize_grant(&config, &grant),
    )
    .await
    .map_err(|_| unavailable())??;
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    if body["jsonrpc"] != "2.0"
        || !body["method"].is_string()
        || !(id.is_null() || id.is_string() || id.is_number())
    {
        return Ok(rpc_error(id, -32600, "Invalid JSON-RPC request"));
    }
    let method = body["method"].as_str().unwrap_or("");
    if body.get("id").is_none() {
        return Ok(StatusCode::ACCEPTED.into_response());
    }
    match method {
        "initialize" => {
            let requested = body["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = if ["2024-11-05", "2025-03-26", "2025-06-18"].contains(&requested) {
                requested
            } else {
                "2025-06-18"
            };
            Ok(rpc(
                id,
                json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"slack-conversations","version":env!("CARGO_PKG_VERSION")}}),
            ))
        }
        "ping" => Ok(rpc(id, json!({}))),
        "tools/list" => {
            let mut result = mcp_tools();
            if authority.owner && authority.origin.as_ref().is_some_and(|c| c.direct) {
                for (name, description) in [
                    (
                        "slack_connector_update_status",
                        "Inspect this connector’s fixed-source update plan and status.",
                    ),
                    (
                        "slack_connector_update_plan",
                        "Prepare a verified update from this connector’s trusted Microsoft main build. No deployment yet.",
                    ),
                    (
                        "slack_connector_update_apply",
                        "Queue this prepared update after current work and replies drain; preserve configuration and sessions.",
                    ),
                    (
                        "slack_connector_update_rollback",
                        "Queue rollback of this connector’s last successful update after current work drains.",
                    ),
                ] {
                    if !name.ends_with("status")
                        && !crate::updates::agent_allowed(
                            &state.home,
                            &config,
                            &authority.actor,
                            true,
                        )
                    {
                        continue;
                    }
                    let props = if name.ends_with("apply") || name.ends_with("rollback") {
                        json!({"plan_id":{"type":"string","format":"uuid"}})
                    } else {
                        json!({})
                    };
                    result["tools"].as_array_mut().unwrap().push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":props,"required":if name.ends_with("apply") || name.ends_with("rollback") { json!(["plan_id"]) } else { json!([]) },"additionalProperties":false},"annotations":{"readOnlyHint":name.ends_with("status"),"destructiveHint":false,"openWorldHint":true}}));
                }
            }
            Ok(rpc(id, result))
        }
        "tools/call" => {
            if let Some(name) = body["params"]["name"]
                .as_str()
                .filter(|n| n.starts_with("slack_reactions_"))
            {
                return reaction_tool(
                    &config,
                    &slack,
                    &authority,
                    id,
                    name,
                    &body["params"]["arguments"],
                )
                .await;
            }
            if let Some(name) = body["params"]["name"]
                .as_str()
                .filter(|n| n.starts_with("slack_connector_update_"))
            {
                if !(authority.owner && authority.origin.as_ref().is_some_and(|c| c.direct))
                    || (name != "slack_connector_update_status"
                        && !crate::updates::agent_allowed(
                            &state.home,
                            &config,
                            &authority.actor,
                            true,
                        ))
                {
                    return Err(ApiError::forbidden());
                }
                let args = body["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                let Some(object) = args.as_object() else {
                    return Ok(rpc_error(id, -32602, "Invalid update arguments"));
                };
                let operation = name.trim_start_matches("slack_connector_update_");
                if object.keys().any(|k| k != "plan_id")
                    || (!["apply", "rollback"].contains(&operation) && !object.is_empty())
                {
                    return Ok(rpc_error(id, -32602, "Invalid update arguments"));
                }
                let value = if operation == "status" {
                    crate::updates::status(&state.home).map_err(ApiError::from)
                } else {
                    crate::updates::queue(
                        &state.home,
                        crate::updates::Command {
                            action: operation.into(),
                            plan_id: args["plan_id"].as_str().map(str::to_owned),
                        },
                        true,
                    )
                }?;
                return Ok(rpc(
                    id,
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false}),
                ));
            }
            let operation = match body["params"]["name"].as_str() {
                Some("slack_context_list") => "list",
                Some("slack_context_read") => "read",
                Some("slack_context_orientation") => "orientation",
                Some("slack_context_search") => "search",
                _ => return Ok(rpc_error(id, -32602, "Unknown context tool")),
            };
            let mut args = body["params"]
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            let Some(object) = args.as_object_mut() else {
                return Ok(rpc_error(id, -32602, "Invalid tool arguments"));
            };
            if object.keys().any(|key| {
                !["conversation_id", "thread_id", "limit", "cursor", "query"]
                    .contains(&key.as_str())
            }) {
                return Ok(rpc_error(id, -32602, "Invalid tool arguments"));
            }
            object.insert("operation".into(), operation.into());
            object.insert("access".into(), json!({"mode":"grant","grant":token}));
            let request = match parse_query(args) {
                Ok(request) => request,
                Err(_) => return Ok(rpc_error(id, -32602, "Invalid tool arguments")),
            };
            match tokio::time::timeout(
                Duration::from_secs(45),
                dispatch(&config, &state.home, &slack, &authority, request),
            )
            .await
            {
                Ok(Ok(value)) => Ok(rpc(
                    id,
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false}),
                )),
                _ => Ok(rpc(
                    id,
                    json!({"content":[{"type":"text","text":"Context is unavailable or outside this conversation's access. No content was assumed."}],"isError":true}),
                )),
            }
        }
        _ => Ok(rpc_error(id, -32601, "Method not found")),
    }
}

async fn reaction_tool(
    c: &Config,
    slack: &SlackReader,
    auth: &Authority,
    id: Value,
    name: &str,
    args: &Value,
) -> ApiResult<Response> {
    let operation = match name {
        "slack_reactions_get" => "get",
        "slack_reactions_add" => "add",
        "slack_reactions_remove" => "remove",
        _ => return Ok(rpc_error(id, -32602, "Unknown reaction tool")),
    };
    let Some(object) = args.as_object() else {
        return Ok(rpc_error(id, -32602, "Invalid reaction arguments"));
    };
    if object
        .keys()
        .any(|k| !["message_id", "conversation_id", "name"].contains(&k.as_str()))
    {
        return Ok(rpc_error(id, -32602, "Invalid reaction arguments"));
    }
    let Some(ts) = args["message_id"].as_str().filter(|s| timestamp(s)) else {
        return Ok(rpc_error(id, -32602, "Invalid message ID"));
    };
    let emoji = args["name"].as_str().unwrap_or("");
    if operation != "get" && !crate::reactions::name(emoji) {
        return Ok(rpc_error(id, -32602, "Invalid emoji name"));
    }
    let Some(origin) = auth.origin.as_ref() else {
        return Err(ApiError::forbidden());
    };
    let channel = args["conversation_id"].as_str().unwrap_or(&origin.id);
    if channel != origin.id {
        return Err(ApiError::forbidden());
    }
    let conversation = target(c, slack, auth, Some(channel)).await?;
    let snapshot = match slack
        .raw_get(
            "reactions.get",
            json!({"channel":channel,"timestamp":ts,"full":true}),
        )
        .await
    {
        Ok(v) => v,
        Err(_) => {
            return Ok(rpc(
                id,
                json!({"isError":true,"content":[{"type":"text","text":"Reaction access is unavailable. This Slack app needs reactions:read and reactions:write; reconnect its installation if these permissions have not been approved."}]}),
            ));
        }
    };
    if snapshot["message"]["ts"] != ts {
        return Err(unavailable());
    }
    if c.context.thread_only_grants
        && !auth.owner
        && !auth.thread.is_empty()
        && ts != auth.thread
        && snapshot["message"]["thread_ts"] != auth.thread
    {
        return Err(ApiError::forbidden());
    }
    if operation == "get" {
        let result = json!({"conversation_id":conversation.id,"message_id":ts,"reactions":crate::reactions::projection(&snapshot["message"]),"observed_at":now_ms()});
        return Ok(rpc(
            id,
            json!({"structuredContent":result,"content":[{"type":"text","text":result.to_string()}],"isError":false}),
        ));
    }
    // Recheck audience immediately before the side effect. Never impersonate
    // another user and never automatically retry an uncertain write.
    target(c, slack, auth, Some(channel)).await?;
    let response = slack
        .client
        .post(format!("{}reactions.{operation}", slack.base))
        .bearer_auth(&slack.token)
        .json(&json!({"channel":channel,"timestamp":ts,"name":emoji}))
        .send()
        .await;
    let mut response = match response {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(reaction_unknown(id)),
    };
    let mut data = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if data.len() + chunk.len() <= 65536 => data.extend_from_slice(&chunk),
            Ok(None) => break,
            _ => return Ok(reaction_unknown(id)),
        }
    }
    let result: Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(_) => return Ok(reaction_unknown(id)),
    };
    let success = result["ok"] == true
        || operation == "add" && result["error"] == "already_reacted"
        || operation == "remove" && result["error"] == "no_reaction";
    let value = json!({"conversation_id":channel,"message_id":ts,"name":emoji,"operation":operation,"outcome":if success{"confirmed"}else{"rejected"},"reason":if success{"completed"}else if result["error"]=="missing_scope"{"reconnect_required"}else{"slack_rejected"}});
    Ok(rpc(
        id,
        json!({"structuredContent":value,"content":[{"type":"text","text":value.to_string()}],"isError":!success}),
    ))
}
fn reaction_unknown(id: Value) -> Response {
    rpc(
        id,
        json!({"isError":true,"content":[{"type":"text","text":"Reaction outcome is unknown; it was not retried. Read current reactions before deciding whether another action is needed."}]}),
    )
}
