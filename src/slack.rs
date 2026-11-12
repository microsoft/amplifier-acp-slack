use crate::{
    config::{Config, Secrets},
    model::Message,
    store::{Outgoing, Store},
};
use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as Ws;

#[derive(Clone)]
pub struct Slack {
    client: reqwest::Client,
    base: String,
    secrets: Secrets,
    test: bool,
    bot_user: String,
    bot_id: String,
    people: Arc<Mutex<std::collections::HashMap<String, Value>>>,
    granted_scopes: Arc<Mutex<Option<Vec<String>>>>,
}
#[derive(Debug)]
pub enum ApiError {
    RateLimited(u64),
    Rejected(String),
    Uncertain,
}

impl Slack {
    pub fn new(c: &Config, s: Secrets) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: c.slack.api_base.clone(),
            secrets: s,
            test: c.slack.loopback_test,
            bot_user: c.slack.bot_user_id.clone(),
            bot_id: c.slack.bot_id.clone(),
            people: Arc::new(Mutex::new(std::collections::HashMap::new())),
            granted_scopes: Arc::new(Mutex::new(None)),
        })
    }
    pub async fn call(
        &self,
        method: &str,
        mut body: Value,
        app: bool,
    ) -> std::result::Result<Value, ApiError> {
        if matches!(method, "conversations.history" | "conversations.replies") {
            body["include_all_metadata"] = true.into();
        }
        let token = if app {
            &self.secrets.app_token
        } else {
            &self.secrets.bot_token
        };
        let url = format!("{}{method}", self.base);
        let request = if [
            "users.conversations",
            "conversations.history",
            "conversations.replies",
            "conversations.members",
            "users.info",
            "bots.info",
            "files.info",
            "chat.getPermalink",
        ]
        .contains(&method)
        {
            self.client.get(url).query(&body)
        } else if method == "files.getUploadURLExternal" {
            self.client.post(url).form(&[
                (
                    "filename",
                    body["filename"].as_str().unwrap_or("").to_owned(),
                ),
                ("length", body["length"].to_string()),
            ])
        } else {
            self.client.post(url).json(&body)
        };
        let r = request
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| ApiError::Uncertain)?;
        if r.status().as_u16() == 429 {
            return Err(ApiError::RateLimited(
                r.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60),
            ));
        }
        if !r.status().is_success() {
            return Err(ApiError::Uncertain);
        }
        let scopes = r
            .headers()
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
            .map(parse_scopes);
        let d: Value = r.json().await.map_err(|_| ApiError::Uncertain)?;
        if d["ok"] != true {
            let code = d["error"].as_str().unwrap_or("unknown_error");
            if code == "ratelimited" {
                return Err(ApiError::RateLimited(60));
            }
            if [
                "internal_error",
                "fatal_error",
                "request_timeout",
                "service_unavailable",
            ]
            .contains(&code)
            {
                return Err(ApiError::Uncertain);
            }
            return Err(ApiError::Rejected(
                code.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(80)
                    .collect(),
            ));
        }
        if !app {
            *self.granted_scopes.lock().unwrap() = scopes;
        }
        Ok(d)
    }
    pub fn media_readiness(&self, c: &Config) -> Value {
        media_readiness(c, self.granted_scopes.lock().unwrap().as_deref())
    }
    pub async fn verify(&self, c: &Config) -> Result<()> {
        let d = self
            .call("auth.test", json!({}), false)
            .await
            .map_err(|e| anyhow::anyhow!("Slack authentication: {e:?}"))?;
        ensure!(
            d["team_id"] == c.slack.team_id
                && d["user_id"] == c.slack.bot_user_id
                && d["bot_id"] == c.slack.bot_id,
            "Slack credential identity does not match this installation"
        );
        Ok(())
    }
    /// Resume only confirmed portions of a multipart reply. Each attempted part
    /// has a durable identity before sending; uncertain effects are never replayed.
    pub async fn publish_durable(
        &self,
        o: &Outgoing,
        store: &Arc<Mutex<Store>>,
    ) -> std::result::Result<String, ApiError> {
        let notice = store
            .lock()
            .unwrap()
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox WHERE id=?1 AND connector_notice=1)",
                [&o.id],
                |r| r.get::<_, bool>(0),
            )
            .map_err(|_| ApiError::Uncertain)?;
        let (indices, single) = {
            let mut s = store.lock().unwrap();
            let tx =
                s.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|_| ApiError::Uncertain)?;
            let mut indices: Vec<i64> = tx
                .prepare("SELECT part FROM outbox_parts WHERE outbox_id=?1 ORDER BY part")
                .map_err(|_| ApiError::Uncertain)?
                .query_map([&o.id], |r| r.get(0))
                .map_err(|_| ApiError::Uncertain)?
                .collect::<rusqlite::Result<_>>()
                .map_err(|_| ApiError::Uncertain)?;
            let mut single = None;
            // A saved plan is authoritative across upgrades. Re-splitting can
            // change boundaries and append a second copy of an already sent tail.
            if indices.is_empty() {
                let parts = crate::reply_parts::split(&o.text)
                    .map_err(|_| ApiError::Rejected("reply_cannot_be_split_safely".into()))?;
                if parts.len() == 1 {
                    single = Some(parts.into_iter().next().unwrap().markdown);
                } else {
                    for (index, part) in parts.iter().enumerate() {
                        let client_id = uuid::Uuid::new_v5(
                            &uuid::Uuid::NAMESPACE_URL,
                            format!("acp-reply:{}:{index}", o.id).as_bytes(),
                        )
                        .to_string();
                        tx.execute("INSERT INTO outbox_parts(outbox_id,part,client_id,text) VALUES(?1,?2,?3,?4)",rusqlite::params![o.id,index as i64,client_id,part.markdown]).map_err(|_|ApiError::Uncertain)?;
                        indices.push(index as i64);
                    }
                }
            }
            tx.commit().map_err(|_| ApiError::Uncertain)?;
            (indices, single)
        };
        if let Some(text) = single {
            let mut payload = o.clone();
            payload.text = text;
            return self.publish_message(&payload, notice, None).await;
        }
        let mut first = None;
        for (position, index) in indices.iter().enumerate() {
            let (client_id, text, status, ts): (String, String, String, Option<String>) = {
                let s = store.lock().unwrap();
                s.db.query_row("SELECT client_id,text,status,slack_ts FROM outbox_parts WHERE outbox_id=?1 AND part=?2",rusqlite::params![o.id,index],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_|ApiError::Uncertain)?
            };
            if status == "sent" {
                if first.is_none() {
                    first = ts;
                }
                continue;
            }
            if status != "ready" {
                return Err(ApiError::Uncertain);
            }
            if !store
                .lock()
                .unwrap()
                .outgoing_source_current(&o.id)
                .map_err(|_| ApiError::Uncertain)?
            {
                return Err(ApiError::Rejected("reply_source_changed".into()));
            }
            store
                .lock()
                .unwrap()
                .db
                .execute(
                    "UPDATE outbox_parts SET status='sending' WHERE outbox_id=?1 AND part=?2",
                    rusqlite::params![o.id, index],
                )
                .map_err(|_| ApiError::Uncertain)?;
            let part = Outgoing {
                id: client_id,
                channel: o.channel.clone(),
                thread: o.thread.clone(),
                text,
                media: if position + 1 == indices.len() {
                    o.media.clone()
                } else {
                    vec![]
                },
            };
            match self.publish_message(&part, notice, None).await {
                Ok(ts) => {
                    store.lock().unwrap().db.execute("UPDATE outbox_parts SET status='sent',slack_ts=?3 WHERE outbox_id=?1 AND part=?2",rusqlite::params![o.id,index,ts]).map_err(|_|ApiError::Uncertain)?;
                    if first.is_none() {
                        first = Some(ts);
                    }
                }
                Err(error) => {
                    let state = match &error {
                        ApiError::RateLimited(_) => "ready",
                        ApiError::Rejected(_) => "failed",
                        ApiError::Uncertain => "uncertain",
                    };
                    store
                        .lock()
                        .unwrap()
                        .db
                        .execute(
                            "UPDATE outbox_parts SET status=?3 WHERE outbox_id=?1 AND part=?2",
                            rusqlite::params![o.id, index, state],
                        )
                        .map_err(|_| ApiError::Uncertain)?;
                    return Err(error);
                }
            }
        }
        first.ok_or(ApiError::Uncertain)
    }
    pub async fn publish(&self, o: &Outgoing) -> std::result::Result<String, ApiError> {
        self.publish_with_metadata(o, None).await
    }
    pub async fn publish_with_metadata(
        &self,
        o: &Outgoing,
        metadata: Option<&Value>,
    ) -> std::result::Result<String, ApiError> {
        let mut payload = o.clone();
        payload.text = crate::reply_parts::markdown(&o.text);
        self.publish_message(&payload, false, metadata).await
    }
    pub async fn permalink(
        &self,
        channel: &str,
        ts: &str,
    ) -> std::result::Result<String, ApiError> {
        let value = self
            .call(
                "chat.getPermalink",
                json!({"channel":channel,"message_ts":ts}),
                false,
            )
            .await?;
        let link = value["permalink"].as_str().ok_or(ApiError::Uncertain)?;
        let url = reqwest::Url::parse(link).map_err(|_| ApiError::Uncertain)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some_and(|p| p != 443)
            || !url
                .host_str()
                .is_some_and(|h| h == "slack.com" || h.ends_with(".slack.com"))
        {
            return Err(ApiError::Rejected("invalid_permalink".into()));
        }
        Ok(link.to_owned())
    }
    async fn publish_message(
        &self,
        o: &Outgoing,
        notice: bool,
        metadata: Option<&Value>,
    ) -> std::result::Result<String, ApiError> {
        // Upload bytes first without sharing them. Once the message has been
        // posted, any file-completion failure is uncertain, never safe to retry.
        let mut uploaded = Vec::new();
        for file in &o.media {
            let bytes = file
                .bytes()
                .map_err(|_| ApiError::Rejected("invalid_media".into()))?;
            let ticket = self
                .call(
                    "files.getUploadURLExternal",
                    json!({"filename":crate::media::filename(&file.name),"length":bytes.len()}),
                    false,
                )
                .await?;
            let url =
                crate::media::file_url(ticket["upload_url"].as_str().unwrap_or(""), self.test)
                    .map_err(|_| ApiError::Rejected("untrusted_upload_host".into()))?;
            // The upload URL itself grants this upload; never send bot credentials.
            let response = self
                .client
                .post(url)
                .header("content-type", "application/octet-stream")
                .body(bytes)
                .send()
                .await
                .map_err(|_| ApiError::Uncertain)?;
            if !response.status().is_success() {
                return Err(ApiError::Uncertain);
            }
            let id = ticket["file_id"]
                .as_str()
                .filter(|id| id.starts_with('F'))
                .ok_or(ApiError::Uncertain)?;
            uploaded.push(json!({"id":id,"title":file.name}));
        }
        // ACP replies remain ordinary Markdown in the outbox. Slack's `text`
        // field uses its different mrkdwn dialect; `markdown_text` asks Slack
        // to translate standard Markdown into its native rich message blocks.
        // Do not combine this field with `text` or `blocks`, or escape Markdown.
        // New payloads repair fence layout before planning. Persisted parts keep
        // their exact saved text and identity, including across software upgrades.
        // Each delivery part stays below Slack's 12,000-character limit.
        let mut p = json!({"channel":o.channel,"markdown_text":o.text,"client_msg_id":o.id,"unfurl_links":false,"unfurl_media":false,"parse":"none"});
        if let Some(metadata) = metadata {
            p["metadata"] = metadata.clone();
        }
        if notice {
            p["metadata"] =
                json!({"event_type":"acp_connector_notice","event_payload":{"id":o.id}});
        }
        if !o.thread.is_empty() {
            p["thread_ts"] = o.thread.clone().into();
            p["reply_broadcast"] = false.into();
        }
        let d = self.call("chat.postMessage", p, false).await?;
        let ts = d["ts"]
            .as_str()
            .map(str::to_owned)
            .ok_or(ApiError::Uncertain)?;
        if !uploaded.is_empty() {
            let thread = if o.thread.is_empty() { &ts } else { &o.thread };
            self.call(
                "files.completeUploadExternal",
                json!({"files":uploaded,"channel_id":o.channel,"thread_ts":thread}),
                false,
            )
            .await
            .map_err(|_| ApiError::Uncertain)?;
        }
        Ok(ts)
    }
    pub async fn download_file(&self, value: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let mut url = crate::media::file_url(value, self.test)?;
        for _ in 0..=3 {
            let mut response = self
                .client
                .get(url.clone())
                .bearer_auth(&self.secrets.bot_token)
                .send()
                .await?;
            if response.status().is_redirection() {
                let next = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .context("Missing redirect")?;
                // Re-validate before attaching credentials, including same-origin
                // relative redirects. There is no unauthenticated SSRF fallback.
                url = crate::media::file_url(url.join(next)?.as_str(), self.test)?;
                continue;
            }
            ensure!(response.status().is_success(), "Attachment unavailable");
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= max_bytes as u64),
                "Attachment too large"
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len() + chunk.len() <= max_bytes,
                    "Attachment too large"
                );
                bytes.extend_from_slice(&chunk);
            }
            return Ok(bytes);
        }
        anyhow::bail!("Too many attachment redirects")
    }
    pub async fn authorize_continuation(&self, c: &Config, source: &Message) -> Result<()> {
        ensure!(
            c.should_wake(&source.channel, &source.text) && !source.deleted,
            "Continuation scope changed"
        );
        let mut cursor = String::new();
        let mut actor = false;
        let mut agent = false;
        for _ in 0..5 {
            let value = self
                .call(
                    "conversations.members",
                    json!({"channel":source.channel,"limit":200,"cursor":cursor}),
                    false,
                )
                .await
                .map_err(|_| anyhow::anyhow!("Continuation audience could not be verified"))?;
            let members = value["members"]
                .as_array()
                .context("Continuation audience unavailable")?;
            actor |= members.iter().any(|m| m == &source.sender);
            agent |= members.iter().any(|m| m == &c.slack.bot_user_id);
            if actor && agent {
                break;
            }
            cursor = value["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or("")
                .into();
            if cursor.is_empty() {
                break;
            }
        }
        ensure!(actor && agent, "Continuation audience changed");
        let (method, body) = if source.thread.is_empty() {
            (
                "conversations.history",
                json!({"channel":source.channel,"oldest":source.ts,"latest":source.ts,"inclusive":true,"limit":2}),
            )
        } else {
            (
                "conversations.replies",
                json!({"channel":source.channel,"ts":source.thread,"oldest":source.ts,"latest":source.ts,"inclusive":true,"limit":2}),
            )
        };
        let value = self
            .call(method, body, false)
            .await
            .map_err(|_| anyhow::anyhow!("Continuation source could not be checked"))?;
        let raw = value["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["ts"] == source.ts)
            .context("Continuation source is unavailable")?;
        let current = normalize(&source.team, &source.channel, raw, None)
            .context("Continuation source is unavailable")?;
        // Existing stored triggers may predate structured-table projection.
        // Accept only the exact Slack fallback they captured; do not rewrite
        // them or relax any identity, revision, or audience check.
        let legacy_text: String = raw["text"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(40000)
            .collect();
        ensure!(
            !current.deleted
                && current.sender == source.sender
                && current.bot == source.bot
                && (current.text == source.text || legacy_text == source.text)
                && current.revision == source.revision,
            "Continuation source changed"
        );
        Ok(())
    }
    pub async fn find_message(
        &self,
        channel: &str,
        thread: &str,
        client_id: &str,
        since: i64,
    ) -> std::result::Result<Option<String>, ApiError> {
        let (method, body) = if thread.is_empty() {
            (
                "conversations.history",
                json!({"channel":channel,"oldest":format!("{}",since/1000-2),"limit":100}),
            )
        } else {
            (
                "conversations.replies",
                json!({"channel":channel,"ts":thread,"limit":100}),
            )
        };
        let data = self.call(method, body, false).await?;
        Ok(data["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| {
                ((!self.bot_user.is_empty() && m["user"] == self.bot_user)
                    || (!self.bot_id.is_empty() && m["bot_id"] == self.bot_id))
                    && (m["client_msg_id"] == client_id
                        || m["metadata"]["event_payload"]["id"] == client_id)
            })
            .and_then(|m| m["ts"].as_str())
            .map(str::to_owned))
    }
    pub async fn recent(&self, m: &Message) -> Result<Vec<Message>> {
        let (method, body) = if m.thread.is_empty() {
            (
                "conversations.history",
                json!({"channel":m.channel,"limit":80}),
            )
        } else {
            (
                "conversations.replies",
                json!({"channel":m.channel,"ts":m.thread,"limit":80}),
            )
        };
        let result = self
            .call(method, body, false)
            .await
            .map_err(|e| anyhow::anyhow!("Slack context fetch: {e:?}"))?;
        let mut out = Vec::new();
        for raw in result["messages"].as_array().into_iter().flatten() {
            if matches!(
                raw["metadata"]["event_type"].as_str(),
                Some("acp_permission" | "acp_question" | "acp_work_status")
            ) {
                continue;
            }
            if let Some(n) = normalize(&m.team, &m.channel, raw, None) {
                out.push(n)
            }
        }
        // The prompt states that this bounded view is not the full room archive.
        Ok(out)
    }
    /// Verify the requested peers rather than treating the bounded prompt roster
    /// as the complete channel membership list.
    pub async fn authorize_bot_members(&self, channel: &str, ids: &[String]) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let mut pending: std::collections::HashSet<&str> =
                ids.iter().map(String::as_str).collect();
            let mut cursor = String::new();
            let mut seen = std::collections::HashSet::new();
            while !pending.is_empty() {
                let page = self
                    .call(
                        "conversations.members",
                        json!({"channel":channel,"limit":200,"cursor":cursor}),
                        false,
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Peer membership is unavailable"))?;
                for member in page["members"]
                    .as_array()
                    .context("Peer membership is unavailable")?
                {
                    if let Some(id) = member.as_str() {
                        pending.remove(id);
                    }
                }
                if pending.is_empty() {
                    break;
                }
                cursor = page["response_metadata"]["next_cursor"]
                    .as_str()
                    .unwrap_or("")
                    .into();
                ensure!(
                    !cursor.is_empty(),
                    "A requested peer is not a member of this channel"
                );
                ensure!(
                    seen.insert(cursor.clone()),
                    "Peer membership pagination did not advance"
                );
            }
            for id in ids {
                let value = self
                    .call("users.info", json!({"user":id}), false)
                    .await
                    .map_err(|_| anyhow::anyhow!("Peer identity is unavailable"))?;
                ensure!(
                    value["user"]["id"] == *id
                        && value["user"]["is_bot"] == true
                        && value["user"]["deleted"] != true,
                    "Invited peers must be active bot members of this channel"
                );
            }
            Ok(())
        })
        .await
        .context("Peer verification timed out")?
    }
    pub async fn participants(&self, channel: &str) -> Vec<Value> {
        let Ok(r) = self
            .call(
                "conversations.members",
                json!({"channel":channel,"limit":40}),
                false,
            )
            .await
        else {
            return Vec::new();
        };
        let mut people = Vec::new();
        for id in r["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(40)
        {
            let cached = self.people.lock().unwrap().get(id).cloned();
            if let Some(p) = cached {
                people.push(p);
                continue;
            }
            if let Ok(r) = self.call("users.info", json!({"user":id}), false).await {
                let u = &r["user"];
                let profile = &u["profile"];
                let name = profile["display_name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| profile["real_name"].as_str())
                    .or_else(|| u["name"].as_str())
                    .unwrap_or(id);
                let p = json!({"id":id,"name":name,"is_bot":u["is_bot"]==true});
                self.people.lock().unwrap().insert(id.into(), p.clone());
                people.push(p);
            }
        }
        people
    }
    pub async fn listen(
        self,
        c: Config,
        home: std::path::PathBuf,
        store: Arc<Mutex<Store>>,
        notify: Arc<Notify>,
    ) -> Result<()> {
        let mut backoff = 1;
        loop {
            let r = self.connection(&c, &home, &store, &notify).await;
            let note = if r.is_ok() {
                "Slack requested reconnect"
            } else {
                "Slack connection unavailable"
            };
            store
                .lock()
                .unwrap()
                .audit("connection", json!({"state":"reconnecting","note":note}))?;
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    }
    async fn connection(
        &self,
        c: &Config,
        home: &std::path::Path,
        store: &Arc<Mutex<Store>>,
        notify: &Arc<Notify>,
    ) -> Result<()> {
        let d = self
            .call("apps.connections.open", json!({}), true)
            .await
            .map_err(|e| anyhow::anyhow!("Socket open: {e:?}"))?;
        let url = d["url"].as_str().context("Missing websocket URL")?;
        let u = reqwest::Url::parse(url)?;
        if self.test {
            ensure!(
                u.scheme() == "ws" && matches!(u.host_str(), Some("127.0.0.1" | "localhost")),
                "Invalid test websocket"
            )
        } else {
            ensure!(
                u.scheme() == "wss" && u.host_str().is_some_and(|h| h.ends_with(".slack.com")),
                "Unexpected Slack websocket host"
            )
        }
        let mut ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        ws_config.max_message_size = Some(2 * 1024 * 1024);
        ws_config.max_frame_size = Some(2 * 1024 * 1024);
        let (mut ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(ws_config), false)
            .await
            .map_err(|_| anyhow::anyhow!("Slack websocket connection failed"))?;
        store
            .lock()
            .unwrap()
            .audit("connection", json!({"state":"connecting"}))?;
        let mut verified_hello = false;
        let mut recovery = tokio::task::JoinSet::new();
        let mut recovered = false;
        let mut retry_at = tokio::time::Instant::now();
        let since = crate::reconnect::checkpoint(&store.lock().unwrap())?;
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
        loop {
            tokio::select! {
                _=ping.tick()=>{
                    ws.send(Ws::Ping(Vec::new().into())).await?;
                    if recovered { crate::reconnect::advance(&store.lock().unwrap(),crate::model::now_ms())?; }
                    else if verified_hello && recovery.is_empty() && tokio::time::Instant::now()>=retry_at {
                        let (client,c,s)=(self.clone(),c.clone(),store.clone());
                        recovery.spawn(async move {crate::reconnect::recover(&client,&c,s,since).await});
                    }
                },
                Some(result)=recovery.join_next(),if !recovery.is_empty()=>{
                    let delay=result.as_ref().ok().and_then(|r|r.as_ref().err()).and_then(|e|e.downcast_ref::<crate::reconnect::Retry>()).map_or(60,|r|r.0);
                    retry_at=tokio::time::Instant::now()+std::time::Duration::from_secs(delay);
                    recovered=matches!(result,Ok(Ok(_)));
                    if !recovered {store.lock().unwrap().audit("reconnect_recovery_incomplete",json!({"reason":"History recovery incomplete; checkpoint retained for retry"}))?;}
                    notify.notify_one();
                },
                frame=ws.next()=>{
                    let Some(frame)=frame else {return Ok(())};
                    match frame? {
                        Ws::Text(raw)=>{
                            let envelope:Value=serde_json::from_str(&raw)?;
                            if envelope["type"]=="disconnect" {return Ok(())}
                            if envelope["type"]=="hello" {
                                ensure!(envelope["connection_info"]["app_id"]==c.slack.app_id,"Socket token belongs to a different Slack app");
                                verified_hello=true;
                                let (client,c,s)=(self.clone(),c.clone(),store.clone());
                                recovery.spawn(async move {crate::reconnect::recover(&client,&c,s,since).await});
                                store.lock().unwrap().audit("connection",json!({"state":"connected"}))?;
                            }
                            if envelope["type"]=="events_api" {
                                ensure!(verified_hello,"Slack events arrived before app identity verification");
                                let payload=&envelope["payload"];
                                // Slack owns routing. An installation can never adopt another
                                // application's events or a workspace just because IDs overlap.
                                if payload["team_id"]==c.slack.team_id && payload["api_app_id"]==c.slack.app_id {
                                    receive(c,&mut store.lock().unwrap(),payload)?;
                                    notify.notify_one();
                                }
                            }
                            if envelope["type"]=="interactive" {
                                ensure!(verified_hello,"Slack interactions arrived before app identity verification");
                                let payload=&envelope["payload"];
                                let is_question_view=payload["view"]["callback_id"]=="acp_question" && matches!(payload["type"].as_str(),Some("view_submission"|"view_closed"));
                                let answer=if is_question_view {
                                    crate::questions::submit(home,&Config::load(home)?,payload).unwrap_or_else(|_|json!({"response_action":"update","view":{"type":"modal","title":{"type":"plain_text","text":"Question unavailable"},"close":{"type":"plain_text","text":"Close"},"blocks":[{"type":"section","text":{"type":"plain_text","text":"This question has closed or belongs to another person."}}]}}))
                                }else{json!({})};
                                if let Some(id)=envelope["envelope_id"].as_str(){ws.send(Ws::Text(json!({"envelope_id":id,"payload":answer}).to_string().into())).await?;}
                                if is_question_view {continue;}
                                if payload["actions"][0]["action_id"]=="acp_question_open" && crate::questions::open(home,&Config::load(home)?,self,payload).await.is_err(){store.lock().unwrap().audit("question_unavailable",json!({"reason":"Expired or unauthorized question"}))?;
                                }
                                if matches!(payload["actions"][0]["action_id"].as_str(),Some("acp_question_url"|"acp_question_decline")) && crate::questions::submit(home,&Config::load(home)?,payload).is_ok(){
                                        let _=self.call("views.update",json!({"view_id":payload["view"]["id"],"view":{"type":"modal","title":{"type":"plain_text","text":"Response sent"},"close":{"type":"plain_text","text":"Close"},"blocks":[{"type":"section","text":{"type":"plain_text","text":"Your choice was sent to the agent. If you opened a sign-in page, finish there."}}]}}),false).await;
                                }
                                if payload["team"]["id"]==c.slack.team_id && payload["api_app_id"]==c.slack.app_id
                                    && payload["actions"][0]["action_id"] == "acp_permission" {
                                    let latest=Config::load(home)?;
                                    let s=store.lock().unwrap();
                                    if crate::permissions::action(&s,&latest,payload).is_err() {
                                        s.audit("permission_action_rejected",json!({"reason":"Expired, mismatched, unauthorized, or already resolved permission control"}))?;
                                    }
                                }
                                if payload["team"]["id"]==c.slack.team_id && payload["api_app_id"]==c.slack.app_id
                                    && payload["actions"][0]["action_id"] == "permission_details" {
                                    let latest=Config::load(home)?;
                                    if crate::permissions::details(home,&latest,self,payload).await.is_err() {
                                        store.lock().unwrap().audit("permission_details_unavailable",json!({"reason":"Permission details were unavailable or not authorized for this member"}))?;
                                    }
                                }
                            }
                            if envelope["type"]!="interactive" && let Some(id)=envelope["envelope_id"].as_str(){
                                ws.send(Ws::Text(json!({"envelope_id":id}).to_string().into())).await?;
                            }
                        },
                        Ws::Ping(bytes)=>ws.send(Ws::Pong(bytes)).await?,
                        Ws::Close(_)=>return Ok(()),
                        _=>{}
                    }
                }
            }
        }
    }
}

/// Structured connector UI is not conversation evidence, regardless of author.
/// Never infer controls from visible text: a person may discuss "Thinking".
pub fn control_message(e: &Value) -> bool {
    [e, &e["message"], &e["previous_message"]].iter().any(|m| {
        matches!(
            m["metadata"]["event_type"].as_str(),
            Some("acp_permission" | "acp_question" | "acp_work_status" | "acp_connector_notice")
        )
    })
}

pub fn known_control(store: &Store, channel: &str, ts: &str) -> Result<bool> {
    let known: bool = store.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM ignored_activity WHERE channel=?1 AND ts=?2 UNION ALL SELECT 1 FROM permissions WHERE channel=?1 AND slack_ts=?2 UNION ALL SELECT 1 FROM outbox WHERE channel=?1 AND slack_ts=?2 AND connector_notice=1)",
        rusqlite::params![channel, ts], |r| r.get(0),
    )?;
    let indicators: bool = store.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='response_indicators')", [], |r| r.get(0))?;
    Ok(known
        || (indicators
            && store.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM response_indicators WHERE channel=?1 AND slack_ts=?2)",
                rusqlite::params![channel, ts],
                |r| r.get::<_, bool>(0),
            )?))
}

