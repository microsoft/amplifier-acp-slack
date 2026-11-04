//! Installation policy and human decisions for ACP permissions, bound to one live turn.
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
    let has_closed = store
        .db
        .prepare("PRAGMA table_info(permissions)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .any(|v| v.is_ok_and(|v| v == "closed_at"));
    if !has_closed {
        store
            .db
            .execute("ALTER TABLE permissions ADD COLUMN closed_at INTEGER", [])?;
        // Upgrade recent existing cards in place, without posting new messages.
        store.db.execute("UPDATE permissions SET shown_status='' WHERE status<>'pending' AND slack_ts IS NOT NULL AND id IN (SELECT id FROM permissions ORDER BY created DESC LIMIT 100)",[])?;
    }
    Ok(())
}
/// Union of actual human-wait intervals, so concurrent requests cannot multiply
/// the execution allowance. Old closed rows do not extend new turns.
pub fn waiting_ms(store: &Store, job: &str, now: i64) -> Result<i64> {
    let mut q=store.db.prepare("SELECT created,MIN(expires,COALESCE(closed_at,CASE WHEN status='pending' THEN ?2 ELSE created END),?2) FROM permissions WHERE job_id=?1 ORDER BY created")?;
    let intervals = q
        .query_map(params![job, now], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut end = 0;
    let mut elapsed = 0;
    for (start, stop) in intervals {
        elapsed += (stop - start.max(end)).max(0);
        end = end.max(stop);
    }
    Ok(elapsed)
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
/// Match the callback's trusted turn input to the current persisted source.
/// No caller-supplied identity can replace the sender recorded by Slack ingress.
pub(crate) fn live_turn(db: &rusqlite::Connection, c: &Config, input: &Value) -> Result<bool> {
    if !c.enabled {
        return Ok(false);
    }
    let Some(job) = input["job_id"].as_str() else {
        return Ok(false);
    };
    let Some(ctx) = input["conversation"]["context"].as_str() else {
        return Ok(false);
    };
    let source: Option<String> = db.query_row(
        "SELECT j.trigger FROM jobs j JOIN contexts x ON x.id=j.context JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE j.id=?1 AND j.context=?2 AND m.context=j.context AND j.status='running' AND j.version=x.version AND m.revision=json_extract(j.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted') AND json_extract(m.body,'$.sender')=json_extract(j.trigger,'$.sender') AND json_extract(m.body,'$.bot')=json_extract(j.trigger,'$.bot')",
        params![job,ctx], |r| r.get(0)).optional()?;
    let Some(source) = source else {
        return Ok(false);
    };
    let source: crate::model::Message = serde_json::from_str(&source)?;
    let source_value = serde_json::to_value(&source)?;
    Ok(!source.deleted
        && !source.sender.is_empty()
        && source.team == c.slack.team_id
        && source.context() == ctx
        && input["conversation"]["channel"] == source.channel
        && input["conversation"]["thread"] == source.thread
        && [
            "team", "channel", "thread", "ts", "revision", "sender", "bot",
        ]
        .iter()
        .all(|key| input["trigger"][key] == source_value[key])
        && c.allows(&source.channel, ""))
}
fn prune(store: &Store) -> Result<()> {
    // Retain active-turn intervals so pruning cannot reset the human wait budget.
    store.db.execute("DELETE FROM permissions WHERE status<>'pending' AND shown_status=status AND job_id NOT IN (SELECT id FROM jobs WHERE status='running') AND id NOT IN (SELECT id FROM permissions ORDER BY created DESC LIMIT 1000)",[])?;
    Ok(())
}
fn live(store: &Store, c: &Config, id: &str) -> Result<bool> {
    Ok(c.enabled && store.db.query_row("SELECT EXISTS(SELECT 1 FROM permissions p JOIN jobs j ON j.id=p.job_id JOIN contexts x ON x.id=j.context JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE p.id=?1 AND p.config_revision=?2 AND p.context=j.context AND m.context=j.context AND p.channel=m.channel AND p.thread=json_extract(j.trigger,'$.thread') AND p.approver=json_extract(j.trigger,'$.sender') AND NOT json_extract(j.trigger,'$.bot') AND json_extract(m.body,'$.sender')=p.approver AND NOT json_extract(m.body,'$.bot') AND j.status='running' AND j.version=x.version AND m.revision=json_extract(j.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted'))", params![id,c.revision as i64], |r|r.get(0))?)
}
pub fn cancel_all(store: &Store, reason: &str) -> Result<()> {
    let changed = store.db.execute(
        "UPDATE permissions SET status='cancelled',actor=?1,closed_at=?2 WHERE status='pending'",
        params![reason, now_ms()],
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
            store.db.execute("UPDATE permissions SET status=?2,actor='lifecycle',closed_at=?3 WHERE id=?1 AND status='pending'",params![id,state,now_ms()])?;
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
    if let Some(payload) = slack {
        ensure!(
            payload["team"]["id"] == c.slack.team_id
                && payload["api_app_id"] == c.slack.app_id
                && payload["user"]["id"] == p["approver"]
                && payload["container"]["channel_id"] == p["channel"]
                && payload["container"]["message_ts"] == p["slack_ts"]
                && p["slack_ts"].is_string()
                && payload["user"].get("is_bot") != Some(&json!(true)),
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
    let n = store.db.execute("UPDATE permissions SET status=?2,option_id=?3,actor=?4,closed_at=?5 WHERE id=?1 AND status='pending' AND expires>?5 AND EXISTS(SELECT 1 FROM jobs j JOIN contexts x ON x.id=j.context WHERE j.id=permissions.job_id AND j.status='running' AND j.version=x.version)", params![id,status,option,actor,now_ms()])?;
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
                && p["slack_ts"].is_string()
                && payload["user"].get("is_bot") != Some(&json!(true)),
            "Permission details belong to another person or conversation"
        );
        let raw = serde_json::to_string_pretty(&p["request"]["toolCall"])?;
        let text: Vec<char> = raw.chars().take(24000).collect();
        let mut blocks = vec![
            json!({"type":"section","text":{"type":"plain_text","text":format!("Status: {}. {}",p["status"].as_str().unwrap_or("closed"),if p["status"] == "pending" {"Close this window to decide on the original request. Allow once applies only to this action."} else {"This request is closed. These are the original action details; approval is not an execution result."})}}),
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
fn details_button(id: &str) -> Value {
    json!({"type":"section","text":{"type":"plain_text","text":"Action details are available to the person who was asked."},"accessory":{"type":"button","action_id":"permission_details","text":{"type":"plain_text","text":"Review details"},"value":id}})
}
fn closed_card(id: &str, status: &str, tool: &Value) -> (String, Vec<Value>) {
    let outcome = match status {
        "approved" => "Approved. This records permission, not whether the action ran or succeeded.",
        "denied" => "Declined. This request granted no permission.",
        "expired" => {
            "Expired without approval. Ask the agent to try again when you are ready; a fresh request is required."
        }
        _ => "Cancelled. This request granted no permission.",
    };
    let text = format!("{}\n{outcome}", summary(tool));
    (
        text.clone(),
        vec![
            json!({"type":"section","text":{"type":"plain_text","text":text}}),
            details_button(id),
        ],
    )
}
pub async fn refresh_messages(home: &Path, c: &Config, slack: &Slack) -> Result<()> {
    let rows = {
        let store = Store::open(home)?;
        expire(&store, c)?;
        let mut stmt=store.db.prepare("SELECT id,channel,slack_ts,status,body FROM permissions WHERE slack_ts IS NOT NULL AND status<>'pending' AND shown_status<>status LIMIT 20")?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, channel, ts, status, body) in rows {
        let request: Value = serde_json::from_str(&body)?;
        let (text, blocks) = closed_card(&id, &status, &request["toolCall"]);
        if slack
            .call(
                "chat.update",
                json!({"channel":channel,"ts":ts,"text":text,"blocks":blocks}),
                false,
            )
            .await
            .is_ok()
        {
            Store::open(home)?.db.execute(
                "UPDATE permissions SET shown_status=?2 WHERE id=?1",
                params![id, status],
            )?;
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
            let _=s.db.execute("UPDATE permissions SET status='cancelled',actor='turn ended',closed_at=?2 WHERE id=?1 AND status='pending'",params![&self.id,now_ms()]);
        }
        crate::work_status::changed(&self.home);
    }
}
fn wait_label(seconds: u64) -> String {
    if seconds == 1 {
        "1 second".into()
    } else if seconds == 60 {
        "1 minute".into()
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} minutes", seconds / 60)
    } else {
        format!("{seconds} seconds")
    }
}
fn remaining_wait_label(milliseconds: i64) -> String {
    if milliseconds < 1000 {
        "less than a second".into()
    } else {
        wait_label((milliseconds / 1000) as u64)
    }
}
pub async fn request(
    c: &Config,
    home: &Path,
    input: &Value,
    session: &str,
    params: &Value,
) -> Result<Value> {
    let cancelled = cancellation("unavailable");
    let sender = input["trigger"]["sender"].as_str().unwrap_or("");
    let ctx = input["conversation"]["context"]
        .as_str()
        .context("Missing conversation")?;
    if c.acp.client.approvals_required && (input["trigger"]["bot"] != false || sender.is_empty()) {
        Store::open(home)?.audit(
            "permission_declined",
            json!({"context":ctx,"reason":"A human must initiate a turn that requires approval"}),
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
    let current_config = Config::load(home)?;
    if current_config.revision != c.revision
        || current_config.acp.client.approvals_required != c.acp.client.approvals_required
    {
        return Ok(cancellation("policy_changed"));
    }
    if !c.acp.client.approvals_required {
        let s = Store::open(home)?;
        // Reserve the write transaction before reading freshness. A deferred
        // read-to-write upgrade can fail immediately with SQLITE_BUSY when
        // activity/reconnect writes race the approval, bypassing busy_timeout.
        let tx =
            rusqlite::Transaction::new_unchecked(&s.db, rusqlite::TransactionBehavior::Immediate)?;
        if !live_turn(&tx, &current_config, input)? {
            s.audit("permission_declined", json!({"context":ctx,"job_id":job,"tool_call_id":params["toolCall"]["toolCallId"],"reason":"The initiating turn or source is no longer current"}))?;
            tx.commit()?;
            return Ok(cancellation("source_changed"));
        }
        let Some(choice) = options.iter().find(|v| v["kind"] == "allow_once") else {
            s.audit("permission_declined", json!({"context":ctx,"job_id":job,"reason":"Automatic approval requires an advertised single-use allow option"}))?;
            tx.commit()?;
            return Ok(cancelled);
        };
        let created = now_ms();
        prune(&s)?;
        tx.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,option_id,actor,channel,thread,shown_status,body,closed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?7,'approved',?8,'policy:automatic',?9,?10,'approved',?11,?7)", params![id,job,ctx,session,sender,c.revision as i64,created,choice["optionId"].as_str(),channel,thread,body.to_string()])?;
        s.audit("permission_auto_approved", json!({"id":id,"context":ctx,"job_id":job,"session_id":session,"option_id":choice["optionId"],"actor":"policy:automatic","config_revision":c.revision}))?;
        tx.commit()?;
        return Ok(json!({"outcome":{"outcome":"selected","optionId":choice["optionId"]}}));
    }
    if !live_turn(&Store::open(home)?.db, &current_config, input)? {
        return Ok(cancelled);
    }
    let remaining = {
        let s = Store::open(home)?;
        let created = now_ms();
        let remaining = (1000 * c.acp.client.permission_timeout_seconds as i64)
            .saturating_sub(waiting_ms(&s, job, created)?);
        if remaining <= 0 {
            s.audit("permission_declined", json!({"context":ctx,"job_id":job,"reason":"This turn has used all of its human approval time; no permission granted"}))?;
            return Ok(cancelled);
        }
        let expires = created + remaining;
        prune(&s)?;
        s.db.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,channel,thread,body) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending',?9,?10,?11)",params![id,job,ctx,session,sender,c.revision as i64,created,expires,channel,thread,body.to_string()])?;
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
        remaining
    };
    let _pending = Pending {
        home: home.to_owned(),
        id: id.clone(),
    };
    crate::work_status::changed(home);
    let slack = Slack::new(c, c.secrets(home)?)?;
    let mut buttons:Vec<Value>=options.iter().enumerate().map(|(i,o)|json!({"type":"button","action_id":"acp_permission","text":{"type":"plain_text","text":o["name"].as_str().unwrap().chars().take(60).collect::<String>()},"value":format!("{id}:{i}")})).collect();
    buttons.push(json!({"type":"button","action_id":"acp_permission","text":{"type":"plain_text","text":"Cancel request"},"value":format!("{id}:cancel")}));
    // Slack requires action_id uniqueness within a block. Each option gets its own block.
    let mut blocks = vec![
        json!({"type":"section","text":{"type":"plain_text","text":format!("Permission needed\n{}\nAbout {} remains to decide. Approval requests in this turn share the same time budget. Allow once applies only to this action.",summary(&params["toolCall"]),remaining_wait_label(remaining))}}),
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
            "expired" => return Ok(cancellation("expired")),
            "cancelled" => return Ok(cancellation("source_changed")),
            _ => return Ok(cancelled),
        }
        drop(s);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn cancellation(reason: &str) -> Value {
    json!({"outcome":{"outcome":"cancelled"},"_meta":{"com.microsoft.amplifier.permission":{"reason":reason}}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_cards_retain_context_and_private_details_without_decision_buttons() {
        let tool = json!({"title":"Run repository checks","kind":"execute","rawInput":{"command":"PRIVATE COMMAND"}});
        for status in ["approved", "denied", "expired", "cancelled"] {
            let (text, blocks) = closed_card("request-1", status, &tool);
            assert!(text.contains("Run repository checks"));
            assert!(!text.contains("PRIVATE COMMAND"));
            assert!(json!(blocks).to_string().contains("permission_details"));
            assert!(!json!(blocks).to_string().contains("acp_permission"));
        }
        assert!(
            closed_card("id", "approved", &tool)
                .0
                .contains("not whether the action ran")
        );
        assert!(
            closed_card("id", "expired", &tool)
                .0
                .contains("fresh request")
        );
    }
    #[test]
    fn human_wait_is_union_of_bounded_intervals_for_only_this_job() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for (id, job, start, end, status, closed) in [
            ("a", "job", 100, 900, "approved", Some(300)),
            ("b", "job", 200, 700, "pending", None),
            ("c", "other", 1, 900, "pending", None),
            ("d", "job", 800, 900, "pending", None),
        ] {
            s.db.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,channel,thread,body,closed_at) VALUES(?1,?2,'ctx','session','Uowner',1,?3,?4,?5,'Droom','','{}',?6)",params![id,job,start,end,status,closed]).unwrap();
        }
        assert_eq!(waiting_ms(&s, "job", 500).unwrap(), 400);
        assert_eq!(waiting_ms(&s, "job", 1000).unwrap(), 700);
        assert_eq!(waiting_ms(&s, "absent", 1000).unwrap(), 0);
        assert_eq!(wait_label(900), "15 minutes");
    }
}
