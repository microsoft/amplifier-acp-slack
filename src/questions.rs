//! Requester-private Slack dialogs for ordinary ACP elicitation.
//! Pending data lives only in memory and dies with its initiating turn.
use crate::{config::Config, elicitation::Elicitations, slack::Slack, store::Store};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};
#[derive(Clone)]
struct Binding {
    home: PathBuf,
    job: String,
    actor: String,
    channel: String,
    ts: String,
    revision: u64,
    view: Option<String>,
}
#[derive(Default)]
struct Questions {
    hub: Elicitations,
    bindings: Mutex<HashMap<String, Binding>>,
}
fn registry() -> &'static Questions {
    static Q: OnceLock<Questions> = OnceLock::new();
    Q.get_or_init(Questions::default)
}
struct Guard(String);
impl Drop for Guard {
    fn drop(&mut self) {
        registry().bindings.lock().unwrap().remove(&self.0);
    }
}
fn live(c: &Config, b: &Binding) -> Result<bool> {
    if !c.enabled || c.revision != b.revision || !c.acp.client.approval_users.contains(&b.actor) {
        return Ok(false);
    }
    Ok(Store::open(&b.home)?.db.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN contexts x ON x.id=j.context JOIN messages m ON m.channel=json_extract(j.trigger,'$.channel') AND m.ts=json_extract(j.trigger,'$.ts') WHERE j.id=?1 AND j.status='running' AND j.version=x.version AND m.revision=json_extract(j.trigger,'$.revision') AND NOT json_extract(m.body,'$.deleted'))",[&b.job],|r|r.get(0))?)
}
fn current(home: &Path, c: &Config, id: &str) -> Result<(Binding, Value)> {
    let b = registry()
        .bindings
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .context("Question has closed")?;
    ensure!(
        b.home == home && live(c, &b)?,
        "Question is no longer active"
    );
    let q = registry()
        .hub
        .list(&b.job, &b.actor)
        .into_iter()
        .find(|q| q["id"] == id)
        .context("Question expired")?;
    Ok((b, q))
}
pub async fn request(c: &Config, home: &Path, input: &Value, params: &Value) -> Result<Value> {
    let actor = input["trigger"]["sender"].as_str().unwrap_or("");
    if input["trigger"]["bot"] != false || !c.acp.client.approval_users.iter().any(|u| u == actor) {
        return Ok(crate::elicitation::cancelled());
    }
    let job = input["job_id"]
        .as_str()
        .context("Question needs a live job")?;
    let channel = input["conversation"]["channel"]
        .as_str()
        .context("Question needs a channel")?;
    let mut b = Binding {
        home: home.to_owned(),
        job: job.into(),
        actor: actor.into(),
        channel: channel.into(),
        ts: String::new(),
        revision: c.revision,
        view: None,
    };
    ensure!(live(c, &b)?, "Question needs an active initiating turn");
    let pending = registry()
        .hub
        .request(
            job,
            actor,
            "Agent",
            params,
            Duration::from_secs(c.acp.client.permission_timeout_seconds),
        )
        .map_err(anyhow::Error::msg)?;
    let id = pending.id.clone();
    let q = registry()
        .hub
        .list(job, actor)
        .into_iter()
        .find(|q| q["id"] == id)
        .unwrap();
    modal(&q)?; // Refuse schemas Slack cannot faithfully render before posting controls.
    registry()
        .bindings
        .lock()
        .unwrap()
        .insert(id.clone(), b.clone());
    let _guard = Guard(id.clone());
    let slack = Slack::new(c, c.secrets(home)?)?;
    let mut post = json!({"channel":channel,"text":"The agent needs a private response from the person who started this turn.","client_msg_id":id,"blocks":[{"type":"section","text":{"type":"plain_text","text":"The agent needs your input. Only the person who started this turn can open this question."},"accessory":{"type":"button","action_id":"acp_question_open","text":{"type":"plain_text","text":"Respond privately"},"value":id}}],"metadata":{"event_type":"acp_question","event_payload":{"id":id}},"unfurl_links":false,"unfurl_media":false});
    if let Some(thread) = input["conversation"]["thread"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        post["thread_ts"] = json!(thread)
    }
    let response = slack
        .call("chat.postMessage", post, false)
        .await
        .map_err(|_| anyhow::anyhow!("Could not deliver agent question"))?;
    b.ts = response["ts"]
        .as_str()
        .context("Question delivery was not confirmed")?
        .into();
    registry()
        .bindings
        .lock()
        .unwrap()
        .insert(id.clone(), b.clone());
    let wait = pending.wait();
    tokio::pin!(wait);
    let result = loop {
        tokio::select! {
            answer=&mut wait=>break answer,
            _=tokio::time::sleep(Duration::from_millis(200))=>{
                if !live(&Config::load(home)?,&b)? {break crate::elicitation::cancelled()}
            }
        }
    };
    let _=slack.call("chat.update",json!({"channel":channel,"ts":b.ts,"text":"Agent question closed.","blocks":[{"type":"section","text":{"type":"plain_text","text":"Agent question closed."}}]}),false).await;
    Store::open(home)?.audit(
        "question_closed",
        json!({"id":id,"action":result["action"]}),
    )?;
    Ok(result)
}
fn plain(s: &str) -> Value {
    json!({"type":"plain_text","text":s})
}
fn options(s: &Value) -> Option<Vec<Value>> {
    if let Some(a) = s["enum"].as_array() {
        return Some(
            a.iter()
                .map(|v| json!({"value":v,"text":plain(v.as_str().unwrap_or(""))}))
                .collect(),
        );
    }
    s["oneOf"].as_array().or(s["anyOf"].as_array()).map(|a|a.iter().map(|v|json!({"value":v["const"],"text":plain(v["title"].as_str().or(v["const"].as_str()).unwrap_or(""))})).collect())
}
pub fn modal(q: &Value) -> Result<Value> {
    let message = q["message"].as_str().unwrap_or("Agent question");
    ensure!(
        message.chars().count() <= 3000,
        "Question is too long for a Slack dialog"
    );
    let mut blocks = vec![json!({"type":"section","text":plain(message)})];
    if q["mode"] == "url" {
        let url = q["url"].as_str().context("Missing authorization URL")?;
        ensure!(url.len() <= 2600, "Authorization URL is too long for Slack");
        blocks.push(json!({"type":"section","text":plain(&format!("Open this address only if you trust its destination. Opening does not mean sign-in is finished.\n{url}"))}));
        blocks.push(json!({"type":"actions","elements":[{"type":"button","action_id":"acp_question_url","text":plain("Open provider page"),"value":q["id"],"url":url}]}));
    } else {
        let properties = q["schema"]["properties"]
            .as_object()
            .context("Missing fields")?;
        ensure!(properties.len() <= 30, "Too many fields for a Slack dialog");
        for (i, (key, s)) in properties.iter().enumerate() {
            let label = s["title"].as_str().unwrap_or(key);
            ensure!(
                label.chars().count() <= 2000,
                "Field label is too long for Slack"
            );
            let choices = options(if s["type"] == "array" { &s["items"] } else { s });
            let mut element = if let Some(options) = choices {
                ensure!(
                    options.len() <= 100
                        && options.iter().all(|o| o["value"]
                            .as_str()
                            .is_some_and(|s| s.len() <= 150)
                            && o["text"]["text"]
                                .as_str()
                                .is_some_and(|s| s.chars().count() <= 75)),
                    "Choices exceed Slack limits"
                );
                json!({"type":if s["type"]=="array" {"multi_static_select"}else{"static_select"},"options":options})
            } else if s["type"] == "boolean" {
                json!({"type":"static_select","options":[{"text":plain("Yes"),"value":"true"},{"text":plain("No"),"value":"false"}]})
            } else {
                json!({"type":"plain_text_input","max_length":s["maxLength"].as_u64().unwrap_or(3000).min(3000)})
            };
            element["action_id"] = json!("value");
            let required = q["schema"]["required"]
                .as_array()
                .is_some_and(|a| a.contains(&json!(key)));
            blocks.push(json!({"type":"input","block_id":format!("field-{i}"),"label":plain(label),"optional":!required,"element":element}));
        }
    }
    blocks.push(json!({"type":"actions","elements":[{"type":"button","action_id":"acp_question_decline","text":plain("Decline"),"value":q["id"]}]}));
    let mut view = json!({"type":"modal","callback_id":"acp_question","private_metadata":q["id"],"notify_on_close":true,"title":plain("Agent question"),"close":plain("Cancel"),"blocks":blocks});
    if q["mode"] != "url" {
        view["submit"] = plain("Send answer")
    }
    Ok(view)
}
fn authorized(c: &Config, b: &Binding, p: &Value, opening: bool) -> bool {
    p["team"]["id"] == c.slack.team_id
        && p["api_app_id"] == c.slack.app_id
        && p["user"]["id"] == b.actor
        && if opening {
            p["container"]["channel_id"] == b.channel
                && p["container"]["message_ts"] == b.ts
                && !b.ts.is_empty()
        } else {
            b.view.as_ref().is_some_and(|v| p["view"]["id"] == *v)
        }
}
pub async fn open(home: &Path, c: &Config, slack: &Slack, p: &Value) -> Result<()> {
    let id = p["actions"][0]["value"]
        .as_str()
        .context("Missing question")?;
    let (b, q) = current(home, c, id)?;
    ensure!(
        authorized(c, &b, p, true),
        "Question belongs to another person or room"
    );
    let v = slack
        .call(
            "views.open",
            json!({"trigger_id":p["trigger_id"],"view":modal(&q)?}),
            false,
        )
        .await
        .map_err(|_| anyhow::anyhow!("Could not open agent question"))?;
    let view = v["view"]["id"]
        .as_str()
        .context("Slack did not confirm question dialog")?;
    if let Some(b) = registry().bindings.lock().unwrap().get_mut(id) {
        b.view = Some(view.into())
    }
    Ok(())
}
pub fn submit(home: &Path, c: &Config, p: &Value) -> Result<Value> {
    let id = p["view"]["private_metadata"]
        .as_str()
        .context("Missing question")?;
    let (b, q) = current(home, c, id)?;
    ensure!(
        authorized(c, &b, p, false),
        "Question belongs to another person or dialog"
    );
    let kind = p["type"].as_str().unwrap_or("");
    let action = if kind == "view_closed" {
        "cancel"
    } else if kind == "view_submission" && q["mode"] != "url" {
        "accept"
    } else {
        match p["actions"][0]["action_id"].as_str() {
            Some("acp_question_decline") => "decline",
            Some("acp_question_url") if q["mode"] == "url" => "accept",
            _ => anyhow::bail!("Unknown question action"),
        }
    };
    let content = if action == "accept" && q["mode"] != "url" {
        Some(answers(&q, &p["view"]["state"]["values"])?)
    } else {
        None
    };
    if let Err(error) = registry()
        .hub
        .resolve(&b.job, &b.actor, id, action, content.as_ref())
    {
        if kind == "view_submission" {
            return Ok(json!({"response_action":"errors","errors":{"field-0":error}}));
        }
        anyhow::bail!("Question response rejected")
    }
    Ok(json!({}))
}
pub fn answers(q: &Value, values: &Value) -> Result<Value> {
    let mut content = json!({});
    for (i, (key, s)) in q["schema"]["properties"]
        .as_object()
        .context("Missing fields")?
        .iter()
        .enumerate()
    {
        let f = &values[format!("field-{i}")]["value"];
        let v = if s["type"] == "array" {
            f["selected_options"]
                .as_array()
                .map(|a| json!(a.iter().map(|v| v["value"].clone()).collect::<Vec<_>>()))
        } else if s["type"] == "boolean" {
            match f["selected_option"]["value"].as_str() {
                Some("true") => Some(json!(true)),
                Some("false") => Some(json!(false)),
                _ => None,
            }
        } else if options(s).is_some() {
            f["selected_option"]
                .get("value")
                .filter(|v| !v.is_null())
                .cloned()
        } else {
            f["value"].as_str().filter(|v| !v.is_empty()).map(|v| {
                if s["type"] == "number" || s["type"] == "integer" {
                    serde_json::from_str::<Value>(v).unwrap_or_else(|_| json!(v))
                } else {
                    json!(v)
                }
            })
        };
        if let Some(v) = v {
            content[key] = v
        }
    }
    Ok(content)
}
/// Separate administrator authority; automation never impersonates a Slack user.
pub fn list(home: &Path, c: &Config) -> Result<Vec<Value>> {
    let ids: Vec<String> = registry()
        .bindings
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, b)| b.home == home)
        .map(|(id, _)| id.clone())
        .collect();
    Ok(ids
        .iter()
        .filter_map(|id| {
            current(home, c, id).ok().map(|(b, mut q)| {
                q["approver"] = json!(b.actor);
                q
            })
        })
        .collect())
}
pub fn decide(
    home: &Path,
    c: &Config,
    id: &str,
    action: &str,
    content: Option<&Value>,
    actor: &str,
) -> Result<()> {
    let (b, _) = current(home, c, id)?;
    registry()
        .hub
        .resolve(&b.job, &b.actor, id, action, content)
        .map_err(anyhow::Error::msg)?;
    Store::open(home)?.audit(
        "question_operator_decision",
        json!({"id":id,"actor":actor,"action":action}),
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_dialog_supports_typed_inputs_and_full_url() {
        let q = json!({"id":"q","mode":"form","message":"Choose","schema":{"type":"object","properties":{"a":{"type":"boolean"},"b":{"type":"number"},"c":{"type":"string","enum":["one","two"]}}}});
        let m = modal(&q).unwrap();
        assert_eq!(m["notify_on_close"], true);
        assert_eq!(m["blocks"][1]["element"]["type"], "static_select");
        let a=answers(&q,&json!({"field-0":{"value":{"selected_option":{"value":"false"}}},"field-1":{"value":{"value":"2.5"}},"field-2":{"value":{"selected_option":{"value":"one"}}}})).unwrap();
        assert_eq!(a, json!({"a":false,"b":2.5,"c":"one"}));
        let u=modal(&json!({"id":"q","mode":"url","message":"Login","url":"https://example.test/login?state=secret"})).unwrap();
        assert!(u["submit"].is_null());
        assert_eq!(
            u["blocks"][2]["elements"][0]["url"],
            "https://example.test/login?state=secret"
        );
    }
    #[test]
    fn controls_are_bound_to_person_workspace_and_view() {
        let c = Config::new("fixture".into(), "owner".into());
        let b = Binding {
            home: PathBuf::new(),
            job: "j".into(),
            actor: "U1".into(),
            channel: "C1".into(),
            ts: "1".into(),
            revision: 1,
            view: Some("V1".into()),
        };
        let mut p = json!({"team":{"id":c.slack.team_id},"api_app_id":c.slack.app_id,"user":{"id":"U1"},"view":{"id":"V1"},"container":{"channel_id":"C1","message_ts":"1"}});
        assert!(authorized(&c, &b, &p, true));
        assert!(authorized(&c, &b, &p, false));
        p["user"]["id"] = json!("U2");
        assert!(!authorized(&c, &b, &p, false));
        p["user"]["id"] = json!("U1");
        p["view"]["id"] = json!("V2");
        assert!(!authorized(&c, &b, &p, false));
    }
}