pub fn normalize(team: &str, channel: &str, e: &Value, prior: Option<Message>) -> Option<Message> {
    if control_message(e) {
        return None;
    }
    let subtype = e["subtype"].as_str().unwrap_or("");
    let changed = subtype == "message_changed";
    if subtype == "message_deleted" {
        let ts = e["deleted_ts"].as_str()?;
        let mut old = prior
            .or_else(|| normalize(team, channel, &e["previous_message"], None))
            .unwrap_or(Message {
                files: vec![],
                team: team.into(),
                channel: channel.into(),
                thread: String::new(),
                ts: ts.into(),
                revision: String::new(),
                sender: String::new(),
                bot: false,
                text: String::new(),
                deleted: true,
            });
        old.deleted = true;
        old.text.clear();
        old.files.clear();
        old.revision = e["event_ts"].as_str().unwrap_or(ts).into();
        return Some(old);
    }
    if ![
        "",
        "bot_message",
        "message_changed",
        "thread_broadcast",
        "file_share",
    ]
    .contains(&subtype)
    {
        return None;
    }
    let raw = if changed { &e["message"] } else { e };
    let ts = raw["ts"].as_str()?;
    let sender = raw["user"].as_str().or_else(|| raw["bot_id"].as_str())?;
    let bot = raw.get("bot_id").is_some() || raw["subtype"] == "bot_message";
    // A bot may invite discussion in its own root's thread. This changes only
    // the reply destination within the same channel, never source authority.
    // Slack may omit metadata on later bookkeeping events; retain the observed
    // self-thread route rather than creating a second channel-context job.
    let coordination_root = bot
        && (raw["metadata"]["event_type"] == "acp_coordination_thread"
            || prior
                .as_ref()
                .is_some_and(|m| m.bot && m.ts == ts && m.thread == ts));
    let thread = raw["thread_ts"]
        .as_str()
        .filter(|s| *s != ts)
        .unwrap_or(if coordination_root { ts } else { "" });
    Some(Message {
        team: team.into(),
        channel: channel.into(),
        thread: thread.into(),
        ts: ts.into(),
        revision: raw["edited"]["ts"]
            .as_str()
            .or_else(|| {
                if changed {
                    e["event_ts"].as_str()
                } else {
                    None
                }
            })
            .unwrap_or(ts)
            .into(),
        sender: sender.into(),
        bot,
        text: crate::message_content::text(raw)
            .chars()
            .take(40000)
            .collect(),
        deleted: false,
        files: raw["files"]
            .as_array()
            .into_iter()
            .flatten()
            .take(8)
            .filter_map(crate::media::SlackFile::from_slack)
            .collect(),
    })
}

