//! Human decisions for ACP permissions. Slack controls are bound to one live turn.
use crate::{config::Config, model::now_ms, slack::Slack, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub fn initialize(store: &Store) -> Result<()> {
    store.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS permissions(
        id TEXT PRIMARY KEY, job_id TEXT NOT NULL, context TEXT NOT NULL,
        session_id TEXT NOT NULL, approver TEXT NOT NULL, config_revision INTEGER NOT NULL,
        created INTEGER NOT NULL, expires INTEGER NOT NULL, status TEXT NOT NULL,
        option_id TEXT, actor TEXT, channel TEXT NOT NULL, thread TEXT NOT NULL,
        slack_ts TEXT, shown_status TEXT NOT NULL DEFAULT '', body TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS permissions_status ON permissions(status,expires);",
    )?;
    Ok(())
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"job_id":r.get::<_,String>(1)?,
        "context":r.get::<_,String>(2)?,"session_id":r.get::<_,String>(3)?,"approver":r.get::<_,String>(4)?,
        "created":r.get::<_,i64>(5)?,"expires":r.get::<_,i64>(6)?,"status":r.get::<_,String>(7)?,
        "option_id":r.get::<_,Option<String>>(8)?,"actor":r.get::<_,Option<String>>(9)?,
        "channel":r.get::<_,String>(10)?,"thread":r.get::<_,String>(11)?,"slack_ts":r.get::<_,Option<String>>(12)?,
        "request":serde_json::from_str::<Value>(&r.get::<_,String>(13)?).unwrap_or(Value::Null)}),
    )
}
pub fn list(store: &Store) -> Result<Vec<Value>> {
    let mut q=store.db.prepare("SELECT id,job_id,context,session_id,approver,created,expires,status,option_id,actor,channel,thread,slack_ts,body FROM permissions ORDER BY (status='pending') DESC,created DESC LIMIT 100")?;
    Ok(q.query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
fn current(store: &Store, id: &str) -> Result<Option<Value>> {
    Ok(store.db.query_row("SELECT id,job_id,context,session_id,approver,created,expires,status,option_id,actor,channel,thread,slack_ts,body FROM permissions WHERE id=?1",[id],row).optional()?)
}
fn live(store: &Store, c: &Config, id: &str) -> Result<bool> {
    Ok(c.enabled && store.db.query_row("SELECT EXISTS(SELECT 1 FROM permissions p JOIN jobs j ON j.id=p.job_id JOIN contexts x ON x.id=j.context JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE p.id=?1 AND p.config_revision=?2 AND j.status='running' AND j.version=x.version AND m.revision=json_extract(j.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted'))", params![id,c.revision as i64], |r|r.get(0))?)
}
pub fn cancel_all(store: &Store, reason: &str) -> Result<()> {
    let changed = store.db.execute(
        "UPDATE permissions SET status='cancelled',actor=?1 WHERE status='pending'",
        [reason],
    )?;
    if changed > 0 {
        store.audit(
            "permissions_cancelled",
            json!({"count":changed,"reason":reason}),
        )?;
    }
    Ok(())
}
pub fn expire(store: &Store, c: &Config) -> Result<()> {
    let pending = store
        .db
        .prepare("SELECT id,expires FROM permissions WHERE status='pending'")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, expires) in pending {
        let state = if expires <= now_ms() {
            Some("expired")
        } else if !live(store, c, &id)? {
            Some("cancelled")
        } else {
            None
        };
        if let Some(state) = state {
            store.db.execute("UPDATE permissions SET status=?2,actor='lifecycle' WHERE id=?1 AND status='pending'",params![id,state])?;
            store.audit("permission_closed", json!({"id":id,"status":state}))?;
        }
    }
    Ok(())
}
/// Management callers are separately authenticated administrators; they cannot
/// impersonate a Slack member. Slack callers must match the initiating human.
pub fn resolve(
    store: &Store,
    c: &Config,
    id: &str,
    option: Option<&str>,
    actor: &str,
    slack: Option<&Value>,
) -> Result<()> {
    expire(store, c)?;
    let p = current(store, id)?.context("Permission request not found")?;
    ensure!(
        p["status"] == "pending" && live(store, c, id)?,
        "Permission is no longer pending"
    );
    ensure!(
        c.acp
            .client
            .approval_users
            .iter()
            .any(|u| p["approver"] == *u),
        "Permission approver is no longer authorized"
    );
    if let Some(payload) = slack {
        ensure!(
            payload["team"]["id"] == c.slack.team_id
                && payload["api_app_id"] == c.slack.app_id
                && payload["user"]["id"] == p["approver"]
                && payload["container"]["channel_id"] == p["channel"]
                && payload["container"]["message_ts"] == p["slack_ts"]
                && p["slack_ts"].is_string(),
            "This control belongs to another person or conversation"
        );
    }
    let choice = match option {
        Some(id) => Some(
            p["request"]["options"]
                .as_array()
                .context("Invalid permission options")?
                .iter()
                .find(|v| v["optionId"] == id)
                .context("Unknown permission option")?,
        ),
        None => None,
    };
    let status = match choice.and_then(|v| v["kind"].as_str()) {
        Some("allow_once") => "approved",
        Some("reject_once") => "denied",
        None => "cancelled",
        _ => anyhow::bail!("Persistent grants are not supported by this connector"),
    };
    let n = store.db.execute("UPDATE permissions SET status=?2,option_id=?3,actor=?4 WHERE id=?1 AND status='pending' AND expires>?5 AND EXISTS(SELECT 1 FROM jobs j JOIN contexts x ON x.id=j.context WHERE j.id=permissions.job_id AND j.status='running' AND j.version=x.version)", params![id,status,option,actor,now_ms()])?;
    ensure!(n == 1, "Permission changed before the decision was applied");
    store.audit(
        "permission_resolved",
        json!({"id":id,"status":status,"actor":actor,"context":p["context"]}),
    )?;
    Ok(())
}
pub fn action(store: &Store, c: &Config, payload: &Value) -> Result<()> {
    ensure!(
        payload["type"] == "block_actions",
        "Unsupported Slack interaction"
    );
    let actions = payload["actions"].as_array().context("Missing actions")?;
    ensure!(
        actions.len() == 1 && actions[0]["action_id"] == "acp_permission",
        "Unknown permission control"
    );
    let value = actions[0]["value"]
        .as_str()
        .context("Missing control value")?;
    let (id, index) = value.split_once(':').context("Invalid control value")?;
    let p = current(store, id)?.context("Permission not found")?;
    let option = if index == "cancel" {
        None
    } else {
        let i: usize = index.parse()?;
        Some(
            p["request"]["options"]
                .get(i)
                .and_then(|v| v["optionId"].as_str())
                .context("Unknown option")?,
        )
    };
    resolve(
        store,
        c,
        id,
        option,
        &format!("slack:{}", payload["user"]["id"].as_str().unwrap_or("")),
        Some(payload),
    )
}
pub async fn details(home: &Path, c: &Config, slack: &Slack, payload: &Value) -> Result<()> {
    let view = {
        let store = Store::open(home)?;
        let id = payload["actions"][0]["value"]
            .as_str()
            .context("Missing request")?;
        let p = current(&store, id)?.context("Permission not found")?;
        ensure!(
            payload["team"]["id"] == c.slack.team_id
                && payload["api_app_id"] == c.slack.app_id
                && payload["user"]["id"] == p["approver"]
                && payload["container"]["channel_id"] == p["channel"]
                && payload["container"]["message_ts"] == p["slack_ts"]
                && c.acp
                    .client
                    .approval_users
                    .iter()
                    .any(|u| p["approver"] == *u),
            "Permission details belong to another person or conversation"
        );
        let raw = serde_json::to_string_pretty(&p["request"]["toolCall"])?;
        let text: Vec<char> = raw.chars().take(24000).collect();
        let mut blocks = vec![
            json!({"type":"section","text":{"type":"plain_text","text":format!("Status: {}. Review this action, close this window, then choose an option on the original request. Allow once applies only to this request.",p["status"].as_str().unwrap_or("closed"))}}),
        ];
        for chunk in text.chunks(2800) {
            blocks.push(json!({"type":"section","text":{"type":"plain_text","text":chunk.iter().collect::<String>()}}));
        }
        if raw.chars().count() > 24000 {
            blocks.push(json!({"type":"section","text":{"type":"plain_text","text":"Details shortened. Do not approve unless this shows enough information to understand the action. The connector owner can inspect the complete request in its dashboard."}}));
        }
        json!({"type":"modal","title":{"type":"plain_text","text":"Review agent action"},"close":{"type":"plain_text","text":"Back to request"},"blocks":blocks})
    };
    slack
        .call(
            "views.open",
            json!({"trigger_id":payload["trigger_id"],"view":view}),
            false,
        )
        .await
        .map_err(|_| anyhow::anyhow!("Could not open permission details"))?;
    Ok(())
}
fn summary(tool: &Value) -> String {
    let title = tool["title"].as_str().unwrap_or("Agent tool request");
    let mut text = format!(
        "{}\nType: {}",
        title,
        tool["kind"].as_str().unwrap_or("other")
    );
    // Full details are private to the authenticated management view. Only a
    // supplied description and locations appear in the originating Slack room.
    for location in tool["locations"].as_array().into_iter().flatten().take(8) {
        if let Some(path) = location["path"].as_str() {
            text.push_str(&format!("\nFile: {path}"));
        }
    }
    text.chars().take(1600).collect()
}
pub async fn refresh_messages(home: &Path, c: &Config, slack: &Slack) -> Result<()> {
    let rows = {
        let store = Store::open(home)?;
        expire(&store, c)?;
        let mut stmt=store.db.prepare("SELECT id,channel,slack_ts,status FROM permissions WHERE slack_ts IS NOT NULL AND status<>'pending' AND shown_status<>status LIMIT 20")?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, channel, ts, status) in rows {
        let text = format!("Agent permission: {status}. This request is closed.");
        if slack.call("chat.update",json!({"channel":channel,"ts":ts,"text":text,"blocks":[{"type":"section","text":{"type":"plain_text","text":text}}]}),false).await.is_ok() {
            Store::open(home)?.db.execute("UPDATE permissions SET shown_status=?2 WHERE id=?1",params![id,status])?;
        }
    }
    Ok(())
}
struct Pending {
    home: PathBuf,
    id: String,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(s) = Store::open(&self.home) {
            let _=s.db.execute("UPDATE permissions SET status='cancelled',actor='turn ended' WHERE id=?1 AND status='pending'",[&self.id]);
        }
    }
}
pub async fn request(
    c: &Config,
    home: &Path,
    input: &Value,
    session: &str,
    params: &Value,
) -> Result<Value> {
    let cancelled = json!({"outcome":{"outcome":"cancelled"}});
    let sender = input["trigger"]["sender"].as_str().unwrap_or("");
    let ctx = input["conversation"]["context"]
        .as_str()
        .context("Missing conversation")?;
    if input["trigger"]["bot"] != false || !c.acp.client.approval_users.iter().any(|u| u == sender)
    {
        Store::open(home)?.audit(
            "permission_declined",
            json!({"context":ctx,"reason":"No configured human approver initiated this turn"}),
        )?;
        return Ok(cancelled);
    }
    ensure!(
        params["sessionId"] == session,
        "Permission belongs to a different session"
    );
    ensure!(
        params.to_string().len() <= 65536,
        "Permission request too large"
    );
    let supplied = params["options"]
        .as_array()
        .context("Missing permission options")?;
    let mut ids = std::collections::HashSet::new();
    for option in supplied {
        let id = option["optionId"]
            .as_str()
            .context("Missing permission option ID")?;
        ensure!(
            !id.is_empty() && ids.insert(id),
            "Permission option IDs must be unique and nonempty"
        );
    }
    let options: Vec<Value> = supplied
        .iter()
        .filter(|v| matches!(v["kind"].as_str(), Some("allow_once" | "reject_once")))
        .filter(|v| v["optionId"].as_str().is_some_and(|s| s.len() <= 512) && v["name"].is_string())
        .take(4)
        .cloned()
        .collect();
    if options.is_empty() {
        return Ok(cancelled);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let channel = input["conversation"]["channel"]
        .as_str()
        .context("Missing channel")?;
    let thread = input["conversation"]["thread"].as_str().unwrap_or("");
    let job = input["job_id"].as_str().context("Missing job binding")?;
    let body = json!({"toolCall":params["toolCall"],"options":options});
    let expires = now_ms() + 1000 * c.acp.client.permission_timeout_seconds as i64;
    {
        let s = Store::open(home)?;
        s.db.execute("DELETE FROM permissions WHERE status<>'pending' AND shown_status=status AND id NOT IN (SELECT id FROM permissions ORDER BY created DESC LIMIT 1000)",[])?;
        s.db.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,channel,thread,body) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending',?9,?10,?11)",params![id,job,ctx,session,sender,c.revision as i64,now_ms(),expires,channel,thread,body.to_string()])?;
        if !live(&s, c, &id)? {
            s.db.execute(
                "UPDATE permissions SET status='cancelled' WHERE id=?1",
                [&id],
            )?;
            return Ok(cancelled);
        }
        s.audit(
            "permission_requested",
            json!({"id":id,"context":ctx,"session_id":session,"approver":sender,"expires":expires}),
        )?;
    }
    let _pending = Pending {
        home: home.to_owned(),
        id: id.clone(),
    };
    let slack = Slack::new(c, c.secrets(home)?)?;
    let mut buttons:Vec<Value>=options.iter().enumerate().map(|(i,o)|json!({"type":"button","action_id":"acp_permission","text":{"type":"plain_text","text":o["name"].as_str().unwrap().chars().take(60).collect::<String>()},"value":format!("{id}:{i}")})).collect();
    buttons.push(json!({"type":"button","action_id":"acp_permission","text":{"type":"plain_text","text":"Cancel request"},"value":format!("{id}:cancel")}));
    // Slack requires action_id uniqueness within a block. Each option gets its own block.
    let mut blocks = vec![
        json!({"type":"section","text":{"type":"plain_text","text":format!("Permission needed from {sender}\n{}\nExpires in {} seconds. Allow once applies only to this request.",summary(&params["toolCall"]),c.acp.client.permission_timeout_seconds)}}),
    ];
    blocks.push(json!({"type":"section","text":{"type":"plain_text","text":"Review the action details privately before deciding."},"accessory":{"type":"button","action_id":"permission_details","text":{"type":"plain_text","text":"Review details"},"value":id}}));
    for button in buttons {
        blocks.push(json!({"type":"actions","elements":[button]}));
    }
    let mut post = json!({"channel":channel,"client_msg_id":id,"text":"Agent permission needs your decision","blocks":blocks,"metadata":{"event_type":"acp_permission","event_payload":{"id":id}},"unfurl_links":false,"unfurl_media":false});
    if !thread.is_empty() {
        post["thread_ts"] = json!(thread);
    }
    let result = slack.call("chat.postMessage", post, false).await;
    let ts = match result {
        Ok(ref v) => v["ts"].as_str(),
        Err(_) => None,
    };
    let Some(ts) = ts else {
        Store::open(home)?.audit("permission_delivery_failed",json!({"id":id,"reason":"Approval request was not confirmed by Slack; no permission granted"}))?;
        return Ok(cancelled);
    };
    Store::open(home)?.db.execute(
        "UPDATE permissions SET slack_ts=?2,shown_status='pending' WHERE id=?1",
        params![id, ts],
    )?;
    loop {
        let current_config = Config::load(home)?;
        let s = Store::open(home)?;
        expire(&s, &current_config)?;
        let p = current(&s, &id)?.context("Permission disappeared")?;
        match p["status"].as_str().unwrap_or("cancelled") {
            "approved" | "denied" => {
                return Ok(json!({"outcome":{"outcome":"selected","optionId":p["option_id"]}}));
            }
            "pending" => {}
            _ => return Ok(cancelled),
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
