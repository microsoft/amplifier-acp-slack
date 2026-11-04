//! Slack lifecycle indicators derived from committed response work.
//! Native thread status or a scoped temporary message, with durable delivery intent.
use crate::{
    config::Config,
    model::now_ms,
    slack::{ApiError, Slack},
    store::{Job, Store},
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::OnceLock,
    time::Duration,
};
use tokio::sync::Notify;

fn notification() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}
pub fn changed(_home: &Path) {
    notification().notify_one();
}

fn initialize(s: &Store) -> Result<()> {
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS work_status_jobs(job_id TEXT PRIMARY KEY,channel TEXT NOT NULL,thread TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS work_status_jobs_target ON work_status_jobs(channel,thread);
        CREATE TABLE IF NOT EXISTS work_status_delivery(channel TEXT NOT NULL,thread TEXT NOT NULL,desired TEXT NOT NULL DEFAULT 'active',confirmed TEXT NOT NULL DEFAULT 'active',dirty INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,attempts INTEGER NOT NULL DEFAULT 0,blocked INTEGER NOT NULL DEFAULT 0,reason TEXT NOT NULL DEFAULT '',updated INTEGER NOT NULL,PRIMARY KEY(channel,thread));
        CREATE TABLE IF NOT EXISTS work_status_control(id INTEGER PRIMARY KEY CHECK(id=1),credential_key TEXT NOT NULL,disabled INTEGER NOT NULL DEFAULT 0,reason TEXT NOT NULL DEFAULT '',retry_at INTEGER NOT NULL DEFAULT 0);")?;
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS response_indicators(job_id TEXT PRIMARY KEY,channel TEXT NOT NULL,thread TEXT NOT NULL,client_id TEXT NOT NULL,slack_ts TEXT,attempted INTEGER NOT NULL DEFAULT 0,state TEXT NOT NULL DEFAULT 'waiting',phase TEXT NOT NULL DEFAULT 'Thinking…',shown TEXT NOT NULL DEFAULT '',retry_at INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL);")?;
    // Existing databases retain their original column default. Normalize only
    // generic live placeholders; preserve tool labels and historical delivery text.
    s.db.execute("UPDATE response_indicators SET phase='Thinking…' WHERE state<>'finished' AND phase IN ('Preparing the response…','Preparing the response...','Working on a response…','Working on the task…','Working on the task...')", [])?;
    Ok(())
}
/// Called only after a direct/addressed turn or accepted participation. The thread
/// root is presentation metadata; normal reply placement remains unchanged.
pub fn track(home: &Path, job: &Job) -> Result<()> {
    let mut s = Store::open(home)?;
    initialize(&s)?;
    let root = if job.trigger.thread.is_empty() {
        &job.trigger.ts
    } else {
        &job.trigger.thread
    };
    let tx = s.db.transaction()?;
    if !job.trigger.thread.is_empty() {
        tx.execute(
            "INSERT OR IGNORE INTO work_status_jobs VALUES(?1,?2,?3)",
            params![job.id, job.trigger.channel, root],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO work_status_delivery(channel,thread,updated) VALUES(?1,?2,?3)",
            params![job.trigger.channel, root, now_ms()],
        )?;
    }
    tx.execute("INSERT OR IGNORE INTO response_indicators(job_id,channel,thread,client_id,created,phase) VALUES(?1,?2,?3,?4,?5,'Thinking…')",params![job.id,job.trigger.channel,job.trigger.thread,uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL,format!("work-status:{}",job.id).as_bytes()).to_string(),now_ms()])?;
    tx.commit()?;
    changed(home);
    Ok(())
}
fn retire_settled(s: &Store) -> Result<()> {
    let tx = s.db.unchecked_transaction()?;
    tx.execute("DELETE FROM work_status_jobs WHERE EXISTS(SELECT 1 FROM work_status_delivery d WHERE d.channel=work_status_jobs.channel AND d.thread=work_status_jobs.thread AND d.desired='active' AND d.confirmed='active' AND d.dirty=0) AND job_id NOT IN (SELECT j.id FROM jobs j LEFT JOIN outbox o ON o.job_id=j.id WHERE j.status='running' OR (j.status='speak' AND o.status IN ('ready','sending')))",[])?;
    tx.execute("DELETE FROM work_status_delivery WHERE desired='active' AND confirmed='active' AND dirty=0 AND NOT EXISTS(SELECT 1 FROM work_status_jobs w WHERE w.channel=work_status_delivery.channel AND w.thread=work_status_delivery.thread)",[])?;
    tx.commit()?;
    Ok(())
}
fn credential_key(home: &Path, c: &Config) -> Result<String> {
    let secrets = c.secrets(home)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{}:{}:{}",
                c.revision,
                c.slack.api_base,
                c.slack.team_id,
                c.slack.bot_user_id,
                secrets.bot_token
            )
            .as_bytes()
        )
    ))
}
fn prepare(home: &Path, c: &Config, stopping: bool) -> Result<()> {
    let s = Store::open(home)?;
    initialize(&s)?;
    let key = credential_key(home, c)?;
    let old: Option<String> =
        s.db.query_row(
            "SELECT credential_key FROM work_status_control WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref() != Some(&key) {
        s.db.execute("INSERT INTO work_status_control(id,credential_key) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET credential_key=excluded.credential_key,disabled=0,reason=''",[key])?;
        s.db.execute("UPDATE work_status_delivery SET blocked=0,attempts=0", [])?;
    }
    crate::permissions::expire(&s, c)?;
    let mut waiting: HashSet<String> = s
        .db
        .prepare("SELECT DISTINCT job_id FROM permissions WHERE status='pending' AND expires>?1")?
        .query_map([now_ms()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    waiting.extend(crate::questions::waiting_job_ids(home, c)?);
    let targets =
        s.db.prepare("SELECT channel,thread,desired FROM work_status_delivery")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    for (channel, thread, old) in targets {
        let jobs=s.db.prepare("SELECT j.id,j.status,COALESCE(o.status,'') FROM work_status_jobs w JOIN jobs j ON j.id=w.job_id LEFT JOIN outbox o ON o.job_id=j.id WHERE w.channel=?1 AND w.thread=?2")?.query_map(params![channel,thread],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let desired = if stopping {
            "active"
        } else if jobs
            .iter()
            .any(|(id, status, _)| status == "running" && waiting.contains(id))
        {
            "suspended"
        } else if jobs.iter().any(|(_, status, outbox)| {
            status == "running"
                || (status == "speak" && ["ready", "sending"].contains(&outbox.as_str()))
        }) {
            "processing"
        } else {
            "active"
        };
        if desired != old {
            s.db.execute("UPDATE work_status_delivery SET desired=?3,updated=?4 WHERE channel=?1 AND thread=?2",params![channel,thread,desired,now_ms()])?;
        }
    }
    retire_settled(&s)?;
    Ok(())
}

/// Only pending transitions issue HTTP. Retries use durable deadlines, and a
/// workspace lacking this optional API stops attempts until config/access changes.
pub async fn reconcile(home: &Path, c: &Config, slack: &Slack, stopping: bool) -> Result<()> {
    prepare(home, c, stopping)?;
    for _ in 0..32 {
        let next = {
            let s = Store::open(home)?;
            let blocked: bool = s.db.query_row(
                "SELECT disabled=1 OR retry_at>?1 FROM work_status_control WHERE id=1",
                [now_ms()],
                |r| r.get(0),
            )?;
            if blocked {
                return Ok(());
            }
            let next:Option<(String,String,String,i64,bool)>=s.db.query_row("SELECT channel,thread,desired,attempts,dirty FROM work_status_delivery WHERE blocked=0 AND retry_at<=?1 AND (desired<>confirmed OR dirty=1) ORDER BY CASE desired WHEN 'active' THEN 0 WHEN 'suspended' THEN 1 ELSE 2 END,updated LIMIT 1",[now_ms()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            if let Some((channel, thread, _, _, _)) = &next {
                s.db.execute(
                    "UPDATE work_status_delivery SET dirty=1 WHERE channel=?1 AND thread=?2",
                    params![channel, thread],
                )?;
            }
            next
        };
        let Some((channel, thread, desired, attempts, previously_uncertain)) = next else {
            return Ok(());
        };
        let result = slack
            .call(
                "agents.sessions.setStatus",
                json!({"channel_id":channel,"thread_ts":thread,"status":desired}),
                false,
            )
            .await;
        let s = Store::open(home)?;
        match result {
            Ok(_) => {
                s.db.execute("UPDATE work_status_control SET reason='',retry_at=0 WHERE id=1 AND retry_at<=?1",[now_ms()])?;
                s.db.execute("UPDATE work_status_delivery SET confirmed=?3,dirty=0,retry_at=0,attempts=0,reason='' WHERE channel=?1 AND thread=?2",params![channel,thread,desired])?;
                s.audit(
                    "work_status_changed",
                    json!({"channel":channel,"thread":thread,"status":desired}),
                )?;
                if desired == "active" {
                    retire_settled(&s)?;
                }
            }
            Err(ApiError::RateLimited(seconds)) => {
                let delay = seconds.max(1).saturating_mul(1000).min(i64::MAX as u64) as i64;
                s.db.execute(
                    "UPDATE work_status_control SET retry_at=?1,reason='rate_limited' WHERE id=1",
                    [now_ms().saturating_add(delay)],
                )?;
                return Ok(());
            }
            Err(ApiError::Uncertain) => {
                let delay = (1_i64 << attempts.min(8)) * 1000;
                s.db.execute("UPDATE work_status_delivery SET retry_at=?3,attempts=attempts+1,reason='delivery_unconfirmed' WHERE channel=?1 AND thread=?2",params![channel,thread,now_ms()+delay])?;
            }
            Err(ApiError::Rejected(reason)) => {
                // An explicit rejection confirms only this call had no effect.
                // Preserve an earlier unconfirmed delivery: it may already have
                // changed Slack, and still needs successful active cleanup.
                s.db.execute(
                    "UPDATE work_status_delivery SET dirty=?3 WHERE channel=?1 AND thread=?2",
                    params![channel, thread, previously_uncertain],
                )?;
                if [
                    "unknown_method",
                    "feature_disabled",
                    "method_deprecated",
                    "deprecated_endpoint",
                    "missing_scope",
                    "invalid_auth",
                    "token_revoked",
                    "token_expired",
                    "not_authed",
                    "not_authorized",
                    "not_allowed_token_type",
                ]
                .contains(&reason.as_str())
                {
                    s.db.execute(
                        "UPDATE work_status_control SET disabled=1,reason=?1 WHERE id=1",
                        [&reason],
                    )?;
                    s.audit("work_status_unavailable", json!({"reason":reason}))?;
                    return Ok(());
                }
                s.db.execute("UPDATE work_status_delivery SET blocked=1,reason=?3 WHERE channel=?1 AND thread=?2",params![channel,thread,reason])?;
                s.audit(
                    "work_status_unavailable",
                    json!({"channel":channel,"thread":thread,"reason":reason}),
                )?;
            }
        }
    }
    Ok(())
}
pub async fn run(home: std::path::PathBuf, c: Config, slack: Slack) {
    loop {
        // The timer reconciles local durable job state and retry deadlines. It
        // never polls Slack or emits a status while the state is unchanged.
        let _ = reconcile(&home, &c, &slack, false).await;
        let _ = fallback_reconcile(&home, &slack, false).await;
        tokio::select! {_=notification().notified()=>{},_=tokio::time::sleep(Duration::from_millis(250))=>{}}
    }
}
pub async fn shutdown(home: &Path) {
    if let Ok(c) = Config::load(home)
        && let Ok(secrets) = c.secrets(home)
        && let Ok(slack) = Slack::new(&c, secrets)
    {
        // Persist desired cleanup even when the network is unavailable or a
        // rate limit outlasts shutdown. Restart reconciles the same records.
        let _ = prepare(home, &c, true);
        let _ =
            tokio::time::timeout(Duration::from_secs(3), reconcile(home, &c, &slack, true)).await;
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            fallback_reconcile(home, &slack, true),
        )
        .await;
    }
}
pub fn status(home: &Path) -> Value {
    let Ok(s) = Store::open(home) else {
        return json!({"available":false});
    };
    if initialize(&s).is_err() {
        return json!({"available":false});
    }
    s.db.query_row("SELECT disabled,reason,retry_at FROM work_status_control WHERE id=1",[],|r|Ok(json!({"available":!r.get::<_,bool>(0)?,"reason":r.get::<_,String>(1)?,"retry_at":r.get::<_,i64>(2)?}))).unwrap_or_else(|_|json!({"available":null}))
}

/// Presentation attaches to a committed reply, never global bot presence.
pub fn track_input(home: &Path, input: &Value) -> Result<()> {
    let s = Store::open(home)?;
    let id = input["job_id"].as_str().unwrap_or("");
    let job =
        s.db.query_row(
            "SELECT context,trigger,version FROM jobs WHERE id=?1 AND status='running'",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((context, trigger, version)) = job {
        track(
            home,
            &Job {
                id: id.into(),
                context,
                trigger: serde_json::from_str(&trigger)?,
                version,
                reconsidered: false,
            },
        )?;
    }
    Ok(())
}
#[derive(Default)]
pub struct ToolActivity {
    active: HashMap<String, ActiveTool>,
    sequence: u64,
}

#[derive(Default)]
struct ActiveTool {
    title: String,
    name: String,
    kind: String,
    status: String,
    sequence: u64,
}

fn single_line(text: &str, limit: usize) -> String {
    let text: String = text.chars().map(|c| {
        if c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') { ' ' } else { c }
    }).collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > limit {
        format!(
            "{}…",
            text.chars()
                .take(limit.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        text
    }
}

impl ToolActivity {
    /// ACP updates are partial. Keep public presentation fields across updates,
    /// never command arguments, tool output, or private reasoning content.
    pub fn update(&mut self, update: &Value) -> Option<String> {
        let id = update["toolCallId"].as_str()?;
        if id.is_empty() || id.len() > 1024 {
            return None;
        }
        let completed = matches!(update["status"].as_str(), Some("completed" | "failed"));
        if completed {
            self.active.remove(id)?;
        } else {
            if !self.active.contains_key(id) && update["sessionUpdate"] != "tool_call" {
                return None;
            }
            if self.active.len() >= 100 && !self.active.contains_key(id) {
                return None;
            }
            self.sequence += 1;
            let tool = self.active.entry(id.into()).or_default();
            for (key, target, limit) in [
                ("title", &mut tool.title, 110),
                ("name", &mut tool.name, 48),
                ("kind", &mut tool.kind, 24),
                ("status", &mut tool.status, 24),
            ] {
                if let Some(value) = update[key].as_str() {
                    *target = single_line(value, limit);
                }
            }
            tool.sequence = self.sequence;
        }
        let Some(tool) = self.active.values().max_by_key(|t| t.sequence) else {
            return Some(if update["status"] == "failed" {
                "Checking a tool failure…".into()
            } else {
                "Thinking…".into()
            });
        };
        let action = if tool.status == "pending" || tool.status.is_empty() {
            "Starting"
        } else {
            match tool.kind.as_str() {
                "read" => "Reading",
                "search" => "Searching",
                "fetch" => "Fetching",
                "edit" => "Editing",
                "delete" => "Deleting",
                "move" => "Moving",
                "execute" => "Running",
                "think" => "Thinking",
                "switch_mode" => "Switching mode",
                _ => "Using a tool",
            }
        };
        let title = if tool.kind == "think" {
            String::new()
        } else if tool.title.is_empty() {
            tool.name.clone()
        } else if !tool.name.is_empty()
            && tool.title.replace('_', " ").to_lowercase()
                != tool.name.replace('_', " ").to_lowercase()
        {
            format!("{} ({})", tool.title, tool.name)
        } else {
            tool.title.clone()
        };
        let label = if title.is_empty() {
            format!("{action}…")
        } else {
            format!("{action} · {title}")
        };
        let others = self.active.len() - 1;
        let suffix = if others == 0 {
            String::new()
        } else {
            format!(
                " · +{others} {}",
                if others == 1 { "tool" } else { "tools" }
            )
        };
        Some(format!(
            "{}{suffix}",
            single_line(&label, 180 - suffix.chars().count())
        ))
    }
}

pub fn phase(home: &Path, job: &str, text: &str) -> Result<()> {
    let s = Store::open(home)?;
    initialize(&s)?;
    s.db.execute(
        "UPDATE response_indicators SET phase=?2 WHERE job_id=?1 AND state<>'finished'",
        params![job, text],
    )?;
    changed(home);
    Ok(())
}

/// Fallback for ordinary Slack apps without agent-session status access. Its
/// post/update/delete calls use existing chat:write access and exact reply scope.
/// A lost POST receipt is looked up, never blindly repeated.
pub async fn fallback_reconcile(home: &Path, slack: &Slack, stopping: bool) -> Result<()> {
    let s = Store::open(home)?;
    initialize(&s)?;
    let rows=s.db.prepare("SELECT i.job_id,i.channel,i.thread,i.client_id,i.slack_ts,i.attempted,i.phase,i.shown,i.created,j.status,COALESCE(o.status,''),EXISTS(SELECT 1 FROM work_status_control WHERE disabled=1) OR EXISTS(SELECT 1 FROM work_status_jobs w JOIN work_status_delivery d ON d.channel=w.channel AND d.thread=w.thread WHERE w.job_id=i.job_id AND d.blocked=1) FROM response_indicators i JOIN jobs j ON j.id=i.job_id LEFT JOIN outbox o ON o.job_id=j.id WHERE i.state<>'finished' AND (i.state<>'failed_visible' OR (j.status NOT IN ('failed','interrupted') AND COALESCE(o.status,'') NOT IN ('failed','uncertain'))) AND i.retry_at<=?1 ORDER BY i.created LIMIT 32")?.query_map([now_ms()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,bool>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,i64>(8)?,r.get::<_,String>(9)?,r.get::<_,String>(10)?,r.get::<_,bool>(11)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (
        id,
        channel,
        thread,
        client_id,
        mut ts,
        attempted,
        phase,
        shown,
        created,
        job,
        outbox,
        unavailable,
    ) in rows
    {
        let failed = ["failed", "interrupted"].contains(&job.as_str())
            || ["failed", "uncertain"].contains(&outbox.as_str());
        let working =
            job == "running" || (job == "speak" && ["ready", "sending"].contains(&outbox.as_str()));
        let waiting = s.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM permissions WHERE job_id=?1 AND status='pending')",
            [&id],
            |r| r.get::<_, bool>(0),
        )? || crate::questions::waiting_job_ids(home, &Config::load(home)?)?
            .contains(&id);
        let text = if failed {
            "I couldn’t confirm this request finished. Some work or a reply may already exist; I’ll need to check that before repeating it."
        } else if job == "speak" {
            "Sending the response…"
        } else if waiting {
            "Waiting for your answer…"
        } else {
            &phase
        };
        if ts.is_none() && attempted {
            if let Ok(Some(found)) = slack
                .find_message(&channel, &thread, &client_id, created)
                .await
            {
                s.db.execute(
                    "UPDATE response_indicators SET slack_ts=?2 WHERE job_id=?1",
                    params![id, found],
                )?;
                ts = Some(found);
            } else {
                s.db.execute(
                    "UPDATE response_indicators SET retry_at=?2 WHERE job_id=?1",
                    params![id, now_ms() + 30_000],
                )?;
                continue;
            }
        }
        if stopping || (!working && !failed) {
            if let Some(ref ts) = ts {
                match slack
                    .call("chat.delete", json!({"channel":channel,"ts":ts}), false)
                    .await
                {
                    Ok(_) => {}
                    Err(ApiError::Rejected(reason)) if reason == "message_not_found" => {}
                    Err(_) => {
                        s.db.execute(
                            "UPDATE response_indicators SET retry_at=?2 WHERE job_id=?1",
                            params![id, now_ms() + 5000],
                        )?;
                        continue;
                    }
                }
            }
            s.db.execute(
                "UPDATE response_indicators SET state='finished' WHERE job_id=?1",
                [&id],
            )?;
            continue;
        }
        if !failed && !unavailable && !thread.is_empty() && ts.is_none() && phase == "Thinking…" {
            continue;
        }
        if ts.is_some() && shown == text {
            continue;
        }
        // Tool labels are display data, not Slack markup or mention instructions.
        let fallback_text = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let mut body = json!({"channel":channel,"text":fallback_text,"mrkdwn":false,"parse":"none","link_names":false,"blocks":[{"type":"section","text":{"type":"plain_text","text":text,"emoji":false}}],"unfurl_links":false,"unfurl_media":false,"metadata":{"event_type":"acp_work_status","event_payload":{"id":client_id}}});
        let method = if let Some(ref ts) = ts {
            body["ts"] = json!(ts);
            "chat.update"
        } else {
            body["client_msg_id"] = json!(client_id);
            if !thread.is_empty() {
                body["thread_ts"] = json!(thread);
            }
            s.db.execute(
                "UPDATE response_indicators SET attempted=1 WHERE job_id=?1",
                [&id],
            )?;
            "chat.postMessage"
        };
        match slack.call(method, body, false).await {
            Ok(value) => {
                let received = ts.or_else(|| value["ts"].as_str().map(str::to_owned));
                if let Some(ts) = received {
                    s.db.execute("UPDATE response_indicators SET slack_ts=?2,shown=?3,state=?4,retry_at=?5 WHERE job_id=?1",params![id,ts,text,if failed {"failed_visible"} else {"posted"},now_ms()+1500])?;
                }
            }
            Err(ApiError::RateLimited(seconds)) => {
                s.db.execute("UPDATE response_indicators SET attempted=CASE WHEN slack_ts IS NULL THEN 0 ELSE attempted END,retry_at=?2 WHERE job_id=?1",params![id,now_ms()+seconds.clamp(1,3600) as i64*1000])?;
            }
            Err(ApiError::Rejected(_)) => {
                s.db.execute(
                    "UPDATE response_indicators SET state='finished' WHERE job_id=?1",
                    [&id],
                )?;
                s.audit(
                    "response_indicator_unavailable",
                    json!({"job":id,"reason":"Slack rejected the scoped status message"}),
                )?;
            }
            Err(ApiError::Uncertain) => {
                s.db.execute(
                    "UPDATE response_indicators SET retry_at=?2 WHERE job_id=?1",
                    params![id, now_ms() + 5000],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upgraded_database_starts_with_thinking_and_preserves_specific_activity() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        initialize(&s).unwrap();
        let schema: String =
            s.db.query_row(
                "SELECT sql FROM sqlite_master WHERE name='response_indicators'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        s.db.execute_batch("DROP TABLE response_indicators")
            .unwrap();
        s.db.execute_batch(
            &schema.replace("DEFAULT 'Thinking…'", "DEFAULT 'Preparing the response…'"),
        )
        .unwrap();
        s.db.execute("INSERT INTO response_indicators(job_id,channel,thread,client_id,created) VALUES('old','Dtest','','old',0)", []).unwrap();
        s.db.execute("INSERT INTO response_indicators(job_id,channel,thread,client_id,created,phase) VALUES('tool','Dtest','','tool',0,'Reading · README.md')", []).unwrap();
        let trigger = serde_json::from_value(json!({"team":"Ttest","channel":"Dtest","thread":"","ts":"1.0","revision":"1","sender":"Utest","bot":false,"text":"Hello","deleted":false})).unwrap();
        track(
            dir.path(),
            &Job {
                id: "new".into(),
                context: "fixture".into(),
                trigger,
                version: 1,
                reconsidered: false,
            },
        )
        .unwrap();
        for id in ["old", "new"] {
            let phase: String =
                s.db.query_row(
                    "SELECT phase FROM response_indicators WHERE job_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(phase, "Thinking…");
        }
        let phase: String =
            s.db.query_row(
                "SELECT phase FROM response_indicators WHERE job_id='tool'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "Reading · README.md");
    }
}
