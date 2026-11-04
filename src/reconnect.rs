//! Recover previously unseen human messages after a verified Socket Mode reconnect.
use crate::{
    config::Config,
    model::now_ms,
    slack::{self, Slack},
    store::Store,
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

pub fn checkpoint(s: &Store) -> Result<i64> {
    let old: Option<String> =
        s.db.query_row(
            "SELECT value FROM settings WHERE key='socket_checkpoint'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        return Ok((old.parse::<i64>()? - 30_000).max(now_ms() - 86400000));
    }
    // A newly installed connection starts here, never at arbitrary old history.
    let now = now_ms();
    advance(s, now)?;
    Ok(now)
}
pub fn advance(s: &Store, at: i64) -> Result<()> {
    s.db.execute("INSERT INTO settings(key,value) VALUES('socket_checkpoint',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[at.to_string()])?;
    Ok(())
}
pub async fn recover(
    client: &Slack,
    c: &Config,
    s: Arc<Mutex<Store>>,
    since: i64,
) -> Result<usize> {
    let until = now_ms();
    let mut cursor = String::new();
    let mut rooms = Vec::new();
    // Bounded pagination. A limit/failure retains the checkpoint for retry.
    for page in 0..10 {
        let data=client.call("users.conversations",json!({"types":"im,mpim,public_channel,private_channel","exclude_archived":true,"limit":100,"cursor":cursor}),false).await.map_err(recovery_error)?;
        rooms.extend(data["channels"].as_array().into_iter().flatten().cloned());
        cursor = data["response_metadata"]["next_cursor"]
            .as_str()
            .unwrap_or("")
            .into();
        if cursor.is_empty() {
            break;
        }
        ensure!(
            page < 9,
            "Conversation recovery page limit reached; checkpoint retained"
        );
    }
    let mut recovered = 0;
    for room in rooms {
        let Some(channel) = room["id"].as_str() else {
            continue;
        };
        let kind = if room["is_im"] == true {
            "im"
        } else if room["is_mpim"] == true {
            "mpim"
        } else {
            "channel"
        };
        if !c.allows(channel, kind) {
            continue;
        }
        let mut cursor = String::new();
        let mut messages = Vec::<Value>::new();
        for page in 0..10 {
            let data=client.call("conversations.history",json!({"channel":channel,"oldest":format!("{:.3}",since as f64/1000.0),"latest":format!("{:.3}",until as f64/1000.0),"inclusive":true,"include_all_metadata":true,"limit":100,"cursor":cursor}),false).await.map_err(recovery_error)?;
            messages.extend(data["messages"].as_array().into_iter().flatten().cloned());
            cursor = data["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or("")
                .into();
            if cursor.is_empty() {
                ensure!(
                    data["has_more"] != true,
                    "Incomplete history pagination; checkpoint retained"
                );
                break;
            }
            ensure!(
                page < 9,
                "History recovery page limit reached; checkpoint retained"
            );
        }
        // Slack history contains parent messages, not their thread replies. Revisit
        // known threads even when their root predates this outage, plus threads
        // discovered in the recovered parent page. Never scan unrelated rooms.
        let mut threads = known_threads(&s.lock().unwrap(), channel)?;
        for m in &messages {
            if let Some(root) = m["thread_ts"].as_str().filter(|v| !v.is_empty()) {
                threads.insert(root.to_owned());
            } else if m["reply_count"].as_u64().unwrap_or(0) > 0
                && let Some(root) = m["ts"].as_str()
            {
                threads.insert(root.to_owned());
            }
        }
        ensure!(
            threads.len() <= 500,
            "Thread recovery limit reached; checkpoint retained"
        );
        for root in threads {
            let mut cursor = String::new();
            for page in 0..10 {
                let data = match client.call("conversations.replies", json!({
                    "channel":channel,"ts":root,"oldest":format!("{:.3}",since as f64/1000.0),
                    "latest":format!("{:.3}",until as f64/1000.0),"inclusive":true,
                    "include_all_metadata":true,"limit":100,"cursor":cursor
                }), false).await {
                    Ok(data) => data,
                    Err(slack::ApiError::Rejected(code)) if code == "thread_not_found" => break,
                    Err(error) => return Err(recovery_error(error)),
                };
                for mut m in data["messages"].as_array().into_iter().flatten().cloned() {
                    // The parent is already represented by history. A reply must
                    // remain in this thread even if the server omits thread_ts.
                    if m["ts"] == root {
                        continue;
                    }
                    m["thread_ts"] = json!(root);
                    messages.push(m);
                }
                cursor = data["response_metadata"]["next_cursor"]
                    .as_str()
                    .unwrap_or("")
                    .into();
                if cursor.is_empty() {
                    ensure!(
                        data["has_more"] != true,
                        "Incomplete thread pagination; checkpoint retained"
                    );
                    break;
                }
                ensure!(
                    page < 9,
                    "Thread recovery page limit reached; checkpoint retained"
                );
            }
        }
        messages.sort_by(|a, b| a["ts"].as_str().cmp(&b["ts"].as_str()));
        for mut m in messages {
            if m.get("bot_id").is_some()
                || m["subtype"] == "bot_message"
                || m["user"] == c.slack.bot_user_id
                || m["metadata"]["event_type"]
                    .as_str()
                    .is_some_and(|v| v.starts_with("acp_"))
            {
                continue;
            }
            let Some(ts) = m["ts"].as_str().map(str::to_owned) else {
                continue;
            };
            let Some(stamp) = ts.parse::<f64>().ok().map(|v| (v * 1000.0) as i64) else {
                continue;
            };
            if stamp < since || stamp > until {
                continue;
            }
            let ctx = crate::model::context_id(
                &c.slack.team_id,
                channel,
                m["thread_ts"].as_str().unwrap_or(""),
            );
            let mut store = s.lock().unwrap();
            let accepted:bool=store.db.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE context=?1 AND json_extract(trigger,'$.ts')>=?2)",params![ctx,ts],|r|r.get(0))?;
            // Any accepted/uncertain/interrupted work is never replayed here.
            if accepted || store.message(channel, &ts)?.is_some() {
                continue;
            }
            m["type"] = json!("message");
            m["channel"] = json!(channel);
            m["channel_type"] = json!(kind);
            slack::receive(
                c,
                &mut store,
                &json!({"event_id":format!("recovery:{channel}:{ts}"),"event":m}),
            )?;
            recovered += 1;
        }
    }
    let store = s.lock().unwrap();
    advance(&store, until)?;
    store.audit("reconnect_recovery",json!({"recovered_messages":recovered,"since":since,"through":until,"scope":"accessible conversations and known/discovered threads; human messages only; no replay of accepted work"}))?;
    Ok(recovered)
}

#[derive(Debug)]
pub struct Retry(pub u64);
impl std::fmt::Display for Retry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Slack history recovery is rate limited; checkpoint retained"
        )
    }
}
impl std::error::Error for Retry {}
fn recovery_error(error: slack::ApiError) -> anyhow::Error {
    match error {
        slack::ApiError::RateLimited(seconds) => Retry(seconds.max(60)).into(),
        _ => anyhow::anyhow!("Slack history recovery unavailable; checkpoint retained"),
    }
}

fn known_threads(s: &Store, channel: &str) -> Result<BTreeSet<String>> {
    let mut q = s.db.prepare("SELECT thread FROM session_bindings WHERE channel=?1 AND thread<>'' UNION SELECT json_extract(body,'$.thread') FROM messages WHERE channel=?1 AND json_extract(body,'$.thread')<>'' LIMIT 501")?;
    let threads = q
        .query_map([channel], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    ensure!(
        threads.len() <= 500,
        "Thread recovery limit reached; checkpoint retained"
    );
    Ok(threads)
}
