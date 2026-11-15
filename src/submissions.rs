//! Read-only source lookup over retained delivery records. A response is not work completion.
use crate::{
    config::Config,
    model::{Message, now_ms},
    store::Store,
};
use anyhow::{Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub const META: &str = "com.microsoft.amplifier.source-delivery";
pub const METHOD: &str = "_amplifier/source_receipts";
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub team: String,
    pub channel: String,
    pub thread: String,
    pub ts: String,
    pub revision: String,
    pub sender: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inspect {
    pub sources: Vec<Source>,
}
impl Source {
    fn valid(&self) -> bool {
        [
            &self.team,
            &self.channel,
            &self.ts,
            &self.revision,
            &self.sender,
        ]
        .iter()
        .all(|s| !s.is_empty() && s.len() <= 512)
            && self.thread.len() <= 512
    }
}
pub fn inspect(store: &Store, c: &Config, request: &Inspect) -> Result<Value> {
    ensure!(
        !request.sources.is_empty()
            && request.sources.len() <= 16
            && request.sources.iter().all(Source::valid),
        "Expected 1..16 exact source references"
    );
    let mut items = vec![];
    for s in &request.sources {
        let mut attempts = vec![];
        if s.team == c.slack.team_id && c.allows(&s.channel, "") {
            let mut q=store.db.prepare("SELECT j.id,j.status,j.updated,j.source_delivery,o.status,o.slack_ts,o.id FROM jobs j LEFT JOIN outbox o ON o.job_id=j.id WHERE j.context=?1 AND json_extract(j.trigger,'$.team')=?2 AND json_extract(j.trigger,'$.channel')=?3 AND json_extract(j.trigger,'$.thread')=?4 AND json_extract(j.trigger,'$.ts')=?5 AND json_extract(j.trigger,'$.revision')=?6 AND json_extract(j.trigger,'$.sender')=?7 ORDER BY j.created,j.rowid LIMIT 65")?;
            let rows = q.query_map(
                params![
                    crate::model::context_id(&s.team, &s.channel, &s.thread),
                    s.team,
                    s.channel,
                    s.thread,
                    s.ts,
                    s.revision,
                    s.sender
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )?;
            for row in rows {
                let (id, status, updated, instance, outgoing, message, outbox) = row?;
                let mut parts=store.db.prepare("SELECT part,status,slack_ts FROM outbox_parts WHERE outbox_id=?1 ORDER BY part")?;
                let parts=parts.query_map([outbox],|r|Ok(json!({"part":r.get::<_,i64>(0)?+1,"state":r.get::<_,String>(1)?,"message_id":r.get::<_,Option<String>>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
                attempts.push(json!({"job_id":id,"processing":{"state":status,"observed_at":updated},"publication":{"state":outgoing.unwrap_or_else(||"unknown".into()),"message_id":message,"parts":parts},"instance":instance.and_then(|v|serde_json::from_str::<Value>(&v).ok()).unwrap_or(json!({"state":"unknown","work":[],"coverage":"not_observed"}))}));
            }
        }
        let partial = attempts.len() > 64;
        attempts.truncate(64);
        items.push(json!({"source":s,"state":if attempts.is_empty(){"unknown"}else{"retained"},"attempts":attempts,"coverage":if partial{"partial"}else{"retained_exact_source_records"},"completion":"not_established"}));
    }
    Ok(
        json!({"version":1,"items":items,"observed_at":now_ms(),"missing":"unknown","inspection":"local_records_only"}),
    )
}
pub fn profile(init: &Value) -> bool {
    let p =
        &init["_meta"]["io.github.bkrabach.agent-instance"]["inputContinuity"]["sourceDelivery"];
    p["version"] == 1
        && p["metadata"] == META
        && p["ordinaryPrompts"] == true
        && p["promptRetry"] == "inspectOnly"
        && p["receiptsMethod"] == METHOD
        && p["workReferences"] == true
}
/// Returns metadata only for actual response turns. Quiet participation checks and
/// scheduled continuations are not new deliveries of the original human message.
pub fn prepare(
    store: &Store,
    c: &Config,
    init: &Value,
    input: &Value,
    sid: &str,
    endpoint: &str,
) -> Result<Option<Value>> {
    let Some(job) = input["job_id"].as_str().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if input.get("accepted_work_continuation").is_some() {
        return Ok(None);
    }
    if !profile(init) {
        store.db.execute(
            "UPDATE jobs SET source_delivery=?1 WHERE id=?2",
            params![
                json!({"state":"unsupported","work":[],"coverage":"not_advertised"}).to_string(),
                job
            ],
        )?;
        return Ok(None);
    }
    let m: Message = serde_json::from_value(input["trigger"].clone())?;
    let occurred = crate::attention::occurred_at(input)?;
    let metadata = json!({"version":1,"inputId":job,"sources":[{"id":m.ts,"revision":m.revision,"occurredAt":occurred}]});
    store.db.execute("UPDATE jobs SET source_delivery=?1 WHERE id=?2",params![json!({"state":"unknown","session_id":sid,"endpoint":endpoint,"input_id":job,"audience_binding":if c.context.enabled {"configured_grant"}else{"unavailable"},"receipt_query":{"method":METHOD,"params":{"sessionId":sid,"sources":metadata["sources"]}},"work":[],"coverage":"not_observed","observed_at":now_ms()}).to_string(),job])?;
    Ok(Some(metadata))
}
pub fn record(
    store: &Store,
    c: &Config,
    job: &str,
    metadata: &Value,
    sid: &str,
    endpoint: &str,
    result: Option<&Value>,
) -> Result<()> {
    let source = &metadata["sources"][0];
    let receipt = result
        .filter(|r| r["version"] == 1)
        .and_then(|r| r["receipts"].as_array())
        .and_then(|rs| {
            rs.iter().find(|r| {
                r["sourceId"] == source["id"]
                    && r["revision"] == source["revision"]
                    && r["occurredAt"] == source["occurredAt"]
            })
        });
    let value = if let Some(r) = receipt {
        json!({"state":"observed","session_id":sid,"endpoint":endpoint,"input_id":job,"audience_binding":if c.context.enabled {"configured_grant"}else{"unavailable"},"receipt_query":{"method":METHOD,"params":{"sessionId":sid,"sources":metadata["sources"]}},"receipt":r,"work":r["work"],"coverage":result.unwrap()["coverage"],"observed_at":now_ms(),"current_state":"not_refreshed"})
    } else {
        json!({"state":"unknown","session_id":sid,"endpoint":endpoint,"input_id":job,"audience_binding":if c.context.enabled {"configured_grant"}else{"unavailable"},"receipt_query":{"method":METHOD,"params":{"sessionId":sid,"sources":metadata["sources"]}},"work":[],"coverage":"missing_or_unavailable","observed_at":now_ms()})
    };
    store.db.execute(
        "UPDATE jobs SET source_delivery=?1 WHERE id=?2",
        params![value.to_string(), job],
    )?;
    Ok(())
}
