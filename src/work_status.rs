//! Slack lifecycle indicators derived from actual addressed work, never timers.
//! Delivery intent is durable and idempotent. No model prompts or chat messages.
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
use std::{collections::HashSet, path::Path, sync::OnceLock, time::Duration};
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
    Ok(())
}
/// Called only after the worker identifies a direct/addressed turn. The thread
/// root is presentation metadata; normal reply placement remains unchanged.
pub fn track(home: &Path, job: &Job) -> Result<()> {
    let mut s = Store::open(home)?;
    initialize(&s)?;
    if s.db
        .query_row(
            "SELECT disabled FROM work_status_control WHERE id=1",
            [],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false)
    {
        return Ok(());
    }
    let root = if job.trigger.thread.is_empty() {
        &job.trigger.ts
    } else {
        &job.trigger.thread
    };
    let tx = s.db.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO work_status_jobs VALUES(?1,?2,?3)",
        params![job.id, job.trigger.channel, root],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO work_status_delivery(channel,thread,updated) VALUES(?1,?2,?3)",
        params![job.trigger.channel, root, now_ms()],
    )?;
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