pub fn receive(c: &Config, store: &mut Store, p: &Value) -> Result<()> {
    let e = &p["event"];
    if matches!(
        e["type"].as_str(),
        Some("reaction_added" | "reaction_removed")
    ) {
        return crate::reactions::receive(c, store, p);
    }
    if !["message", "app_mention"].contains(&e["type"].as_str().unwrap_or("")) {
        return Ok(());
    }
    let Some(channel) = e["channel"].as_str() else {
        return Ok(());
    };
    if !c.allows(channel, e["channel_type"].as_str().unwrap_or("")) {
        return Ok(());
    }
    let activity_ts = e["deleted_ts"]
        .as_str()
        .or_else(|| e["message"]["ts"].as_str())
        .or_else(|| e["ts"].as_str())
        .unwrap_or("");
    // Reconcile own UI delivery before filtering it from model context. An echo
    // can confirm a notice or permission post whose HTTP response was lost.
    let raw = if e["subtype"] == "message_changed" {
        &e["message"]
    } else {
        e
    };
    if (raw["user"] == c.slack.bot_user_id || raw["bot_id"] == c.slack.bot_id)
        && let (Some(id), Some(ts)) = (raw["client_msg_id"].as_str(), raw["ts"].as_str())
    {
        store.echo(id, ts)?;
        store.db.execute(
            "UPDATE permissions SET slack_ts=COALESCE(slack_ts,?2) WHERE id=?1 AND channel=?3",
            rusqlite::params![id, ts, channel],
        )?;
    }
    if known_control(store, channel, activity_ts)? || control_message(e) {
        if !activity_ts.is_empty() {
            store.db.execute(
                "INSERT OR REPLACE INTO ignored_activity VALUES(?1,?2,?3)",
                rusqlite::params![channel, activity_ts, crate::model::now_ms()],
            )?;
            store.db.execute("DELETE FROM ignored_activity WHERE seen<?1 OR rowid NOT IN (SELECT rowid FROM ignored_activity ORDER BY seen DESC LIMIT 10000)",[crate::model::now_ms()-3*86400000])?;
        }
        return Ok(());
    }
    let prior = if activity_ts.is_empty() {
        None
    } else {
        store.message(channel, activity_ts)?
    };
    let Some(m) = normalize(&c.slack.team_id, channel, e, prior) else {
        return Ok(());
    };
    let own = m.sender == c.slack.bot_user_id
        || m.sender == c.slack.bot_id
        || e["bot_id"] == c.slack.bot_id;
    if own && let Some(id) = e["client_msg_id"].as_str() {
        let found = store.db.execute(
            "UPDATE permissions SET slack_ts=COALESCE(slack_ts,?2) WHERE id=?1 AND channel=?3",
            rusqlite::params![id, m.ts, m.channel],
        )?;
        if found > 0 {
            return Ok(());
        }
    }
    if own
        && matches!(
            e["metadata"]["event_type"].as_str(),
            Some("acp_permission" | "acp_question" | "acp_work_status")
        )
    {
        return Ok(());
    }
    if own
        && store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM permissions WHERE channel=?1 AND slack_ts=?2)",
            rusqlite::params![m.channel, m.ts],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Ok(());
    }
    if let Some(id) = p["event_id"].as_str() {
        let active_peer: bool = m.bot
            && store.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE context=?1 AND status='running')",
                [m.context()],
                |r| r.get(0),
            )?;
        store.ingest(
            id,
            &m,
            !own && (c.should_wake(&m.channel, &m.text) || active_peer),
        )?;
    }
    Ok(())
}

