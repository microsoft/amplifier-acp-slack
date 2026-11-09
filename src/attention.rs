//! Slack facts for an optional ACP host admission capability. No model or social policy here.
use crate::config::Config;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub const META: &str = "com.microsoft.amplifier.attention";
pub const METHOD: &str = "_amplifier/attention";
pub fn supported(init: &Value) -> bool {
    init["_meta"][META]["version"] == 1
        && init["_meta"][META]["enabled"] == true
        && init["_meta"][META]["method"] == METHOD
}
/// Only the negotiated host attestation for this exact request establishes
/// non-forwarding. Generic error codes and provider prose establish nothing.
pub fn not_forwarded(init: &Value, error: &Value, offer: Option<&Value>, session: &str) -> bool {
    let Some(id) = offer
        .and_then(|v| v["id"].as_str())
        .filter(|id| !id.is_empty())
    else {
        return false;
    };
    let evidence = &error["data"][META];
    supported(init)
        && init["_meta"][META]["noForwardRejectionVersion"] == 1
        && evidence["version"] == 1
        && evidence["id"] == id
        && evidence["sessionId"] == session
        && evidence["outcome"] == "not_forwarded"
}
pub fn applies(c: &Config, input: &Value) -> bool {
    c.participation == "ambient"
        && input.get("accepted_work_continuation").is_none()
        && input["orientation"]["conversation_kind"] != "direct"
        && !input["conversation"]["channel"]
            .as_str()
            .is_some_and(|s| s.starts_with('D'))
}
pub fn occurred_at(input: &Value) -> Result<i64> {
    let ts = input["trigger"]["ts"]
        .as_str()
        .context("Missing Slack timestamp")?
        .parse::<f64>()?;
    anyhow::ensure!(ts.is_finite() && ts > 0., "Invalid Slack timestamp");
    Ok((ts * 1000.) as i64)
}
pub fn context(c: &Config, input: &Value) -> Result<Value> {
    let trigger = &input["trigger"];
    let mut messages: Vec<Value> = input["history"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["deleted"] != true)
        .cloned()
        .collect();
    if !messages.iter().any(|m| m["ts"] == trigger["ts"]) {
        messages.push(trigger.clone());
    }
    messages.sort_by(|a, b| a["ts"].as_str().cmp(&b["ts"].as_str()));
    if messages.len() > 24 {
        messages.drain(..messages.len() - 24);
    }
    let source_people: BTreeMap<_, _> = input["participants_from_slack"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["id"].as_str().map(|id| (id, p)))
        .collect();
    let mut people = BTreeMap::new();
    for m in &messages {
        let id = m["sender"].as_str().context("Missing message sender")?;
        let p = source_people.get(id);
        people.entry(id.to_owned()).or_insert(json!({"id":id,"name":p.and_then(|p|p["name"].as_str()).unwrap_or(id).chars().take(100).collect::<String>(),"is_agent":m["bot"]==true || p.is_some_and(|p|p["is_bot"]==true)}));
    }
    let me = json!({"id":c.slack.bot_user_id,"name":c.label,"is_agent":true,"role":""});
    people.insert(c.slack.bot_user_id.clone(), me.clone());
    // Membership can be large; include only speakers and referenced addressees.
    for (id, p) in source_people {
        if people.len() >= 64 {
            break;
        }
        if messages.iter().any(|m| {
            m["text"]
                .as_str()
                .unwrap_or("")
                .contains(&format!("<@{id}>"))
        }) {
            people.entry(id.to_owned()).or_insert(json!({"id":id,"name":p["name"].as_str().unwrap_or(id).chars().take(100).collect::<String>(),"is_agent":p["is_bot"]==true}));
        }
    }
    let messages:Vec<_>=messages.iter().map(|m|{
        let text=m["text"].as_str().unwrap_or("");
        let recipients:Vec<_>=people.keys().filter(|id|text.contains(&format!("<@{id}>"))).collect();
        json!({"id":m["ts"],"sender":m["sender"],"text":text.chars().take(700).collect::<String>(),"recipients":recipients,
            "reply_to":m["thread"].as_str().filter(|s|!s.is_empty()&&Some(*s)!=m["ts"].as_str())})
    }).collect();
    Ok(
        json!({"conversation_id":input["conversation"]["channel"],"thread_id":input["conversation"]["thread"].as_str().unwrap_or(""),"agent":me,"participants":people.into_values().collect::<Vec<_>>(),"messages":messages,"trigger_id":trigger["ts"],"responsibilities":""}),
    )
}

pub fn source_delivery_supported(init: &Value) -> bool {
    init["_meta"]["io.github.bkrabach.agent-instance"]["inputContinuity"]["sourceDelivery"]["version"]
        == 1
}
pub fn context_for(c: &Config, input: &Value, init: &Value) -> Result<Value> {
    let mut ctx = context(c, input)?;
    if source_delivery_supported(init) {
        for m in ctx["messages"].as_array_mut().unwrap() {
            let source = input["history"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(std::iter::once(&input["trigger"]))
                .find(|s| s["ts"] == m["id"]);
            if let Some(source) = source
                && source["revision"].is_string()
                && let Some(ts) = source["ts"]
                    .as_str()
                    .and_then(|t| t.parse::<f64>().ok())
                    .filter(|n| n.is_finite() && *n > 0.)
            {
                m["revision"] = source["revision"].clone();
                m["occurred_at"] = json!((ts * 1000.) as i64);
            }
        }
    }
    Ok(ctx)
}
