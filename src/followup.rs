//! Bounded continuations belong to the source conversation and its existing
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
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS followups(parent TEXT PRIMARY KEY,root TEXT NOT NULL,depth INTEGER NOT NULL,deadline INTEGER NOT NULL,available INTEGER NOT NULL,goal TEXT NOT NULL,endpoint TEXT NOT NULL,child TEXT UNIQUE,status TEXT NOT NULL DEFAULT 'proposed');")?;
    Ok(())
}
/// A final response can request one later turn. Capture it before finishing the
/// job; a proposal is not eligible until that response was durably delivered.
pub fn capture(home: &Path, c: &Config, input: &Value, output: &str) -> Result<String> {
    let Some((answer, tail)) = output.rsplit_once("\nSLACK_FOLLOW_UP ") else {
        return Ok(output.into());
    };
    let request: Value = serde_json::from_str(tail.trim()).unwrap_or(Value::Null);
    let seconds = request["after_seconds"].as_i64().unwrap_or(0);
    let goal = request["goal"].as_str().unwrap_or("");
    if !(5..=3600).contains(&seconds) || goal.trim().is_empty() || goal.len() > 2000 {
        return Ok(notice(
            answer,
            "The follow-up request was invalid, so no later turn was scheduled.",
        ));
    }
    let id = input["job_id"].as_str().unwrap_or("");
    let s = Store::open(home)?;
    initialize(&s)?;
    let trigger: Option<String> =
        s.db.query_row(
            "SELECT trigger FROM jobs WHERE id=?1 AND status='running'",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(trigger) = trigger else {
        return Ok(answer.into());
    };
    let trigger: Message = serde_json::from_str(&trigger)?;
    // Bots may collaborate, but cannot manufacture an unbounded autonomous loop.
    if trigger.bot {
        return Ok(notice(
            answer,
            "No automatic follow-up was scheduled for this agent-to-agent message.",
        ));
    }
    let prior: Option<(String, i64, i64)> =
        s.db.query_row(
            "SELECT root,depth,deadline FROM followups WHERE child=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (root, depth, deadline) = prior.unwrap_or((id.into(), 0, now_ms() + 86_400_000));
    if depth >= 8 || now_ms() + seconds * 1000 >= deadline {
        s.audit(
            "continuation_limit",
            json!({"job":id,"reason":"Continuation budget exhausted"}),
        )?;
        return Ok(notice(
            answer,
            "The automatic continuation budget is exhausted. Further work needs a new request.",
        ));
    }
    s.db.execute("INSERT OR IGNORE INTO followups(parent,root,depth,deadline,available,goal,endpoint) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,root,depth+1,deadline,now_ms()+seconds*1000,goal,crate::driver::endpoint_key(c)])?;
    Ok(answer.into())
}
pub fn input(home: &Path, id: &str) -> Result<Option<Value>> {
    let s = Store::open(home)?;
    initialize(&s)?;
    Ok(s.db.query_row("SELECT goal,depth,deadline FROM followups WHERE child=?1",[id],|r|Ok(json!({"goal":r.get::<_,String>(0)?,"turn":r.get::<_,i64>(1)?,"maximum_turns":8,"deadline":r.get::<_,i64>(2)?,"instructions":"Continue only the already accepted goal. Inspect saved results and actual delivery receipts, then take the next concrete step. This wake-up grants no additional authority."}))).optional()?)
}
pub fn pump(home: &Path, c: &Config) -> Result<()> {
    let mut s = Store::open(home)?;
    initialize(&s)?;
    let tx =
        s.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let rows=tx.prepare("SELECT f.parent,f.endpoint,f.deadline,j.context,j.trigger,j.version FROM followups f JOIN jobs j ON j.id=f.parent WHERE f.status='proposed' AND f.available<=?1 AND EXISTS(SELECT 1 FROM outbox o WHERE o.job_id=j.id AND o.status='sent')")?.query_map([now_ms()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (parent, endpoint, deadline, context, trigger, version) in rows {
        let m: Message = serde_json::from_str(&trigger)?;
        let latest: i64 = tx.query_row(
            "SELECT human_version FROM contexts WHERE id=?1",
            [&context],
            |r| r.get(0),
        )?;
        if deadline <= now_ms()
            || endpoint != crate::driver::endpoint_key(c)
            || latest > version
            || !c.should_wake(&m.channel, &m.text)
            || m.bot
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
        tx.execute("INSERT INTO jobs(id,context,trigger,status,created,updated,version,note,available) VALUES(?1,?2,?3,'pending',?4,?4,?5,'Bounded continuation of accepted work',?4)",params![child,context,trigger,now_ms(),version])?;
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