/// Validate credentials through Slack without publishing or receiving messages.
pub async fn verify_credentials(c: &Config, secrets: Secrets) -> Result<Value> {
    if c.slack.loopback_test {
        ensure!(
            secrets.app_token.starts_with("test-") && secrets.bot_token.starts_with("test-"),
            "Test transport refuses real credentials"
        );
    } else {
        ensure!(
            secrets.app_token.starts_with("xapp-") && secrets.bot_token.starts_with("xoxb-"),
            "Supply the app token and bot token from Slack"
        );
    }
    let client = Slack::new(c, secrets)?;
    let auth = client
        .call("auth.test", json!({}), false)
        .await
        .map_err(|e| anyhow::anyhow!("Slack bot authentication: {e:?}"))?;
    let user = auth["user_id"]
        .as_str()
        .context("Slack returned no bot user")?;
    let bot = auth["bot_id"]
        .as_str()
        .context("Slack returned no bot identity")?;
    let team = auth["team_id"]
        .as_str()
        .context("Slack returned no workspace")?;
    // The bot identity establishes which app owns the bot token. Socket envelopes
    // are checked against this app and workspace before accepting any messages.
    let info = client
        .call("bots.info", json!({"bot":bot}), false)
        .await
        .map_err(|e| anyhow::anyhow!("Slack app identity: {e:?}"))?;
    let app = info["bot"]["app_id"]
        .as_str()
        .context("Slack returned no application identity")?;
    if !c.slack.app_id.is_empty() {
        ensure!(
            c.slack.app_id == app,
            "Bot token belongs to a different Slack app"
        );
    }
    let socket = client
        .call("apps.connections.open", json!({}), true)
        .await
        .map_err(|e| anyhow::anyhow!("Slack Socket Mode: {e:?}"))?;
    let url = socket["url"].as_str().context("No Slack socket URL")?;
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        if c.slack.loopback_test {
            parsed.scheme() == "ws" && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"))
        } else {
            parsed.scheme() == "wss" && parsed.host_str().is_some_and(|h| h.ends_with(".slack.com"))
        },
        "Unexpected Slack socket address"
    );
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|_| anyhow::anyhow!("Could not connect to Slack Socket Mode"))?;
        let mut matched = false;
        while let Some(frame) = ws.next().await {
            match frame? {
                Ws::Text(raw) => {
                    let value: Value = serde_json::from_str(&raw)?;
                    if value["type"] == "hello" {
                        matched = value["connection_info"]["app_id"] == app;
                        break;
                    }
                    ensure!(
                        value["type"] != "events_api",
                        "Stop this app's existing connection before changing its credentials"
                    );
                }
                Ws::Ping(p) => ws.send(Ws::Pong(p)).await?,
                Ws::Close(_) => break,
                _ => {}
            }
        }
        let _ = ws.close(None).await;
        ensure!(
            matched,
            "The app token and bot token belong to different Slack apps"
        );
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Slack Socket Mode check timed out")??;
    Ok(
        json!({"ok":true,"team_id":team,"team_name":auth["team"],"bot_user_id":user,"bot_id":bot,"app_id":app,"media":client.media_readiness(c)}),
    )
}

