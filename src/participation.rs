//! Optional pre-ACP conversation gate. Social judgment lives in an independently
//! configured helper (for example Runtime participate), not in Slack name rules.
use crate::{
    config::{self, Config},
    model::Decision,
    process,
    store::Store,
};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub secret_env: BTreeMap<String, String>,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub role: String,
}
fn timeout() -> u64 {
    8
}
impl Service {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.command.is_empty()
                && self.command.len() <= 4096
                && self.args.len() <= 64
                && (1..=30).contains(&self.timeout_seconds)
                && self.role.len() <= 2000,
            "Invalid conversation decision service"
        );
        ensure!(
            self.args
                .iter()
                .all(|a| a.len() <= 4096 && !a.contains('\0') && !config::looks_secret(a)),
            "Invalid decision arguments; use secret references for credentials"
        );
        ensure!(
            self.secret_env.len() <= 32
                && self.secret_env.iter().all(|(key, name)| !key.is_empty()
                    && key.len() <= 200
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && config::valid_secret_name(name)),
            "Invalid decision secret references"
        );
        Ok(())
    }
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
    let mut people = BTreeMap::new();
    for p in input["participants_from_slack"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(id) = p["id"].as_str() {
            people.insert(id.to_owned(),json!({"id":id,"name":p["name"].as_str().unwrap_or(id),"is_agent":p["is_bot"]==true}));
        }
    }
    for m in &messages {
        let id = m["sender"].as_str().context("Missing message sender")?;
        people
            .entry(id.into())
            .or_insert(json!({"id":id,"name":id,"is_agent":m["bot"]==true}));
    }
    let me = json!({"id":c.slack.bot_user_id,"name":c.label,"is_agent":true,"role":c.attention.decision_service.as_ref().map(|s|s.role.as_str()).unwrap_or("")});
    people.insert(c.slack.bot_user_id.clone(), me.clone());
    let messages:Vec<_>=messages.iter().map(|m|{
        let text=m["text"].as_str().unwrap_or("");
        let recipients:Vec<_>=people.keys().filter(|id|text.contains(&format!("<@{id}>"))).collect();
        json!({"id":m["ts"],"sender":m["sender"],"text":text.chars().take(700).collect::<String>(),"recipients":recipients,
            "reply_to":m["thread"].as_str().filter(|s|!s.is_empty()&&Some(*s)!=m["ts"].as_str())})
    }).collect();
    Ok(
        json!({"conversation_id":input["conversation"]["context"],"agent":me,"participants":people.into_values().collect::<Vec<_>>(),"messages":messages,"trigger_id":trigger["ts"],"responsibilities":""}),
    )
}

pub fn applies(c: &Config, input: &Value) -> bool {
    c.attention.decision_service.is_some()
        && c.participation == "ambient"
        && input.get("accepted_work_continuation").is_none()
        && input["orientation"]["conversation_kind"] != "direct"
        && !input["conversation"]["channel"]
            .as_str()
            .is_some_and(|s| s.starts_with('D'))
}

async fn evaluate(service: &Service, home: &Path, context: &Value) -> Result<Value> {
    service.validate()?;
    let mut command = Command::new(config::local_path(home, &service.command));
    process::prepare(&mut command);
    command
        .args(
            service
                .args
                .iter()
                .map(|s| s.replace("${ADAPTER_HOME}", &home.to_string_lossy())),
        )
        .env_clear()
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    for (key, name) in &service.secret_env {
        let path = home.join("secrets").join(name);
        config::check_private(&path)?;
        command.env(key, std::fs::read_to_string(path)?.trim_end());
    }
    let mut child = command
        .spawn()
        .context("Could not launch conversation decision service")?;
    let _tree = process::ProcessTree::attach(&child)?;
    let mut stdin = child.stdin.take().context("Missing decision stdin")?;
    let stdout = child.stdout.take().context("Missing decision stdout")?;
    tokio::time::timeout(Duration::from_secs(service.timeout_seconds), async {
        let bytes = serde_json::to_vec(context)?;
        ensure!(bytes.len() <= 32768, "Conversation evidence too large");
        stdin.write_all(&bytes).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut bytes = Vec::new();
        stdout.take(65537).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= 65536,
            "Conversation decision response too large"
        );
        ensure!(
            child.wait().await?.success(),
            "Conversation decision service failed"
        );
        let result: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            result["policy"] == "conversation-participation-v1"
                && result["conversation_id"] == context["conversation_id"]
                && result["trigger_id"] == context["trigger_id"]
                && matches!(result["action"].as_str(), Some("engage" | "hold")),
            "Invalid conversation decision response"
        );
        Ok::<_, anyhow::Error>(result)
    })
    .await
    .context("Conversation decision service timed out")?
}

pub async fn gate(
    c: &Config,
    home: &Path,
    input: &Value,
    store: &Arc<Mutex<Store>>,
) -> Result<Option<Decision>> {
    if !applies(c, input) {
        return Ok(None);
    }
    let evidence = context(c, input)?;
    let job = input["job_id"].clone();
    store.lock().unwrap().audit(
        "participation_decision_attempt",
        json!({"job":job,"context":evidence["conversation_id"],"trigger":evidence["trigger_id"]}),
    )?;
    let result = evaluate(
        c.attention.decision_service.as_ref().unwrap(),
        home,
        &evidence,
    )
    .await?;
    let fresh_history = store
        .lock()
        .unwrap()
        .history(&serde_json::from_value(input["trigger"].clone())?)?;
    let mut fresh_input = input.clone();
    fresh_input["history"] = serde_json::to_value(fresh_history)?;
    let current = context(c, &fresh_input)? == evidence;
    let admitted = result["action"] == "engage" && current;
    store.lock().unwrap().audit(
        "participation_gate",
        json!({"job":job,"admitted":admitted,"context_current":current,"result":result}),
    )?;
    Ok(if admitted {
        None
    } else {
        Some(Decision {
            decision: "pass".into(),
            text: String::new(),
            reason: "Conversation decision service: no reasoning turn needed".into(),
            media: vec![],
        })
    })
}
