//! Bounded, conversation-local handoff of observed work between superseded turns.
//! These are observations and prior requests, never new permission grants.
use crate::{
    model::now_ms,
    store::{Job, Store},
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub fn initialize(s: &Store) -> Result<()> {
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS work_handoffs(job TEXT PRIMARY KEY,endpoint TEXT NOT NULL,request TEXT NOT NULL,inherited TEXT NOT NULL DEFAULT '[]',plan TEXT NOT NULL DEFAULT '[]',child TEXT); CREATE TABLE IF NOT EXISTS observed_tools(job TEXT NOT NULL,id TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(job,id));")?;
    Ok(())
}
fn bounded(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").chars().take(max).collect()
}

pub fn start(s: &Store, job: &str, endpoint: &str, input: &Value) -> Result<()> {
    let request = json!({"message_id":input["trigger"]["ts"],"sender":input["trigger"]["sender"],"text":bounded(&input["trigger"]["text"],8000)});
    s.db.execute(
        "INSERT OR IGNORE INTO work_handoffs(job,endpoint,request,inherited) VALUES(?1,?2,?3,?4)",
        params![
            job,
            endpoint,
            request.to_string(),
            input
                .get("work_handoff")
                .cloned()
                .unwrap_or(json!([]))
                .to_string()
        ],
    )?;
    if let Some(parents) = input["work_handoff"]["earlier_requests"].as_array() {
        for parent in parents {
            if let Some(id) = parent["predecessor_job"].as_str() {
                s.db.execute(
                    "UPDATE work_handoffs SET child=?2 WHERE job=?1",
                    params![id, job],
                )?;
            }
        }
    }
    Ok(())
}
pub fn observe(s: &Store, job: &str, u: &Value) -> Result<()> {
    if u["sessionUpdate"] == "plan" {
        let entries = u["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .take(24)
            .map(
                |e| json!({"content":bounded(&e["content"],600),"status":bounded(&e["status"],32)}),
            )
            .collect::<Vec<_>>();
        s.db.execute(
            "UPDATE work_handoffs SET plan=?2 WHERE job=?1",
            params![job, json!(entries).to_string()],
        )?;
    } else if matches!(
        u["sessionUpdate"].as_str(),
        Some("tool_call" | "tool_call_update")
    ) && let Some(id) = u["toolCallId"].as_str().filter(|s| s.len() <= 1024)
    {
        let old: Option<String> =
            s.db.query_row(
                "SELECT body FROM observed_tools WHERE job=?1 AND id=?2",
                params![job, id],
                |r| r.get(0),
            )
            .optional()?;
        let count: i64 = s.db.query_row(
            "SELECT COUNT(*) FROM observed_tools WHERE job=?1",
            [job],
            |r| r.get(0),
        )?;
        if old.is_none() && count >= 64 {
            return Ok(());
        }
        let mut body: Value = old
            .and_then(|x| serde_json::from_str(&x).ok())
            .unwrap_or(json!({"tool_call_id":id}));
        for key in ["title", "kind", "status"] {
            if u[key].is_string() {
                body[key] = json!(bounded(&u[key], 300));
            }
        }
        s.db.execute(
            "INSERT OR REPLACE INTO observed_tools VALUES(?1,?2,?3)",
            params![job, id, body.to_string()],
        )?;
    }
    Ok(())
}
pub fn attach(s: &Store, job: &Job, endpoint: &str) -> Result<Option<Value>> {
    let mut q=s.db.prepare("SELECT w.job,w.request,w.inherited,w.plan,j.note,(SELECT goal FROM followups f WHERE f.parent=j.id AND f.status='cancelled') FROM work_handoffs w JOIN jobs j ON j.id=w.job JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE j.context=?1 AND w.endpoint=?2 AND (j.status='superseded' OR (j.status IN ('speak','pass') AND EXISTS(SELECT 1 FROM followups f WHERE f.parent=j.id AND f.status='cancelled' AND (f.deadline IS NULL OR f.deadline>0)))) AND j.updated>?3 AND (w.child IS NULL OR w.child=?4 OR w.child IN (SELECT id FROM jobs WHERE status IN ('failed','interrupted'))) AND NOT json_extract(m.body,'$.deleted') AND m.revision=json_extract(j.trigger,'$.revision') AND (EXISTS(SELECT 1 FROM observed_tools t WHERE t.job=w.job) OR w.plan!='[]' OR w.inherited!='[]' OR EXISTS(SELECT 1 FROM followups f WHERE f.parent=j.id AND f.status='cancelled')) ORDER BY j.updated DESC LIMIT 4")?;
    let rows = q
        .query_map(
            params![job.context, endpoint, now_ms() - 86400000, job.id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut handoffs = Vec::new();
    for (parent, request, inherited, plan, reason, pending_check) in rows {
        let mut q =
            s.db.prepare("SELECT body FROM observed_tools WHERE job=?1 ORDER BY rowid")?;
        let tools = q
            .query_map([&parent], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str::<Value>(&r?)?))
            .collect::<Result<Vec<_>>>()?;
        let inherited: Value = serde_json::from_str(&inherited)?;
        // Flatten the original requests across repeated interruptions; never
        // grow nested prompt snapshots or import another conversation's state.
        let requests = inherited["earlier_requests"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for request in requests.into_iter().take(4) {
            let valid:bool=s.db.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE j.id=?1 AND j.context=?2 AND NOT json_extract(m.body,'$.deleted') AND m.revision=json_extract(j.trigger,'$.revision'))",params![request["predecessor_job"].as_str().unwrap_or(""),job.context],|r|r.get(0))?;
            if valid && !handoffs.contains(&request) {
                handoffs.push(request);
            }
        }
        handoffs.push(json!({"predecessor_job":parent,"request":serde_json::from_str::<Value>(&request)?,"plan":serde_json::from_str::<Value>(&plan)?,"observed_tools":tools,"withheld_reason":reason,"pending_check":pending_check}));
    }
    handoffs.truncate(6);
    if handoffs.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        json!({"earlier_requests":handoffs,"instruction":"Re-evaluate these prior requests against the newest message and current permissions. A question about status or capability does not by itself cancel unfinished work. A pending_check is a prior commitment whose scheduled wake was superseded, not proof of completion or new authority. Inspect its existing result and report a failure as well as success. Answer the new question and continue still-authorized steps in this turn, or request a bounded follow-up. Honor stop/change instructions, source edits, scope and policy changes. Do not rerun completed effects; inspect their receipts/results in this session or verify existing artifacts. Tool lifecycle observations are not independent proof of external success. Explain any real blocker accurately; a cancelled old turn does not establish a GitHub authorization failure."}),
    ))
}