fn parse_scopes(header: &str) -> Vec<String> {
    let mut scopes: Vec<String> = header
        .split(',')
        .map(str::trim)
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 100
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'_' || b == b'.')
        })
        .map(str::to_owned)
        .collect();
    scopes.sort();
    scopes.dedup();
    scopes
}
pub fn media_readiness(c: &Config, scopes: Option<&[String]>) -> Value {
    let has = |scope: &str| scopes.is_some_and(|s| s.iter().any(|v| v == scope));
    json!({"verified":scopes.is_some(),"checked_at":crate::model::now_ms(),
      "granted_scopes":scopes,"missing_scopes":(["files:read","files:write"].into_iter().filter(|s| !has(s)).collect::<Vec<_>>()),
      "incoming":{"enabled":c.media.incoming,"permitted":has("files:read")},
      "outgoing":{"enabled":c.media.outgoing,"permitted":has("files:write")},
      "ready":has("files:read") && has("files:write"),
      "settings_url":if c.slack.app_id.is_empty(){"https://api.slack.com/apps".into()}else{format!("https://api.slack.com/apps/{}/oauth",c.slack.app_id)},
      "note":if scopes.is_none(){"Slack did not return its granted scopes. Verify again; the manifest alone does not confirm access."}else if has("files:read") && has("files:write") && c.media.incoming && c.media.outgoing {"Files and images are ready."}else if has("files:read") && has("files:write"){"File access is available; this connection's attachment settings still apply."}else{"Add the missing Bot Token Scopes in OAuth & Permissions, then reinstall this existing app to the workspace. Recheck permissions afterward; creating a new app is unnecessary."}})
}

