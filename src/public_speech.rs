//! Completed public utterances have a durable append-only delivery path.
//! Activity is mutable; this module never updates or deletes a Slack message.
use crate::{
    config::Config,
    model::{Message, now_ms},
    slack::{ApiError, Slack},
    store::{Outgoing, Store},
};
use amplifier_acp_connection::speech::Utterance;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

pub fn initialize(s: &Store) -> Result<()> {
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS public_speech(
        id TEXT PRIMARY KEY,job_id TEXT NOT NULL,message_id TEXT NOT NULL,part INTEGER NOT NULL,
        context TEXT NOT NULL,channel TEXT NOT NULL,thread TEXT NOT NULL,text TEXT NOT NULL,
        status TEXT NOT NULL,created INTEGER NOT NULL,available INTEGER NOT NULL,
        attempted INTEGER NOT NULL DEFAULT 0,slack_ts TEXT,note TEXT NOT NULL DEFAULT '',
        checked INTEGER NOT NULL DEFAULT 0,UNIQUE(job_id,message_id,part));
        CREATE INDEX IF NOT EXISTS public_speech_status ON public_speech(status,created);
        CREATE VIEW IF NOT EXISTS reply_activity AS
        SELECT job_id,context,MIN(attempted) AS attempted FROM (
            SELECT job_id,context,attempted FROM outbox WHERE attempted>0 AND status IN ('ready','sent','sending','uncertain')
            UNION ALL
            SELECT job_id,context,attempted FROM public_speech WHERE attempted>0 AND status IN ('ready','sent','sending','uncertain')
        ) GROUP BY job_id,context;")?;
    Ok(())
}

pub fn enqueue(s: &Store, job: &str, utterance: &Utterance) -> Result<()> {
    if utterance.text.trim().is_empty() {
        return Ok(());
    }
    ensure!(utterance.id.len() <= 1024, "Public message ID too long");
    let (_context, raw): (String, String) =
        s.db.query_row("SELECT context,trigger FROM jobs WHERE id=?1", [job], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let m: Message = serde_json::from_str(&raw)?;
    let tx = rusqlite::Transaction::new_unchecked(&s.db, rusqlite::TransactionBehavior::Immediate)?;
    enqueue_routed(
        &tx,
        job,
        &utterance.id,
        &utterance.text,
        &m.channel,
        &m.thread,
    )?;
    tx.commit()?;
    Ok(())
}

/// Reuse the ordinary speech journal for an installation-resolved destination.
/// The caller owns a transaction and must bind any route change to its source.
pub(crate) fn enqueue_routed(
    db: &Connection,
    job: &str,
    message_id: &str,
    text: &str,
    channel: &str,
    thread: &str,
) -> Result<Vec<String>> {
    let context: String =
        db.query_row("SELECT context FROM jobs WHERE id=?1", [job], |r| r.get(0))?;
    let parts = crate::reply_parts::split(text)?;
    let mut ids = Vec::new();
    for (part, text) in parts.iter().enumerate() {
        let id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            serde_json::to_string(&(job, message_id, part))?.as_bytes(),
        )
        .to_string();
        let existing: Option<(String, String, String)> = db
            .query_row(
                "SELECT text,channel,thread FROM public_speech WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        ensure!(
            existing.as_ref().is_none_or(
                |old| old == &(text.markdown.clone(), channel.to_owned(), thread.to_owned())
            ),
            "A completed public message or destination cannot be revised"
        );
        db.execute("INSERT OR IGNORE INTO public_speech(id,job_id,message_id,part,context,channel,thread,text,status,created,available) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'ready',?9,?9)",params![id,job,message_id,part as i64,context,channel,thread,text.markdown,now_ms()])?;
        ids.push(id);
    }
    db.execute(
        "INSERT INTO audit(time,kind,data) VALUES(?1,'public_speech_ready',?2)",
        params![
            now_ms(),
            json!({"job_id":job,"parts":parts.len(),"bytes":text.len()}).to_string()
        ],
    )?;
    Ok(ids)
}

pub(crate) fn reserved(db: &Connection, job: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE job_id=?1 AND attempted>0 UNION ALL SELECT 1 FROM public_speech WHERE job_id=?1 AND attempted>0)",[job],|r|r.get(0))?)
}

