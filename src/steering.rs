//! Durable client ownership for optional, ordered ACP steering. No prompt replay
//! follows an ambiguous transmission, and input never changes its reply scope.
use crate::{
    model::{Message, now_ms},
    store::Store,
};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
pub const METHOD: &str = "_session/steering";
pub const RECEIPTS: &str = "io.github.bkrabach.agent-runtime";
pub const MAX_PENDING_AWARENESS: i64 = 256;
pub const MAX_PENDING_AWARENESS_BYTES: i64 = 4 * 1024 * 1024;
#[derive(Clone)]
pub struct Input {
    pub id: String,
    pub message: Message,
    pub awareness: bool,
}
pub fn supported(init: &Value) -> bool {
    init["_meta"]["steering"]["supported"] == true
}
pub fn awareness_supported(init: &Value) -> bool {
    let capability =
        &init["_meta"]["io.github.bkrabach.agent-instance"]["inputContinuity"]["awareness"];
    supported(init)
        && capability["version"] == 1
        && capability["activeOnly"] == true
        && capability["promptIdRequired"] == true
}
pub fn initialize(s: &Store) -> Result<()> {
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS steering_inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,input_id TEXT NOT NULL UNIQUE,context TEXT NOT NULL,source_id TEXT NOT NULL,revision TEXT NOT NULL,state TEXT NOT NULL,created INTEGER NOT NULL,target_job TEXT,session_id TEXT,receipt TEXT); CREATE INDEX IF NOT EXISTS steering_queue ON steering_inputs(context,seq);")?;
    let has_attention =
        s.db.prepare("PRAGMA table_info(steering_inputs)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|v| v.is_ok_and(|v| v == "attention_offer"));
    if !has_attention {
        s.db.execute(
            "ALTER TABLE steering_inputs ADD COLUMN attention_offer TEXT",
            [],
        )?;
    }
    for (name, definition) in [
        ("intent", "TEXT NOT NULL DEFAULT 'input'"),
        ("body", "TEXT"),
        ("reason", "TEXT NOT NULL DEFAULT ''"),
    ] {
        let exists =
            s.db.prepare("PRAGMA table_info(steering_inputs)")?
                .query_map([], |r| r.get::<_, String>(1))?
                .any(|v| v.is_ok_and(|v| v == name));
        if !exists {
            s.db.execute(
                &format!("ALTER TABLE steering_inputs ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}
pub fn next(s: &Store, context: &str, active: &str) -> Result<Option<Input>> {
    // An unresolved earlier submission blocks later steering; normal queued
    // prompts still have their own durable lifecycle after this one finishes.
    let row:Option<(String,String,bool)>=s.db.query_row("SELECT i.input_id,COALESCE(i.body,j.trigger),i.intent='awareness' FROM steering_inputs i LEFT JOIN jobs j ON j.id=i.input_id WHERE i.context=?1 AND ((i.intent='awareness' AND i.target_job=?2 AND i.state='queued') OR (i.intent='input' AND j.status='pending' AND i.state IN ('queued','promptRequired') AND COALESCE(i.target_job,'')<>?2)) ORDER BY i.seq LIMIT 1",params![context,active],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    row.map(|(id, body, awareness)| {
        Ok(Input {
            id,
            message: serde_json::from_str(&body)?,
            awareness,
        })
    })
    .transpose()
}
pub fn offering(s: &Store, id: &str, active: &str, sid: &str) -> Result<()> {
    // Admission itself consumes a source-trigger evaluation. A disconnect must
    // not blindly evaluate the same trigger again even before steering is sent.
    s.db.execute(
        "UPDATE steering_inputs SET state='offering',target_job=?2,session_id=?3 WHERE input_id=?1",
        params![id, active, sid],
    )?;
    Ok(())
}
pub fn submit(s: &Store, input: &Input, active: &str, sid: &str) -> Result<bool> {
    let tx = s.db.unchecked_transaction()?;
    if input.awareness {
        let n = tx.execute("UPDATE steering_inputs SET state='submitted',session_id=?3 WHERE input_id=?1 AND intent='awareness' AND target_job=?2 AND state='queued' AND EXISTS(SELECT 1 FROM jobs a JOIN contexts c ON c.id=a.context WHERE a.id=?2 AND a.context=steering_inputs.context AND a.status='running' AND a.version>=c.control_version) AND EXISTS(SELECT 1 FROM messages m WHERE m.channel=json_extract(steering_inputs.body,'$.channel') AND m.ts=json_extract(steering_inputs.body,'$.ts') AND m.revision=steering_inputs.revision AND NOT json_extract(m.body,'$.deleted'))", params![input.id,active,sid])?;
        tx.commit()?;
        if n == 1 {
            trace(s, &input.id, "submitted")?;
        }
        return Ok(n == 1);
    }
    let n=tx.execute("UPDATE jobs SET status='steering',version=(SELECT version FROM contexts WHERE id=jobs.context) WHERE id=?1 AND status='pending' AND EXISTS(SELECT 1 FROM jobs a WHERE a.id=?2 AND a.status='running' AND a.context=jobs.context AND a.version>=(SELECT control_version FROM contexts WHERE id=a.context)) AND EXISTS(SELECT 1 FROM messages m WHERE m.channel=json_extract(jobs.trigger,'$.channel') AND m.ts=json_extract(jobs.trigger,'$.ts') AND m.revision=json_extract(jobs.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted'))",params![input.id,active])?;
    if n == 1 {
        tx.execute("UPDATE steering_inputs SET state='submitted',target_job=?2,session_id=?3 WHERE input_id=?1",params![input.id,active,sid])?;
    }
    tx.commit()?;
    if n == 1 {
        trace(s, &input.id, "submitted")?;
    }
    Ok(n == 1)
}
pub fn defer(s: &Store, id: &str, active: &str) -> Result<()> {
    let tx = s.db.unchecked_transaction()?;
    tx.execute(
        "UPDATE steering_inputs SET state=CASE WHEN intent='awareness' THEN 'historyOnly' ELSE 'promptRequired' END,reason=CASE WHEN intent='awareness' THEN 'no_matching_active_prompt' ELSE reason END,target_job=?2 WHERE input_id=?1",
        params![id, active],
    )?;
    tx.execute(
        "UPDATE jobs SET status='pending' WHERE id=?1 AND status='steering'",
        [id],
    )?;
    tx.commit()?;
    trace(s, id, "promptRequired")
}
pub fn held(s: &Store, id: &str, active: &str) -> Result<()> {
    let tx = s.db.unchecked_transaction()?;
    tx.execute(
        "UPDATE steering_inputs SET state='held',target_job=?2 WHERE input_id=?1",
        params![id, active],
    )?;
    tx.execute("UPDATE jobs SET status='pass',note='Host attention held this update; it was not resubmitted' WHERE id=?1 AND status='pending'",[id])?;
    tx.commit()?;
    s.audit("steering_held", json!({"input_id":id}))
}
pub fn injected(s: &Store, id: &str, receipt: &Value) -> Result<()> {
    s.db.execute(
        "UPDATE steering_inputs SET state=CASE WHEN state IN ('incorporated','remoteQueued') THEN state WHEN ?3='remoteQueued' THEN 'remoteQueued' ELSE 'received' END,receipt=CASE WHEN state IN ('incorporated','remoteQueued') THEN receipt ELSE ?2 END WHERE input_id=?1 AND state IN ('submitted','received','remoteQueued','incorporated')",
        params![id, receipt.to_string(),receipt["state"].as_str().unwrap_or("")],
    )?;
    s.db.execute("UPDATE jobs SET status='steered',note='Input admitted to the existing ACP turn; incorporation is not yet confirmed' WHERE id=?1 AND status='steering'",[id])?;
    trace(s, id, "received")
}
pub fn fail(s: &Store, id: &str, state: &str) -> Result<()> {
    ensure!(
        ["failed", "unknown", "startedNewTurn", "notAccepted"].contains(&state),
        "Invalid steering failure"
    );
    let note = if state == "notAccepted" || state == "failed" {
        "The agent could not accept your update. It is saved but has not been sent again."
    } else {
        "Your update is saved, but I could not confirm whether it reached the running work. It was not sent again because that could repeat work."
    };
    failure_notice(s, id, state, note)
}
pub fn recovery_required(s: &Store, id: &str) -> Result<()> {
    failure_notice(
        s,
        id,
        "notAccepted",
        "The agent needs an explicit review of an earlier input whose outcome is uncertain before it can accept more work. This request was not run or retried. Review the agent's input receipts through its installation controls before continuing.",
    )
}
fn failure_notice(s: &Store, id: &str, state: &str, note: &str) -> Result<()> {
    let tx = s.db.unchecked_transaction()?;
    tx.execute(
        "UPDATE steering_inputs SET state=?2 WHERE input_id=?1",
        params![id, state],
    )?;
    tx.execute("UPDATE jobs SET status='failed',note=?2,version=(SELECT version FROM contexts WHERE id=jobs.context) WHERE id=?1",params![id,format!("Steering {state}; input was not automatically replayed")])?;
    tx.execute("INSERT OR IGNORE INTO outbox(id,job_id,context,channel,thread,text,status,created,available,connector_notice) SELECT ?1,id,context,json_extract(trigger,'$.channel'),json_extract(trigger,'$.thread'),?2,'ready',?3,?3,1 FROM jobs WHERE id=?1",params![id,note,now_ms()])?;
    tx.commit()?;
    trace(s, id, state)
}
pub fn receipt(s: &Store, active: &str, value: &Value) -> Result<()> {
    let Some(id) = value["inputId"].as_str() else {
        return Ok(());
    };
    let Some(state) = value["state"].as_str() else {
        return Ok(());
    };
    let matches:bool=s.db.query_row("SELECT EXISTS(SELECT 1 FROM steering_inputs WHERE input_id=?1 AND target_job=?2 AND state IN ('received','submitted','remoteQueued','incorporated'))",params![id,active],|r|r.get(0))?;
    if !matches {
        return Ok(());
    }
    if ["unknown", "notAccepted"].contains(&state) {
        return fail(s, id, state);
    }
    if !["received", "submitted", "remoteQueued", "incorporated"].contains(&state) {
        return Ok(());
    }
    // Do not let delayed/duplicate receipts move incorporated input backwards.
    s.db.execute("UPDATE steering_inputs SET state=?2,receipt=?3 WHERE input_id=?1 AND state<>'incorporated' AND (state<>'remoteQueued' OR ?2='incorporated')",params![id,state,json!({"inputId":id,"state":state,"turnId":value["turnId"],"sequence":value["sequence"]}).to_string()])?;
    trace(s, id, state)
}
pub fn recover(s: &Store) -> Result<()> {
    let ids=s.db.prepare("SELECT input_id FROM steering_inputs WHERE state IN ('offering','submitted','received','remoteQueued')")?.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        fail(s, &id, "unknown")?;
    }
    s.db.execute("UPDATE steering_inputs SET state='historyOnly' WHERE intent='awareness' AND state='queued'", [])?;
    Ok(())
}
pub fn abandon(s: &Store, active: &str) -> Result<()> {
    let ids=s.db.prepare("SELECT input_id FROM steering_inputs WHERE target_job=?1 AND state IN ('offering','submitted','received','remoteQueued')")?.query_map([active],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        fail(s, &id, "unknown")?;
    }
    retire_awareness(s, active)?;
    Ok(())
}
pub fn complete(s: &Store, active: &str) -> Result<()> {
    // A provider-side queue is not proof that the final turn used this input.
    // Preserve the explicit uncertainty and never turn it into replayable work.
    let queued =
        s.db.prepare(
            "SELECT input_id FROM steering_inputs WHERE target_job=?1 AND state='remoteQueued'",
        )?
        .query_map([active], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in queued {
        s.db.execute("UPDATE steering_inputs SET reason='remote_queue_unincorporated_at_end' WHERE input_id=?1", [&id])?;
        fail(s, &id, "unknown")?;
    }
    s.db.execute(
        "UPDATE steering_inputs SET state='completed' WHERE target_job=?1 AND state='received'",
        [active],
    )?;
    retire_awareness(s, active)?;
    Ok(())
}
pub fn history_only(s: &Store, id: &str) -> Result<()> {
    s.db.execute("UPDATE steering_inputs SET state='historyOnly',reason='not_submitted_to_active_turn' WHERE input_id=?1 AND intent='awareness' AND state='queued'", [id])?;
    trace(s, id, "historyOnly")
}
pub(crate) fn retire_awareness(s: &Store, active: &str) -> Result<()> {
    // A claimed job may still be evaluating attention, without an ACP prompt.
    // New peer evidence in that interval has never reached a model turn. Keep
    // one fresh candidate for ordinary attention instead of dropping the burst.
    // An attempted/ambiguous prompt is deliberately ineligible for this path.
    let tx = s.db.unchecked_transaction()?;
    let candidate: Option<(String, String, String, i64)> = tx.query_row(
        "SELECT i.input_id,i.context,i.body,i.created FROM steering_inputs i
         JOIN jobs j ON j.id=i.target_job JOIN contexts c ON c.id=i.context
         JOIN messages m ON m.channel=json_extract(i.body,'$.channel') AND m.ts=json_extract(i.body,'$.ts')
         WHERE i.target_job=?1 AND i.intent='awareness' AND i.state='queued'
         AND j.status='pass' AND j.version>=c.control_version
         AND NOT EXISTS(SELECT 1 FROM prompt_attempts p WHERE p.job_id=j.id)
         AND m.revision=i.revision AND NOT json_extract(m.body,'$.deleted')
         ORDER BY i.seq DESC LIMIT 1",
        [active], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
    ).optional()?;
    if let Some((id, context, body, created)) = candidate {
        tx.execute("INSERT INTO jobs(id,context,trigger,status,created,updated,note) VALUES(?1,?2,?3,'pending',?4,?5,'Fresh evidence arrived before ACP admission; evaluate attention again')", params![id,context,body,created,now_ms()])?;
        tx.execute("UPDATE steering_inputs SET intent='input',target_job=NULL,reason='attention_snapshot_changed_before_prompt' WHERE input_id=?1", [&id])?;
        tx.execute("INSERT INTO audit(time,kind,data) VALUES(?1,'attention_candidate_preserved',?2)", params![now_ms(),json!({"previous_job":active,"candidate_job":id,"context":context,"reason":"new_evidence_before_prompt","prompt_attempted":false}).to_string()])?;
    }
    tx.commit()?;
    let ids = s.db.prepare("SELECT input_id FROM steering_inputs WHERE intent='awareness' AND target_job=?1 AND state='queued'")?.query_map([active], |r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        history_only(s, &id)?;
    }
    Ok(())
}
/// Identifiers and receipt stages only: no source text or provider diagnostics.
fn trace(s: &Store, id: &str, stage: &str) -> Result<()> {
    let data:Option<Value> = s.db.query_row("SELECT intent,context,source_id,revision,target_job,json_extract(COALESCE(body,(SELECT trigger FROM jobs WHERE id=input_id)),'$.ts'),reason,state FROM steering_inputs WHERE input_id=?1",[id],|r|Ok(json!({"input_id":id,"intent":r.get::<_,String>(0)?,"context":r.get::<_,String>(1)?,"source_id":r.get::<_,String>(2)?,"revision":r.get::<_,String>(3)?,"active_job":r.get::<_,Option<String>>(4)?,"message_id":r.get::<_,String>(5)?,"reason":r.get::<_,String>(6)?,"retained_state":r.get::<_,String>(7)?,"stage":stage}))).optional()?;
    if let Some(data) = data {
        s.audit("steering_delivery", data)?;
    }
    Ok(())
}
pub fn coverage(s: &Store, active: &str, stage: &str, outbox: Option<&str>) -> Result<()> {
    let source: Option<(String,i64,i64,i64,String)> = s.db.query_row("SELECT j.context,j.version,c.version,c.control_version,json_extract(j.trigger,'$.ts') FROM jobs j JOIN contexts c ON c.id=j.context WHERE j.id=?1", [active], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((context, prompt_version, observed_version, control_version, trigger)) = source else {
        return Ok(());
    };
    let latest: Option<String> =
        s.db.query_row(
            "SELECT ts FROM messages WHERE context=?1 ORDER BY ts DESC LIMIT 1",
            [&context],
            |r| r.get(0),
        )
        .optional()?;
    let receipts = s.db.prepare("SELECT input_id,intent,state,revision,COALESCE(json_extract(body,'$.ts'),(SELECT json_extract(trigger,'$.ts') FROM jobs WHERE id=input_id)),reason FROM steering_inputs WHERE target_job=?1 ORDER BY seq DESC LIMIT 32")?.query_map([active], |r|Ok(json!({"input_id":r.get::<_,String>(0)?,"intent":r.get::<_,String>(1)?,"stage":r.get::<_,String>(2)?,"revision":r.get::<_,String>(3)?,"message_id":r.get::<_,String>(4)?,"reason":r.get::<_,String>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    s.audit("turn_context_coverage", json!({"active_job":active,"stage":stage,"outbox_id":outbox,"context":context,"trigger_message_id":trigger,"latest_message_id":latest,"prompt_context_version":prompt_version,"observed_context_version":observed_version,"control_version":control_version,"inputs":receipts,"max_inputs":32,"guaranteed_final_reconsideration":false}))
}
/// Tickets are private installation state. Only a definite backend idle result
/// may restore the original offer, and its expiry is never extended here.
pub fn save_attention(s: &Store, id: &str, offer: &Value) -> Result<()> {
    s.db.execute(
        "UPDATE steering_inputs SET attention_offer=?2 WHERE input_id=?1",
        params![id, offer.to_string()],
    )?;
    Ok(())
}
pub fn restore_attention(s: &Store, id: &str, result: &Value) -> Result<()> {
    let notice = &result["_meta"][crate::attention::META];
    if notice["ticketState"] != "restored" {
        return Ok(());
    }
    let raw: Option<String> = s.db.query_row(
        "SELECT attention_offer FROM steering_inputs WHERE input_id=?1",
        [id],
        |r| r.get(0),
    )?;
    if let Some(raw) = raw {
        let mut offer: Value = serde_json::from_str(&raw)?;
        if notice["id"] == offer["id"] && notice["expiresAt"].as_i64().is_some_and(|t| t > now_ms())
        {
            offer["restoredUntil"] = notice["expiresAt"].clone();
            save_attention(s, id, &offer)?;
        }
    }
    Ok(())
}
pub fn restored_attention(s: &Store, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> =
        s.db.query_row(
            "SELECT attention_offer FROM steering_inputs WHERE input_id=?1 AND state='prompt'",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(raw) = raw else { return Ok(None) };
    let offer: Value = serde_json::from_str(&raw)?;
    // This offer was already forwarded. Missing restoration is a consumed
    // ticket, not permission to request a new offer for the same source.
    ensure!(
        offer["restoredUntil"]
            .as_i64()
            .is_some_and(|t| t > now_ms()),
        "Attention offer was not restored or has expired; input was not resubmitted"
    );
    Ok(Some(json!({"id":offer["id"],"ticket":offer["ticket"]})))
}
pub fn status(s: &Store) -> Result<Value> {
    let counts =
        s.db.prepare("SELECT i.state,COUNT(*) FROM steering_inputs i GROUP BY i.state")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<std::collections::BTreeMap<_, _>>>()?;
    let oldest:Option<i64>=s.db.query_row("SELECT MIN(i.created) FROM steering_inputs i LEFT JOIN jobs j ON j.id=i.input_id WHERE j.status IN ('pending','steering') OR (i.intent='awareness' AND i.state IN ('queued','submitted'))",[],|r|r.get(0))?;
    Ok(json!({"states":counts,"oldest_queued_age_ms":oldest.map(|t|(now_ms()-t).max(0))}))
}