pub fn manifest(label: &str) -> Value {
    json!({"display_information":{"name":label.chars().take(80).collect::<String>(),"description":"Connect this agent to Slack through its ACP endpoint.","background_color":"#176d64"},
      "features":{"bot_user":{"display_name":label.chars().take(80).collect::<String>(),"always_online":false},"app_home":{"home_tab_enabled":false,"messages_tab_enabled":true,"messages_tab_read_only_enabled":false}},
      "oauth_config":{"scopes":{"bot":["app_mentions:read","channels:history","channels:read","chat:write","reactions:read","reactions:write","files:read","files:write","groups:history","groups:read","im:history","im:read","im:write","mpim:history","mpim:read","users:read"]}},
      "settings":{"org_deploy_enabled":false,"interactivity":{"is_enabled":true},"socket_mode_enabled":true,"token_rotation_enabled":false,"event_subscriptions":{"bot_events":["app_mention","message.channels","message.groups","message.im","message.mpim","reaction_added","reaction_removed"]}}})
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    #[test]
    fn media_readiness_uses_granted_scopes_not_requested_manifest() {
        let c = Config::new("File access".into(), "tester".into());
        assert_eq!(media_readiness(&c, None)["verified"], false);
        let read = parse_scopes("chat:write, files:read,files:read");
        let partial = media_readiness(&c, Some(&read));
        assert_eq!(partial["ready"], false);
        assert_eq!(partial["incoming"]["permitted"], true);
        assert_eq!(partial["missing_scopes"], json!(["files:write"]));
        let both = parse_scopes("files:read, files:write");
        let complete = media_readiness(&c, Some(&both));
        assert_eq!(complete["ready"], true);
        assert_eq!(complete["incoming"]["enabled"], c.media.incoming);
        assert_eq!(complete["missing_scopes"], json!([]));
    }
}
