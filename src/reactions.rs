//! Slack reactions are audience-scoped observations, never permission decisions
//! or new work. Own writes use Slack's normal bot identity and granted scopes.
use crate::{
    config::Config,
    model::{Message, now_ms},
    store::Store,
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
pub fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_+-:".contains(&c))
}
pub fn projection(message: &Value) -> Value {
    json!(message["reactions"].as_array().into_iter().flatten().take(100).filter_map(|r|{
        let n=r["name"].as_str().filter(|n|name(n))?;
        let users:Vec<_>=r["users"].as_array().into_iter().flatten().take(100).filter_map(|u|u.as_str().filter(|s|s.len()<=100 && s.bytes().all(|b|b.is_ascii_alphanumeric()))).collect();
        Some(json!({"name":n,"count":r["count"].as_u64().unwrap_or(0),"users":users,"users_complete":false}))
    }).collect::<Vec<_>>())
}
pub fn receive(c: &Config, s: &mut Store, p: &Value) -> Result<()> {
    let e = &p["event"];
    let (Some(channel), Some(ts), Some(actor), Some(emoji), Some(event_id)) = (
        e["item"]["channel"].as_str(),
        e["item"]["ts"].as_str(),
        e["user"].as_str(),
        e["reaction"].as_str(),
        p["event_id"].as_str(),
    ) else {
        return Ok(());
    };
    if e["item"]["type"] != "message"
        || !name(emoji)
        || !c.allows(channel, "")
        || event_id.len() > 200
        || actor == c.slack.bot_user_id
        || actor.is_empty()
        || actor.len() > 100
        || !actor.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Ok(());
    }
    // Restrict active awareness to a message this installation has already
    // observed. Unknown items are available through fresh authorized reads.
    let Some(original) = s.message(channel, ts)? else {
        return Ok(());
    };
    if original.deleted || crate::slack::known_control(s, channel, ts)? {
        return Ok(());
    }
    let context = original.context();
    let active: Option<String> =
        s.db.query_row(
            "SELECT id FROM jobs WHERE context=?1 AND status='running' LIMIT 1",
            [&context],
            |r| r.get(0),
        )
        .optional()?;
    let Some(active) = active else { return Ok(()) };
    let id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("reaction:{event_id}:{active}").as_bytes(),
    )
    .to_string();
    let (pending,bytes):(i64,i64)=s.db.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM steering_inputs WHERE target_job=?1 AND intent='awareness' AND state IN ('queued','submitted','received','remoteQueued')",[&active],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let verb = if e["type"] == "reaction_removed" {
        "removed"
    } else {
        "added"
    };
    let message = Message {
        text: format!(
            "Slack reaction observation: {actor} {verb} :{emoji}: on message {ts}. This is awareness, not a new request or approval. Inspect the current reactions if their state matters."
        ),
        sender: actor.into(),
        bot: true,
        files: vec![],
        ..original
    };
    let body = serde_json::to_string(&message)?;
    let overflow = pending >= crate::steering::MAX_PENDING_AWARENESS
        || bytes + body.len() as i64 > crate::steering::MAX_PENDING_AWARENESS_BYTES;
    let n=s.db.execute("INSERT OR IGNORE INTO steering_inputs(input_id,context,source_id,revision,state,created,target_job,intent,body,reason) VALUES(?1,?2,?3,?4,?5,?6,?7,'awareness',?8,'reaction_observation')",params![id,context,event_id,message.revision,if overflow{"historyOnly"}else{"queued"},now_ms(),active,body])?;
    if n > 0 {
        s.audit("reaction_observed",json!({"input_id":id,"context":context,"active_job":active,"state":if overflow{"historyOnly"}else{"queued"},"kind":verb}))?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_is_bounded_and_does_not_claim_a_complete_actor_list() {
        let v = projection(
            &json!({"reactions":[{"name":"thumbsup","count":5,"users":["U1"],"token":"secret"},{"name":"<evil>","count":1}]}),
        );
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["count"], 5);
        assert_eq!(v[0]["users_complete"], false);
        assert!(!v.to_string().contains("secret"));
    }
}