pub fn claim(s: &mut Store, c: &Config) -> Result<Option<Outgoing>> {
    let tx =
        s.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let next:Option<(Outgoing,String,String,i64,i64,String)>=tx.query_row(
        "SELECT p.id,p.channel,p.thread,p.text,p.job_id,j.trigger,j.version,c.control_version,j.status
        FROM public_speech p JOIN jobs j ON j.id=p.job_id JOIN contexts c ON c.id=p.context
        WHERE p.status='ready' AND p.available<=?1
        AND NOT EXISTS(SELECT 1 FROM public_speech prior WHERE prior.job_id=p.job_id AND prior.rowid<p.rowid AND prior.status IN ('ready','sending'))
        ORDER BY p.rowid LIMIT 1",[now_ms()],|r|Ok((Outgoing{id:r.get(0)?,channel:r.get(1)?,thread:r.get(2)?,text:r.get(3)?,media:vec![]},r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
    let Some((out, job, raw, version, current, state)) = next else {
        return Ok(None);
    };
    let trigger: Message = serde_json::from_str(&raw)?;
    let source: Option<String> = tx
        .query_row(
            "SELECT body FROM messages WHERE channel=?1 AND ts=?2",
            params![trigger.channel, trigger.ts],
            |r| r.get(0),
        )
        .optional()?;
    let source = source
        .map(|raw| serde_json::from_str::<Message>(&raw))
        .transpose()?;
    let allowed = crate::huddles::delivery_current(&tx, c, &out.id)?
        && c.enabled
        && c.allows(&out.channel, "")
        && version >= current
        && !matches!(state.as_str(), "superseded" | "cancelled")
        && source.is_some_and(|m| {
            !m.deleted && m.revision == trigger.revision && c.should_wake(&m.channel, &m.text)
        });
    if !allowed {
        tx.execute("UPDATE public_speech SET status='cancelled',note='Source or connection changed before delivery' WHERE job_id=?1 AND status='ready'",[&job])?;
        tx.commit()?;
        return Ok(None);
    }
    let now = now_ms();
    if !reserved(&tx, &job)? {
        if let Some((why, until)) = crate::store::delivery_wait(&tx, &trigger, &c.attention, now)? {
            tx.execute(
                "UPDATE public_speech SET available=?2,note=?3 WHERE id=?1",
                params![out.id, until.unwrap_or(now + 30_000), why],
            )?;
            tx.commit()?;
            return Ok(None);
        }
        crate::store::reserve_reply(&tx, &trigger, &c.attention, now)?;
    }
    tx.execute("UPDATE public_speech SET status='sending',attempted=CASE WHEN attempted=0 THEN ?2 ELSE attempted END,note='' WHERE id=?1",params![out.id,now])?;
    tx.commit()?;
    Ok(Some(out))
}

pub fn settle(
    s: &Store,
    id: &str,
    status: &str,
    ts: Option<&str>,
    note: &str,
    delay: i64,
) -> Result<()> {
    // An echo may already have settled the write while its HTTP reply was lost.
    s.db.execute("UPDATE public_speech SET status=?2,slack_ts=COALESCE(?3,slack_ts),note=?4,available=?5 WHERE id=?1 AND status='sending'",params![id,status,ts,note,now_ms()+delay])?;
    s.audit(
        "public_speech_delivery",
        json!({"id":id,"status":status,"ts":ts}),
    )
}
pub fn echo(s: &Store, id: &str, ts: &str) -> Result<()> {
    s.db.execute("UPDATE public_speech SET status='sent',slack_ts=?2,note='Confirmed by Slack event or history' WHERE id=?1 AND status IN ('sending','uncertain')",params![id,ts])?;
    Ok(())
}
pub fn recover(s: &Store) -> Result<()> {
    s.db.execute("UPDATE public_speech SET status='uncertain',note='Process stopped during send; awaiting delivery evidence' WHERE status='sending'",[])?;
    Ok(())
}

pub async fn pump(
    home: &Path,
    client: &Slack,
    store: &Arc<Mutex<Store>>,
    c: &Config,
) -> Result<bool> {
    let next = claim(&mut store.lock().unwrap(), c)?;
    let Some(out) = next else {
        return Ok(false);
    };
    // Huddle returns retain their original audience even when the current turn
    // was caused by a peer in a separate Slack thread. A failed read is not a send.
    if crate::huddles::authorize_delivery(home, c, &out.id, client)
        .await
        .is_err()
    {
        let s = Store::open(home)?;
        settle(
            &s,
            &out.id,
            "ready",
            None,
            "Original huddle source or audience could not be verified; not sent",
            30_000,
        )?;
        return Ok(true);
    }
    let metadata = {
        let s = Store::open(home)?;
        crate::huddles::delivery_metadata(&s.db, &out.id)?
    };
    let result = client.publish_with_metadata(&out, metadata.as_ref()).await;
    let s = Store::open(home)?;
    match result {
        Ok(ts) => settle(&s, &out.id, "sent", Some(&ts), "Confirmed by Slack", 0)?,
        Err(ApiError::RateLimited(delay)) => settle(
            &s,
            &out.id,
            "ready",
            None,
            "Slack rate limit",
            (delay.max(1) * 1000) as i64,
        )?,
        Err(ApiError::Rejected(_)) => settle(
            &s,
            &out.id,
            "failed",
            None,
            "Slack rejected public message",
            0,
        )?,
        Err(ApiError::Uncertain) => settle(
            &s,
            &out.id,
            "uncertain",
            None,
            "Delivery uncertain; no resend",
            0,
        )?,
    }
    Ok(true)
}
pub async fn reconcile(home: &Path, client: &Slack) -> Result<()> {
    let s = Store::open(home)?;
    let rows=s.db.prepare("SELECT id,channel,thread,created FROM public_speech WHERE status='uncertain' AND checked<?1 ORDER BY created LIMIT 2")?.query_map([now_ms()-30_000],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, channel, thread, created) in rows {
        s.db.execute(
            "UPDATE public_speech SET checked=?2 WHERE id=?1",
            params![id, now_ms()],
        )?;
        if let Ok(Some(ts)) = client.find_message(&channel, &thread, &id, created).await {
            echo(&s, &id, &ts)?;
        }
    }
    Ok(())
}
