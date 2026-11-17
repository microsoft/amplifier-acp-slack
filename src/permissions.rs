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
/// New participant input does not revoke an active operation. Source edit/removal,
/// explicitly stopped jobs and configuration changes still invalidate it.
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
        "SELECT j.trigger FROM jobs j JOIN contexts x ON x.id=j.context JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE j.id=?1 AND j.context=?2 AND m.context=j.context AND j.status='running' AND j.version>=x.control_version AND m.revision=json_extract(j.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted') AND json_extract(m.body,'$.sender')=json_extract(j.trigger,'$.sender') AND json_extract(m.body,'$.bot')=json_extract(j.trigger,'$.bot')",
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
    Ok(c.enabled && store.db.query_row("SELECT EXISTS(SELECT 1 FROM permissions p JOIN jobs j ON j.id=p.job_id JOIN contexts x ON x.id=j.context JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE p.id=?1 AND p.config_revision=?2 AND p.context=j.context AND m.context=j.context AND p.channel=m.channel AND p.thread=json_extract(j.trigger,'$.thread') AND p.approver=json_extract(j.trigger,'$.sender') AND NOT json_extract(j.trigger,'$.bot') AND json_extract(m.body,'$.sender')=p.approver AND NOT json_extract(m.body,'$.bot') AND j.status='running' AND j.version>=x.control_version AND m.revision=json_extract(j.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted'))", params![id,c.revision as i64], |r|r.get(0))?)
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
        Some("allow_once" | "allow_always") => "approved",
        Some("reject_once" | "reject_always") => "denied",
        None => "cancelled",
        _ => anyhow::bail!("Unsupported permission option"),
    };
    let n = store.db.execute("UPDATE permissions SET status=?2,option_id=?3,actor=?4,closed_at=?5 WHERE id=?1 AND status='pending' AND expires>?5 AND EXISTS(SELECT 1 FROM jobs j JOIN contexts x ON x.id=j.context WHERE j.id=permissions.job_id AND j.status='running' AND j.version>=x.control_version)", params![id,status,option,actor,now_ms()])?;
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
    // A neutral explanation still requires an authentic click on this exact card.
    // It grants neither request visibility nor permission to decide for its recipient.
    ensure!(
        payload["type"] == "block_actions"
            && payload["actions"].as_array().is_some_and(|a| a.len() == 1)
            && payload["actions"][0]["action_id"] == "permission_details"
            && payload["team"]["id"] == c.slack.team_id
            && payload["api_app_id"] == c.slack.app_id
            && payload["user"]["id"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && matches!(
                payload["user"].get("is_bot"),
                None | Some(Value::Bool(false))
            ),
        "Invalid permission details interaction"
    );
    let (view, notice) = {
        let store = Store::open(home)?;
        let id = payload["actions"][0]["value"]
            .as_str()
            .context("Missing request")?;
        let p = current(&store, id)?.context("Permission not found")?;
        ensure!(
            payload["container"]["channel_id"] == p["channel"]
                && payload["container"]["message_ts"] == p["slack_ts"]
                && p["slack_ts"].is_string(),
            "Permission details belong to another conversation"
        );
        if payload["user"]["id"] == p["approver"] {
            (
                details_view(&p),
                "If the request window didn't open, try View request again.",
            )
        } else {
            let notice = "Only the person who received this request can view its details.";
            (
                json!({"type":"modal","title":{"type":"plain_text","text":"Private request"},"close":{"type":"plain_text","text":"Close"},"blocks":[section(notice)]}),
                notice,
            )
        }
    };
    if let Some(trigger) = payload["trigger_id"].as_str().filter(|s| !s.is_empty())
        && slack
            .call(
                "views.open",
                json!({"trigger_id":trigger,"view":view}),
                false,
            )
            .await
            .is_ok()
    {
        return Ok(());
    }
    // An expired or missing trigger cannot open a modal. Attempt one private,
    // content-free notice to the clicking member; never retry either delivery.
    slack
        .call(
            "chat.postEphemeral",
            json!({"channel":payload["container"]["channel_id"],"user":payload["user"]["id"],"text":notice,"mrkdwn":false}),
            false,
        )
        .await
        .map_err(|_| anyhow::anyhow!("Could not deliver the private permission details notice"))?;
    Ok(())
}
// Display-only helpers. Producer text is inert plain_text, never parsed into
// approval authority. Raw arguments, locations and scope stay in the existing
// requester-only modal, not the originating (potentially public) Slack room.
fn summary(tool: &Value) -> String {
    tool["title"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Agent action")
        .chars()
        .take(1600)
        .collect()
}
fn section(text: impl Into<String>) -> Value {
    json!({"type":"section","text":{"type":"plain_text","text":text.into()}})
}
fn details_button(id: &str) -> Value {
    json!({"type":"section","text":{"type":"plain_text","text":"View the request details privately."},"accessory":{"type":"button","action_id":"permission_details","text":{"type":"plain_text","text":"View request"},"value":id}})
}
const REMEMBERED_OPTIONS_HELP: &str = "Remember covers matching requests. Different commands can still ask. To stop individual prompts, choose Automatic in your agent's approval settings, when supported.";
const DETAILS_LIMIT: usize = 24000;
const DETAILS_SHORTENED: &str = "Some details are shortened. Do not approve unless you can understand the complete request. The connector owner can inspect the full request in its dashboard.";

fn option_label(option: &Value, options: &[Value], index: usize) -> String {
    let label = match option["kind"].as_str() {
        Some("allow_once") => "Allow this request",
        Some("reject_once") => "Deny",
        Some("allow_always") => "Remember matching action",
        Some("reject_always") => "Block matching action",
        _ => "Review choice",
    };
    // Do not collapse multiple producer choices of the same kind. The number
    // matches the complete, unshortened choice wording in the private modal.
    if options
        .iter()
        .filter(|v| v["kind"] == option["kind"])
        .count()
        > 1
    {
        format!("{label} · {}", index + 1)
    } else {
        label.into()
    }
}

fn named_fields(value: &Value, depth: usize) -> String {
    let indent = "  ".repeat(depth.min(8));
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) if depth < 8 => items
            .iter()
            .map(|v| format!("{indent}• {}", named_fields(v, depth + 1)))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(fields) if depth < 8 => fields
            .iter()
            .map(|(key, v)| {
                let separator = if v.is_object() || v.is_array() {
                    "\n"
                } else {
                    " "
                };
                format!("{indent}{key}:{separator}{}", named_fields(v, depth + 1))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn readable_text(text: &str) -> (String, Option<i64>) {
    // ACP text remains text. A fully valid JSON object at the end is display
    // data only: show every field without inventing semantics from its labels.
    let split = text.rsplit_once("\n\n").unwrap_or(("", text));
    match serde_json::from_str::<Value>(split.1) {
        Ok(mut value @ Value::Object(_)) => {
            // Only format an explicit numeric Expires at field for reading.
            // This does not set or validate any authority/deadline. Keep the
            // original epoch in the private technical section.
            let epoch = value["Expires at"]
                .as_i64()
                .filter(|epoch| (0..=253402300799).contains(epoch));
            if let Some(epoch) = epoch
                && let Ok(date) = time::OffsetDateTime::from_unix_timestamp(epoch)
            {
                value["Expires at"] = json!(format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                    date.year(),
                    date.month() as u8,
                    date.day(),
                    date.hour(),
                    date.minute(),
                    date.second()
                ));
            }
            let fields = named_fields(&value, 0);
            if split.0.is_empty() {
                (fields, epoch)
            } else {
                (format!("{}\n\n{fields}", split.0), epoch)
            }
        }
        _ => (text.into(), None),
    }
}

fn private_details(tool: &Value) -> (Vec<String>, Value, Vec<i64>) {
    let mut readable = Vec::new();
    let mut epochs = Vec::new();
    let mut remaining = tool.clone();
    if tool["title"]
        .as_str()
        .is_some_and(|title| title.chars().count() <= 1600)
    {
        remaining.as_object_mut().unwrap().remove("title");
    }
    if let Some(content) = tool["content"].as_array() {
        let mut unrendered = Vec::new();
        for item in content {
            if item["type"] == "content"
                && item["content"]["type"] == "text"
                && let Some(text) = item["content"]["text"].as_str()
            {
                if !text.trim().is_empty() {
                    let (text, epoch) = readable_text(text);
                    readable.push(text);
                    if let Some(epoch) = epoch {
                        epochs.push(epoch);
                    }
                }
                // Unknown annotations/fields are not silently discarded.
                let mut extra = item.clone();
                extra.as_object_mut().unwrap().remove("type");
                let nested = extra["content"].as_object_mut().unwrap();
                nested.remove("type");
                nested.remove("text");
                if nested.is_empty() {
                    extra.as_object_mut().unwrap().remove("content");
                }
                if extra.as_object().is_some_and(|v| !v.is_empty()) {
                    unrendered.push(extra);
                }
            } else {
                unrendered.push(item.clone());
            }
        }
        if unrendered.is_empty() {
            remaining.as_object_mut().unwrap().remove("content");
        } else {
            remaining["content"] = json!(unrendered);
        }
    }
    (readable, remaining, epochs)
}

fn details_view(p: &Value) -> Value {
    let tool = &p["request"]["toolCall"];
    let pending = p["status"] == "pending";
    let mut blocks = vec![
        section(summary(tool)),
        section(if pending {
            "Waiting for your decision. Close this view to choose on the original message. Any permission duration is described by the agent below."
        } else {
            "This request is closed. These are the details originally supplied by the agent. Approval records permission, not whether the action ran or succeeded."
        }),
    ];
    let mut budget = DETAILS_LIMIT;
    let mut shortened = false;
    let mut append = |blocks: &mut Vec<Value>, text: &str| {
        let chars: Vec<_> = text.chars().take(budget).collect();
        if chars.len() < text.chars().count() {
            shortened = true;
        }
        budget -= chars.len();
        for chunk in chars.chunks(2800) {
            if blocks.len() >= 40 {
                shortened = true;
                break;
            }
            blocks.push(section(chunk.iter().collect::<String>()));
        }
    };
    let (readable, technical, epochs) = private_details(tool);
    let mut extra_options = Vec::new();
    if readable.is_empty() {
        append(
            &mut blocks,
            "The agent supplied technical details only. Review them below before deciding.",
        );
    } else {
        append(&mut blocks, "Request details supplied by the agent");
        for text in readable {
            append(&mut blocks, &text);
        }
    }
    if let Some(options) = p["request"]["options"].as_array() {
        if has_remembered_options(&p["request"]["options"]) {
            append(&mut blocks, REMEMBERED_OPTIONS_HELP);
        }
        append(
            &mut blocks,
            "What your choice means — full wording from the agent",
        );
        for (index, option) in options.iter().enumerate() {
            let name = option["name"].as_str().unwrap_or("Unnamed choice");
            let label = option_label(option, options, index);
            let text = if name == label {
                format!("{}. {name}", index + 1)
            } else {
                format!("{}. {label}\n{name}", index + 1)
            };
            append(&mut blocks, &text);
            let mut extra = option.clone();
            if let Some(fields) = extra.as_object_mut() {
                for name in ["optionId", "kind", "name"] {
                    fields.remove(name);
                }
                if !fields.is_empty() {
                    extra_options.push(json!({"choice":index+1,"fields":extra}));
                }
            }
        }
    }
    if !epochs.is_empty()
        || !extra_options.is_empty()
        || technical.as_object().is_none_or(|v| !v.is_empty())
    {
        append(&mut blocks, "Technical details");
        append(
            &mut blocks,
            &serde_json::to_string_pretty(&technical).unwrap_or_default(),
        );
        if !epochs.is_empty() {
            append(
                &mut blocks,
                &format!(
                    "Original numeric Expires at values (Unix seconds): {}",
                    json!(epochs)
                ),
            );
        }
        if !extra_options.is_empty() {
            append(
                &mut blocks,
                &format!(
                    "Additional choice fields: {}",
                    serde_json::to_string_pretty(&extra_options).unwrap_or_default()
                ),
            );
        }
    }
    if shortened {
        blocks.push(section(DETAILS_SHORTENED));
    }
    json!({"type":"modal","title":{"type":"plain_text","text":"Review request"},"close":{"type":"plain_text","text":"Back to request"},"blocks":blocks})
}

fn pending_blocks(id: &str, tool: &Value, options: &[Value], remaining: i64) -> Vec<Value> {
    let mut blocks = vec![
        section(format!("Approval needed\n{}", summary(tool))),
        details_button(id),
        section(format!(
            "Respond within about {}. This is the time to answer, not the permission duration. Requests in this turn share the same response time.",
            remaining_wait_label(remaining)
        )),
    ];
    if has_remembered_options(&json!(options)) {
        blocks.push(section(REMEMBERED_OPTIONS_HELP));
    }
    for (index, option) in options.iter().enumerate() {
        let label = option_label(option, options, index);
        blocks.push(json!({"type":"actions","elements":[{"type":"button","action_id":"acp_permission","text":{"type":"plain_text","text":label},"accessibility_label":label,"value":format!("{id}:{index}")}]}));
    }
    blocks.push(json!({"type":"actions","elements":[{"type":"button","action_id":"acp_permission","text":{"type":"plain_text","text":"Cancel request"},"value":format!("{id}:cancel")}]}));
    blocks
}

fn has_remembered_options(options: &Value) -> bool {
    options.as_array().is_some_and(|options| {
        options.iter().any(|option| {
            matches!(
                option["kind"].as_str(),
                Some("allow_always" | "reject_always")
            )
        })
    })
}

fn closed_card(
    id: &str,
    status: &str,
    tool: &Value,
    selected_kind: Option<&str>,
) -> (String, Vec<Value>) {
    let outcome = match (status, selected_kind) {
        ("approved", Some("allow_always")) => {
            "Remember matching action selected. Manage the rule in the agent's settings."
        }
        ("denied", Some("reject_always")) => {
            "Block matching action selected. No permission granted."
        }
        ("approved", _) => {
            "Approved for this request. Execution is not confirmed here."
        }
        ("denied", _) => "Declined. This request granted no permission.",
        ("expired", _) => {
            "Expired without approval. Ask the agent to try again when you are ready; a fresh request is required."
        }
        _ => "Cancelled. This request granted no permission.",
    };
    let text = format!("{}\n{outcome}", summary(tool));
    (
        text.clone(),
        vec![json!({"type":"section","text":{"type":"plain_text","text":text},"accessory":details_button(id)["accessory"]})],
    )
}
pub async fn refresh_messages(home: &Path, c: &Config, slack: &Slack) -> Result<()> {
    let rows = {
        let store = Store::open(home)?;
        expire(&store, c)?;
        let mut stmt=store.db.prepare("SELECT id,channel,slack_ts,status,body,option_id FROM permissions WHERE slack_ts IS NOT NULL AND status<>'pending' AND shown_status<>status LIMIT 20")?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, channel, ts, status, body, option_id) in rows {
        let request: Value = serde_json::from_str(&body)?;
        let selected_kind = option_id.as_deref().and_then(|id| {
            request["options"]
                .as_array()?
                .iter()
                .find(|option| option["optionId"] == id)?["kind"]
                .as_str()
        });
        let (text, blocks) = closed_card(&id, &status, &request["toolCall"], selected_kind);
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
            let _=s.db.execute("UPDATE permissions SET status='cancelled',actor='request ended',closed_at=?2 WHERE id=?1 AND status='pending'",params![&self.id,now_ms()]);
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
        .filter(|v| {
            matches!(
                v["kind"].as_str(),
                Some("allow_once" | "reject_once" | "allow_always" | "reject_always")
            )
        })
        .filter(|v| v["optionId"].as_str().is_some_and(|s| s.len() <= 512) && v["name"].is_string())
        .take(if c.acp.client.approvals_required {
            4
        } else {
            // Automatic policy may only select allow_once. A display limit
            // must not hide it behind multiple advertised persistent choices.
            usize::MAX
        })
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
    let blocks = pending_blocks(&id, &params["toolCall"], &options, remaining);
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
            let (text, blocks) = closed_card("request-1", status, &tool, None);
            assert!(text.contains("Run repository checks"));
            assert!(!text.contains("PRIVATE COMMAND"));
            assert!(json!(blocks).to_string().contains("permission_details"));
            assert!(!json!(blocks).to_string().contains("acp_permission"));
        }
        assert!(
            closed_card("id", "approved", &tool, None)
                .0
                .contains("Execution is not confirmed here")
        );
        assert!(
            closed_card("id", "expired", &tool, None)
                .0
                .contains("fresh request")
        );
    }
    fn example_permission() -> Value {
        let scope = json!({"Changes":["Change file-access settings"],"Scopes":["local"],"Applies to":"This conversation only, until Stop, revocation, or expiry.","Expires at":1794868606});
        json!({"status":"pending","request":{
            "toolCall":{"toolCallId":"capability:example","title":"Allow configuration changes?","kind":"other","content":[{"type":"content","content":{"type":"text","text":format!("Approval-flow check only. Request temporary permission for this conversation's local file-access settings for two minutes. Do not change settings or read/write files. Revoke this exact grant after approval.\n\n{scope}")}}]},
            "options":[{"optionId":"once-exact","kind":"allow_once","name":"Allow this request"},{"optionId":"deny-exact","kind":"reject_once","name":"Deny"},{"optionId":"always-exact","kind":"allow_always","name":"Always allow matching requests across this agent's chats"}]
        }})
    }
    #[test]
    fn permission_card_and_modal_render_fixture() {
        let permission = example_permission();
        let options = permission["request"]["options"].as_array().unwrap();
        let pending = pending_blocks(
            "request",
            &permission["request"]["toolCall"],
            options,
            900000,
        );
        let card = json!(pending).to_string();
        assert!(card.contains("not the permission duration"));
        assert!(!card.contains("Type: other"));
        assert!(!card.contains("file-access settings"));
        let buttons: Vec<_> = pending
            .iter()
            .filter(|v| v["type"] == "actions")
            .map(|v| &v["elements"][0])
            .collect();
        assert_eq!(
            buttons
                .iter()
                .map(|v| v["text"]["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "Allow this request",
                "Deny",
                "Remember matching action",
                "Cancel request"
            ]
        );
        for (index, button) in buttons.iter().take(3).enumerate() {
            assert_eq!(button["value"], format!("request:{index}"));
            assert_eq!(button["action_id"], "acp_permission");
            assert!(button["text"]["text"].as_str().unwrap().chars().count() <= 30);
            assert_eq!(button["accessibility_label"], button["text"]["text"]);
        }
        let modal = details_view(&permission);
        let text = modal["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"]["text"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Changes:\n  • Change file-access settings"));
        assert!(text.contains("Applies to: This conversation only"));
        assert!(text.contains("2026-11-16 22:36:46 UTC"));
        assert!(text.contains("Original numeric Expires at values (Unix seconds): [1794868606]"));
        assert!(text.contains("Always allow matching requests across this agent's chats"));
        assert_eq!(text.matches("Approval-flow check only").count(), 1);
        assert!(!text.contains("\\n"));
        let (_, approved) = closed_card(
            "request",
            "approved",
            &permission["request"]["toolCall"],
            Some("allow_once"),
        );
        println!(
            "PERMISSION_UI_RENDER {}",
            json!({"pending":pending,"modal":modal,"approved":approved,"scope":"Locally generated Block Kit, not a real Slack screenshot"})
        );
    }
    #[test]
    fn private_details_preserve_unknown_data_and_malformed_text_without_public_leaks() {
        let mut p = example_permission();
        p["request"]["toolCall"]["rawInput"] = json!({"secret":"PRIVATE_SECRET"});
        p["request"]["toolCall"]["title"] =
            json!(format!("{} IMPORTANT_TITLE_TAIL", "x".repeat(1700)));
        p["request"]["options"][0]["futureRisk"] = json!("PRIVATE_CHOICE_RISK");
        p["request"]["toolCall"]["locations"] = json!([{"path":"/private/PRIVATE_PATH"}]);
        p["request"]["toolCall"]["content"][0]["content"]["annotations"] =
            json!({"extra":"PRIVATE_ANNOTATION"});
        p["request"]["toolCall"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"future-content","risk":"PRIVATE_RISK"}));
        p["request"]["toolCall"]["content"].as_array_mut().unwrap().push(json!({"type":"content","content":{"type":"text","text":"Literal <@everyone> & café 🛟\n\n{malformed JSON"}}));
        let modal = details_view(&p);
        let raw = modal.to_string();
        for marker in [
            "PRIVATE_SECRET",
            "IMPORTANT_TITLE_TAIL",
            "PRIVATE_CHOICE_RISK",
            "PRIVATE_PATH",
            "PRIVATE_ANNOTATION",
            "PRIVATE_RISK",
            "<@everyone>",
            "café 🛟",
            "{malformed JSON",
        ] {
            assert!(raw.contains(marker), "{marker}");
        }
        for block in modal["blocks"].as_array().unwrap() {
            assert_eq!(block["text"]["type"], "plain_text");
        }
        let public = json!(pending_blocks(
            "id",
            &p["request"]["toolCall"],
            p["request"]["options"].as_array().unwrap(),
            1000
        ))
        .to_string();
        assert!(!public.contains("PRIVATE_"));
        assert!(!public.contains("<@everyone>"));
    }
    #[test]
    fn custom_choices_keep_distinct_positions_and_full_private_wording() {
        let mut p = example_permission();
        p["request"]["options"] = json!([
            {"optionId":"narrow","kind":"allow_once","name":"Only allow the selected private file α"},
            {"optionId":"broad","kind":"allow_once","name":"Allow this request including its private folder β"},
            {"optionId":"never","kind":"reject_always","name":"Never allow this exact scope again"}
        ]);
        let opts = p["request"]["options"].as_array().unwrap();
        let blocks = pending_blocks("id", &p["request"]["toolCall"], opts, 1000);
        let buttons: Vec<_> = blocks.iter().filter(|b| b["type"] == "actions").collect();
        assert_eq!(
            buttons[0]["elements"][0]["text"]["text"],
            "Allow this request · 1"
        );
        assert_eq!(
            buttons[1]["elements"][0]["text"]["text"],
            "Allow this request · 2"
        );
        assert_eq!(buttons[1]["elements"][0]["value"], "id:1");
        let modal = details_view(&p).to_string();
        for option in opts {
            assert!(modal.contains(option["name"].as_str().unwrap()));
        }
        assert!(!json!(blocks).to_string().contains("private file"));
    }
    #[test]
    fn private_details_are_bounded_with_explicit_truncation_and_utf8_safe_sections() {
        let mut p = example_permission();
        p["request"]["toolCall"]["content"] =
            json!([{"type":"content","content":{"type":"text","text":"🛟".repeat(25000)}}]);
        let modal = details_view(&p);
        let blocks = modal["blocks"].as_array().unwrap();
        assert!(modal.to_string().contains(DETAILS_SHORTENED));
        assert!(blocks.len() <= 41);
        for block in blocks {
            assert!(block["text"]["text"].as_str().unwrap().chars().count() <= 3000);
        }
        p["request"]["toolCall"]["content"] = json!(
            (0..500)
                .map(|_| json!({"type":"content","content":{"type":"text","text":"short"}}))
                .collect::<Vec<_>>()
        );
        let modal = details_view(&p);
        assert!(modal["blocks"].as_array().unwrap().len() <= 41);
        assert!(modal.to_string().contains(DETAILS_SHORTENED));
    }
    #[test]
    fn pending_human_decision_survives_peer_reply_and_preserves_requester() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut c = Config::new("Test".into(), "owner".into());
        c.enabled = true;
        c.slack.team_id = "Ttest".into();
        c.attention.settle_ms = 0;
        let source = crate::model::Message {
            team: "Ttest".into(),
            channel: "Ctest".into(),
            thread: "".into(),
            ts: "123.0".into(),
            revision: "123.0".into(),
            sender: "Uhuman".into(),
            bot: false,
            text: "Build the assigned result".into(),
            deleted: false,
            files: vec![],
        };
        store.ingest("human", &source, true).unwrap();
        let job = store.claim(&c.attention).unwrap().unwrap();
        store.db.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,channel,thread,body) VALUES('approval',?1,?2,'session','Uhuman',?3,?4,?5,'pending','Ctest','',?6)", params![job.id, job.context, c.revision as i64, now_ms(), now_ms()+60000, json!({"options":[{"optionId":"once","kind":"allow_once"}]}).to_string()]).unwrap();
        let mut peer = source.clone();
        peer.ts = "124.0".into();
        peer.revision = peer.ts.clone();
        peer.sender = "Ureviewer".into();
        peer.bot = true;
        peer.text = "I will review your result".into();
        store.ingest("peer", &peer, true).unwrap();
        expire(&store, &c).unwrap();
        assert!(live(&store, &c, "approval").unwrap());
        resolve(&store, &c, "approval", Some("once"), "owner", None).unwrap();
        let value = current(&store, "approval").unwrap().unwrap();
        assert_eq!(value["status"], "approved");
        assert_eq!(value["approver"], "Uhuman");
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
