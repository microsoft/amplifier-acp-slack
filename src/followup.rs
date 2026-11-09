//! Accepted-work continuations belong to the source conversation and its existing
//! endpoint. This is a delivery policy over ordinary ACP, not a new agent API.
use crate::{
    config::Config,
    model::{Message, now_ms},
    store::Store,
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

pub fn initialize(s: &Store) -> Result<()> {
    // Additive storage keeps old releases able to read this journal on rollback.
    // Existing rows retain their finite bounds; new rows explicitly opt out.
    let tx = rusqlite::Transaction::new_unchecked(&s.db, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS followups(parent TEXT PRIMARY KEY,root TEXT NOT NULL,depth INTEGER NOT NULL,deadline INTEGER NOT NULL,available INTEGER NOT NULL,goal TEXT NOT NULL,endpoint TEXT NOT NULL,child TEXT UNIQUE,status TEXT NOT NULL DEFAULT 'proposed');")?;
    let columns = tx
        .prepare("PRAGMA table_info(followups)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, definition) in [
        ("source_version", "INTEGER NOT NULL DEFAULT -1"),
        ("legacy_bounds", "INTEGER NOT NULL DEFAULT 1"),
    ] {
        if !columns.iter().any(|column| column == name) {
            tx.execute(
                &format!("ALTER TABLE followups ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
/// Both the MCP tool and the legacy footer use this one durable continuation.
/// It cannot start until the current turn has a confirmed terminal. Scheduling
/// is independent of whether that turn has anything useful to publish.
pub fn schedule(home: &Path, c: &Config, id: &str, seconds: i64, goal: &str) -> Result<Value> {
    let mut s = Store::open(home)?;
    let tx =
        s.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let trigger: Option<String> = tx
        .query_row(
            "SELECT trigger FROM jobs WHERE id=?1 AND status='running' AND EXISTS(SELECT 1 FROM work_handoffs WHERE job=?1)",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(trigger) = trigger else {
        return Ok(
            json!({"scheduled":false,"reason":"No accepted response is active for this follow-up."}),
        );
    };
    let trigger: Message = serde_json::from_str(&trigger)?;
    if seconds == 0 {
        tx.execute(
            "UPDATE followups SET status='withdrawn' WHERE parent=?1 AND status='proposed'",
            [id],
        )?;
        tx.commit()?;
        return Ok(json!({"scheduled":false,"cancelled":true}));
    }
    if !(5..=3600).contains(&seconds) || goal.trim().is_empty() || goal.len() > 2000 {
        return Ok(
            json!({"scheduled":false,"reason":"The follow-up request was invalid, so no later turn was scheduled."}),
        );
    }
    let prior: Option<(String, i64, i64, bool)> = tx
        .query_row(
            "SELECT root,depth,deadline,legacy_bounds FROM followups WHERE child=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    // The old deadline remains a finite rollback fallback, not a bound in this
    // version when legacy_bounds is false. No large numeric sentinel is used.
    let (root, depth, deadline, legacy_bounds) =
        prior.unwrap_or((id.into(), 0, now_ms() + 86_400_000, false));
    if legacy_bounds && (depth >= 8 || now_ms() + seconds * 1000 >= deadline) {
        tx.execute(
            "INSERT INTO audit(time,kind,data) VALUES(?1,'continuation_limit',?2)",
            params![
                now_ms(),
                json!({"job":id,"reason":"Continuation budget exhausted"}).to_string()
            ],
        )?;
        tx.commit()?;
        return Ok(
            json!({"scheduled":false,"reason":"The automatic continuation budget is exhausted. Further work needs a new request."}),
        );
    }
    let unsettled:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs j WHERE j.context=?1 AND j.status='pending' AND NOT json_extract(j.trigger,'$.bot')) OR EXISTS(SELECT 1 FROM steering_inputs WHERE target_job=?2 AND state IN ('offering','submitted','received','remoteQueued','unknown','notAccepted','failed','startedNewTurn','held'))",params![trigger.context(),id],|r|r.get(0))?;
    if unsettled {
        return Ok(
            json!({"scheduled":false,"reason":"No automatic follow-up was scheduled while newer input is waiting or its delivery is unresolved."}),
        );
    }
    let source_version: i64 = tx.query_row(
        "SELECT human_version FROM contexts WHERE id=?1",
        [trigger.context()],
        |r| r.get(0),
    )?;
    // An identical call (including a lost acknowledgement) retains its original
    // wake time. A deliberate revision changes only this turn's single proposal.
    tx.execute("INSERT INTO followups(parent,root,depth,deadline,available,goal,endpoint,source_version,legacy_bounds) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(parent) DO UPDATE SET goal=excluded.goal,available=excluded.available,source_version=excluded.source_version,status='proposed' WHERE followups.child IS NULL AND (followups.status IN ('cancelled','withdrawn') OR followups.goal!=excluded.goal)",params![id,root,depth+1,deadline,now_ms()+seconds*1000,goal,crate::driver::endpoint_key(c),source_version,legacy_bounds])?;
    let available: i64 = tx.query_row(
        "SELECT available FROM followups WHERE parent=?1",
        [id],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(json!({"scheduled":true,"available_at":available,"goal":goal,"after_current_turn":true}))
}

/// Compatibility for plain ACP endpoints without the context MCP tools.
pub fn capture(home: &Path, c: &Config, input: &Value, output: &str) -> Result<String> {
    let Some((answer, tail)) = output.rsplit_once("\nSLACK_FOLLOW_UP ") else {
        return Ok(output.into());
    };
    let request: Value = serde_json::from_str(tail.trim()).unwrap_or(Value::Null);
    let result = schedule(
        home,
        c,
        input["job_id"].as_str().unwrap_or(""),
        request["after_seconds"].as_i64().unwrap_or(-1),
        request["goal"].as_str().unwrap_or(""),
    )?;
    if result["scheduled"] == true || result["cancelled"] == true {
        Ok(answer.into())
    } else {
        // Private control failures must not turn a peer pass into public
        // bookkeeping. Keep the failure available to the installation instead.
        let s = Store::open(home)?;
        let id = input["job_id"].as_str().unwrap_or("");
        let peer: bool = s.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND json_extract(trigger,'$.bot'))",
            [id],
            |r| r.get(0),
        )?;
        if peer {
            s.audit(
                "continuation_not_scheduled",
                json!({"job":id,"reason":result["reason"]}),
            )?;
            return Ok(answer.into());
        }
        Ok(notice(
            answer,
            result["reason"]
                .as_str()
                .unwrap_or("No follow-up was scheduled."),
        ))
    }
}

/// Offer the control only where the scheduler can honor its source authority.
/// An absent/unknown source does not establish accepted work.
pub fn guidance(input: &Value) -> &'static str {
    if !input["trigger"]["bot"].is_boolean() {
        return "";
    }
    "When accepted work is still running or a result needs checking after this response, schedule that check with slack_follow_up before ending. Inspect the existing run and report success, failure or a real blocker; do not launch it again. A promise to return does not schedule anything. You can remain silent while waiting. If that tool is unavailable, append SLACK_FOLLOW_UP {\"after_seconds\":60,\"goal\":\"next concrete step\"} on its own final line, including after a pass JSON response. Delay 5–3600 seconds. Continue the accepted goal until done, stopped or genuinely blocked, respecting explicit user limits and any retained continuation bounds. New human input supersedes queued follow-ups: reassess unfinished commitments. Peer-origin work retains its original source and permissions; a follow-up grants no new authority. Do not schedule turns just to announce delivery, agreement, record closure or continued availability."
}
pub fn input(home: &Path, id: &str) -> Result<Option<Value>> {
    let s = Store::open(home)?;
    Ok(s.db.query_row("SELECT goal,depth,deadline,legacy_bounds FROM followups WHERE child=?1",[id],|r|Ok(json!({"goal":r.get::<_,String>(0)?,"turn":r.get::<_,i64>(1)?,"maximum_turns":r.get::<_,bool>(3)?.then_some(8),"deadline":r.get::<_,bool>(3)?.then_some(r.get::<_,i64>(2)?),"instructions":"This is a follow-up, not a replay of the original request. Continue only the already accepted goal. Inspect saved results and actual delivery receipts, then take the next concrete step. This wake-up grants no additional authority."}))).optional()?)
}
pub fn pump(home: &Path, c: &Config) -> Result<()> {
    let mut s = Store::open(home)?;
    let tx =
        s.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let rows=tx.prepare("SELECT f.parent,f.endpoint,CASE WHEN f.legacy_bounds THEN f.deadline ELSE NULL END,j.context,j.trigger,CASE WHEN f.source_version>=0 THEN f.source_version ELSE j.version END FROM followups f JOIN jobs j ON j.id=f.parent WHERE f.status='proposed' AND f.available<=?1 AND j.status IN ('speak','pass')")?.query_map([now_ms()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (parent, endpoint, deadline, context, trigger, version) in rows {
        let m: Message = serde_json::from_str(&trigger)?;
        let unresolved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM steering_inputs WHERE target_job=?1 AND state IN ('offering','submitted','received','remoteQueued','unknown','notAccepted','failed','startedNewTurn','held'))",[&parent],|r|r.get(0))?;
        if unresolved {
            continue;
        }
        let latest: i64 = tx.query_row(
            "SELECT human_version FROM contexts WHERE id=?1",
            [&context],
            |r| r.get(0),
        )?;
        if deadline.is_some_and(|limit| limit <= now_ms())
            || endpoint != crate::driver::endpoint_key(c)
            || latest > version
            || !c.should_wake(&m.channel, &m.text)
        {
            tx.execute(
                "UPDATE followups SET status='cancelled' WHERE parent=?1",
                [&parent],
            )?;
            continue;
        }
        // Never overlap another turn or invent a new source actor/conversation.
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE context=?1 AND status IN ('running','pending'))",[&context],|r|r.get(0))?;
        if busy {
            continue;
        }
        let child = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO jobs(id,context,trigger,status,created,updated,version,note,available) VALUES(?1,?2,?3,'pending',?4,?4,?5,'Continuation of accepted work',?4)",params![child,context,trigger,now_ms(),version])?;
        tx.execute(
            "UPDATE followups SET status='queued',child=?2 WHERE parent=?1",
            params![parent, child],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn notice(answer: &str, text: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<Value>(answer)
        && value["decision"] == "speak"
    {
        value["text"] = json!(format!(
            "{}\n\n{}",
            value["text"].as_str().unwrap_or(""),
            text
        ));
        value.to_string()
    } else {
        format!("{answer}\n\n{text}")
    }
}
