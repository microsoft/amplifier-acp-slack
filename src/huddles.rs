//! Source-bound Slack discussion threads. Delivery remains in the ordinary
//! public-speech journal; this module adds no scheduler or work authority.
use crate::{
    config::Config,
    model::{Message, now_ms},
    slack::Slack,
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    adapter_id: String,
    team: String,
    bot_user: String,
    source: Message,
    purpose: String,
    peers: Vec<String>,
    origin_url: Option<String>,
}
struct Huddle {
    id: String,
    root_delivery: String,
    binding: Binding,
    inspect_url: Option<String>,
}

pub fn initialize(s: &Store) -> Result<()> {
    s.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS slack_huddles(
      id TEXT PRIMARY KEY,origin_key TEXT NOT NULL UNIQUE,origin_context TEXT NOT NULL,
      channel TEXT NOT NULL,root_delivery TEXT NOT NULL,binding TEXT NOT NULL,
      created INTEGER NOT NULL,inspect_url TEXT);
      CREATE TABLE IF NOT EXISTS slack_huddle_operations(
      id TEXT PRIMARY KEY,huddle_id TEXT NOT NULL,job_id TEXT NOT NULL,kind TEXT NOT NULL,
      payload TEXT NOT NULL,created INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS slack_huddle_parts(
      delivery_id TEXT PRIMARY KEY,operation_id TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS slack_huddle_jobs ON slack_huddle_operations(job_id);",
    )?;
    Ok(())
}

pub fn tools() -> Vec<Value> {
    vec![
        json!({"name":"slack_huddle_ensure","description":"Create or reuse an inspectable Slack discussion thread for routine coordination, separate from the originating conversation. Not an audio huddle or a private room. Use the existing coordination thread when already in one. One initiating agent owns returning meaningful results; peer participation grants no new work authority. A pending or uncertain receipt is not delivery.","inputSchema":{"type":"object","properties":{"purpose":{"type":"string","minLength":1,"maxLength":400},"peer_user_ids":{"type":"array","items":{"type":"string"},"maxItems":8}},"required":["purpose"],"additionalProperties":false}}),
        json!({"name":"slack_huddle_status","description":"Read the saved discussion thread and actual delivery receipts. Unknown sends are checked by their original IDs, never repeated. Inspect or join using the returned Slack link, when available.","inputSchema":{"type":"object","properties":{"huddle_id":{"type":"string","format":"uuid"}},"additionalProperties":false}}),
        json!({"name":"slack_huddle_post","description":"Put routine coordination in a saved discussion thread without broadcasting it to the channel. Reuse the same request_id only for the identical message. Wait for a confirmed root before posting. This does not authorize additional work.","inputSchema":{"type":"object","properties":{"huddle_id":{"type":"string","format":"uuid"},"request_id":{"type":"string","format":"uuid"},"text":{"type":"string","minLength":1,"maxLength":100000}},"required":["huddle_id","request_id","text"],"additionalProperties":false}}),
        json!({"name":"slack_huddle_result","description":"Return meaningful progress, a result, blocker or decision to this thread's exact original conversation. Routine back-and-forth stays in the discussion thread. The connector fixes the destination; no new work authority or proof of completion is implied. Reuse request_id to inspect an uncertain send, not to send it again.","inputSchema":{"type":"object","properties":{"huddle_id":{"type":"string","format":"uuid"},"request_id":{"type":"string","format":"uuid"},"text":{"type":"string","minLength":1,"maxLength":100000},"kind":{"type":"string","enum":["progress","result","blocked","decision"]}},"required":["huddle_id","request_id","text","kind"],"additionalProperties":false}}),
    ]
}

fn load(db: &Connection, id: &str) -> Result<Huddle> {
    let (id, root_delivery, raw, inspect_url): (String, String, String, Option<String>) = db
        .query_row(
            "SELECT id,root_delivery,binding,inspect_url FROM slack_huddles WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .context("Saved discussion thread is unavailable on this connection")?;
    Ok(Huddle {
        id,
        root_delivery,
        binding: serde_json::from_str(&raw)?,
        inspect_url,
    })
}
fn current_source(db: &Connection, c: &Config, source: &Message) -> Result<bool> {
    let raw: Option<String> = db
        .query_row(
            "SELECT body FROM messages WHERE channel=?1 AND ts=?2",
            params![source.channel, source.ts],
            |r| r.get(0),
        )
        .optional()?;
    let current = raw
        .map(|v| serde_json::from_str::<Message>(&v))
        .transpose()?;
    Ok(c.enabled
        && source.team == c.slack.team_id
        && !source.channel.starts_with('D')
        && c.allows(&source.channel, "")
        && c.should_wake(&source.channel, &source.text)
        && current.is_some_and(|m| {
            !m.deleted
                && m.revision == source.revision
                && m.sender == source.sender
                && m.bot == source.bot
                && m.text == source.text
                && m.thread == source.thread
        }))
}
fn original_current(db: &Connection, c: &Config, h: &Huddle) -> Result<bool> {
    Ok(h.binding.adapter_id == c.id
        && h.binding.team == c.slack.team_id
        && h.binding.bot_user == c.slack.bot_user_id
        && current_source(db, c, &h.binding.source)?)
}
fn job_source(db: &Connection, c: &Config, job: &str) -> Result<Message> {
    let (raw,state,version,control):(String,String,i64,i64)=db.query_row(
        "SELECT j.trigger,j.status,j.version,x.control_version FROM jobs j JOIN contexts x ON x.id=j.context WHERE j.id=?1",[job],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let source: Message = serde_json::from_str(&raw)?;
    ensure!(
        state == "running" && version >= control && current_source(db, c, &source)?,
        "Current conversation source or connection changed"
    );
    Ok(source)
}
// Reserve the writer before reading source state. A deferred transaction can
// read successfully while the publisher owns the writer, then fail its upgrade
// immediately (SQLITE_BUSY) instead of honoring the connection busy timeout.
fn source_transaction<'a>(
    s: &'a mut Store,
    c: &Config,
    job: &str,
) -> Result<rusqlite::Transaction<'a>> {
    let tx =
        s.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    job_source(&tx, c, job)?;
    Ok(tx)
}

fn root(db: &Connection, h: &Huddle) -> Result<(String, Option<String>)> {
    Ok(db.query_row(
        "SELECT status,slack_ts FROM public_speech WHERE id=?1",
        [&h.root_delivery],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}
fn accessible(db: &Connection, c: &Config, h: &Huddle, source: &Message) -> Result<bool> {
    let (_, ts) = root(db, h)?;
    Ok(h.binding.adapter_id == c.id
        && h.binding.team == source.team
        && h.binding.bot_user == c.slack.bot_user_id
        && source.channel == h.binding.source.channel
        && (source.context() == h.binding.source.context()
            || ts
                .as_ref()
                .is_some_and(|v| !source.thread.is_empty() && &source.thread == v)))
}
fn mapped(db: &Connection, c: &Config, source: &Message) -> Result<Option<Huddle>> {
    let ids=db.prepare("SELECT h.id FROM slack_huddles h JOIN public_speech p ON p.id=h.root_delivery WHERE h.channel=?1 AND (h.origin_context=?2 OR p.slack_ts=?3) ORDER BY h.created DESC LIMIT 20")?
        .query_map(params![source.channel,source.context(),source.thread],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let h = load(db, &id)?;
        if accessible(db, c, &h, source)? {
            return Ok(Some(h));
        }
    }
    Ok(None)
}
// The transport has placed a bot-created root in its own native thread.
// Later replies and human joins reuse that same thread without borrowing its
// creator's original-source mapping or gaining a return-route capability.
fn existing_coordination_thread(db: &Connection, source: &Message) -> Result<bool> {
    if source.thread.is_empty() {
        return Ok(false);
    }
    let raw: Option<String> = db
        .query_row(
            "SELECT body FROM messages WHERE channel=?1 AND ts=?2",
            params![source.channel, source.thread],
            |r| r.get(0),
        )
        .optional()?;
    let root = raw
        .map(|s| serde_json::from_str::<Message>(&s))
        .transpose()?;
    Ok(root.is_some_and(|m| {
        !m.deleted
            && m.bot
            && m.team == source.team
            && m.channel == source.channel
            && m.ts == source.thread
            && m.thread == m.ts
    }))
}

fn operation(db: &Connection, id: &str) -> Result<Option<Value>> {
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT kind,payload,huddle_id FROM slack_huddle_operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kind, _, huddle)) = row else {
        return Ok(None);
    };
    let receipts=db.prepare("SELECT p.id,p.status,p.slack_ts,p.note FROM public_speech p JOIN slack_huddle_parts d ON d.delivery_id=p.id WHERE d.operation_id=?1 ORDER BY p.part")?
        .query_map([id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"slack_ts":r.get::<_,Option<String>>(2)?,"note":r.get::<_,String>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let status = if receipts.iter().all(|r| r["status"] == "sent") {
        "sent"
    } else if receipts.iter().any(|r| r["status"] == "uncertain") {
        "uncertain"
    } else if receipts.iter().any(|r| r["status"] == "failed") {
        "failed"
    } else if receipts.iter().any(|r| r["status"] == "cancelled") {
        "cancelled"
    } else {
        "pending"
    };
    Ok(Some(
        json!({"request_id":id,"huddle_id":huddle,"kind":kind,"status":status,"delivery_ids":receipts.iter().map(|r|r["id"].clone()).collect::<Vec<_>>(),"receipts":receipts}),
    ))
}
fn descriptor(db: &Connection, h: &Huddle) -> Result<Value> {
    let (status, root_ts) = root(db, h)?;
    let ids=db.prepare("SELECT id FROM slack_huddle_operations WHERE huddle_id=?1 ORDER BY created DESC,rowid DESC LIMIT 12")?
        .query_map([&h.id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let ops = ids
        .iter()
        .map(|id| operation(db, id))
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"huddle_id":h.id,"purpose":h.binding.purpose,"status":if status=="ready"||status=="sending"{"pending"}else{status.as_str()},"channel":h.binding.source.channel,"root_ts":root_ts,"inspect_url":h.inspect_url,"peer_user_ids":h.binding.peers,
        "origin":{"channel":h.binding.source.channel,"thread":h.binding.source.thread,"source_ts":h.binding.source.ts,"source_revision":h.binding.source.revision,"source_actor":h.binding.source.sender,"source_is_bot":h.binding.source.bot,"inspect_url":h.binding.origin_url},
        "operations":ops,"work_authority":"unchanged","completion":"not_established","kind":"slack_discussion_thread"}),
    )
}
fn put_operation(
    db: &Connection,
    huddle: &str,
    job: &str,
    request: &str,
    kind: &str,
    payload: &Value,
    delivery: (&str, &str, &str),
) -> Result<()> {
    let (text, channel, thread) = delivery;
    let encoded = serde_json::to_string(payload)?;
    let old: Option<(String, String, String)> = db
        .query_row(
            "SELECT huddle_id,kind,payload FROM slack_huddle_operations WHERE id=?1",
            [request],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some(old) = old {
        ensure!(
            old == (huddle.into(), kind.into(), encoded),
            "A saved request ID cannot change its thread, kind or message"
        );
        return Ok(());
    }
    db.execute("INSERT INTO slack_huddle_operations(id,huddle_id,job_id,kind,payload,created) VALUES(?1,?2,?3,?4,?5,?6)",params![request,huddle,job,kind,encoded,now_ms()])?;
    let ids = crate::public_speech::enqueue_routed(db, job, request, text, channel, thread)?;
    ensure!(!ids.is_empty(), "Discussion messages cannot be empty");
    for id in ids {
        db.execute(
            "INSERT INTO slack_huddle_parts(delivery_id,operation_id) VALUES(?1,?2)",
            params![id, request],
        )?;
    }
    Ok(())
}
fn text_arg<'a>(args: &'a Value, key: &str, max: usize) -> Result<&'a str> {
    let text = args[key]
        .as_str()
        .with_context(|| format!("{key} must be text"))?;
    ensure!(
        !text.trim().is_empty() && text.chars().count() <= max,
        "Invalid {key} length"
    );
    Ok(text)
}
fn uuid_arg(args: &Value, key: &str) -> Result<String> {
    Ok(Uuid::parse_str(text_arg(args, key, 36)?)?.to_string())
}
fn known_args(args: &Value, allowed: &[&str]) -> Result<()> {
    let args = args.as_object().context("Arguments must be an object")?;
    ensure!(
        args.keys().all(|k| allowed.contains(&k.as_str())),
        "Unsupported discussion argument"
    );
    Ok(())
}
async fn refresh_link(home: &Path, h: &Huddle, client: &Slack) -> Result<()> {
    let (_, ts) = {
        let s = Store::open(home)?;
        root(&s.db, h)?
    };
    if h.inspect_url.is_none()
        && let Some(ts) = ts
        && let Ok(url) = client.permalink(&h.binding.source.channel, &ts).await
    {
        let s = Store::open(home)?;
        s.db.execute(
            "UPDATE slack_huddles SET inspect_url=?2 WHERE id=?1 AND inspect_url IS NULL",
            params![h.id, url],
        )?;
    }
    Ok(())
}

pub async fn call(
    c: &Config,
    home: &Path,
    job: &str,
    name: &str,
    args: &Value,
    channel_scope: bool,
) -> Result<Value> {
    ensure!(
        channel_scope,
        "Discussion controls require the current channel audience; direct and thread-restricted grants cannot expand it"
    );
    let mut s = Store::open(home)?;
    let source = job_source(&s.db, c, job)?;
    let client = Slack::new(c, c.secrets(home)?)?;
    match name {
        "slack_huddle_ensure" => {
            known_args(args, &["purpose", "peer_user_ids"])?;
            let purpose = text_arg(args, "purpose", 400)?.to_owned();
            // Creator mappings are reused from their actual side thread. A peer
            // receives the root as its own thread through transport metadata.
            if !source.thread.is_empty() {
                if let Some(h) = mapped(&s.db, c, &source)?
                    && root(&s.db, &h)?.1.as_deref() == Some(&source.thread)
                {
                    refresh_link(home, &h, &client).await?;
                    let mut value = descriptor(&s.db, &load(&s.db, &h.id)?)?;
                    value["reused"] = true.into();
                    return Ok(value);
                }
                if existing_coordination_thread(&s.db, &source)? {
                    return Ok(
                        json!({"huddle_id":null,"status":"existing_thread","channel":source.channel,"root_ts":source.thread,"inspect_url":client.permalink(&source.channel,&source.thread).await.ok(),"reused":true,"return_route_owned_here":false,"guidance":"Coordinate in this existing Slack thread. Its initiating agent owns returning results; no new work authority is granted."}),
                    );
                }
            }
            let origin_key = serde_json::to_string(&(c.id.as_str(), &source))?;
            let id = Uuid::new_v5(&Uuid::NAMESPACE_URL, origin_key.as_bytes()).to_string();
            if s.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM slack_huddles WHERE id=?1)",
                [&id],
                |r| r.get::<_, bool>(0),
            )? {
                let h = load(&s.db, &id)?;
                refresh_link(home, &h, &client).await?;
                let mut value = descriptor(&s.db, &load(&s.db, &id)?)?;
                value["reused"] = true.into();
                return Ok(value);
            }
            let mut peers = Vec::new();
            if let Some(values) = args.get("peer_user_ids") {
                let values = values
                    .as_array()
                    .context("peer_user_ids must be an array")?;
                ensure!(values.len() <= 8, "Too many thread participants");
                for value in values {
                    let id = value.as_str().context("A peer must have a Slack user ID")?;
                    ensure!(
                        (id.starts_with('U') || id.starts_with('W'))
                            && id.len() <= 80
                            && id.chars().all(|x| x.is_ascii_alphanumeric()),
                        "Invalid Slack user ID"
                    );
                    ensure!(
                        id != c.slack.bot_user_id,
                        "Do not invite this agent to its own thread"
                    );
                    if !peers.iter().any(|p| p == id) {
                        peers.push(id.to_owned());
                    }
                }
            }
            client.authorize_continuation(c, &source).await?;
            if !peers.is_empty() {
                client
                    .authorize_bot_members(&source.channel, &peers)
                    .await?;
            }
            let origin_url = client.permalink(&source.channel, &source.ts).await.ok();
            let mentions = peers
                .iter()
                .map(|id| format!("<@{id}>"))
                .collect::<Vec<_>>()
                .join(" ");
            let origin = origin_url
                .as_ref()
                .map(|v| format!("Original conversation: {v}"))
                .unwrap_or_else(|| {
                    format!(
                        "Original conversation: {} message {}",
                        source.channel, source.ts
                    )
                });
            let text = format!(
                "Coordination thread: {purpose}\n{mentions}\n{origin}\nRoutine coordination stays here; meaningful results go back to the original conversation. Anyone in this channel can inspect or join."
            );
            let binding = Binding {
                adapter_id: c.id.clone(),
                team: c.slack.team_id.clone(),
                bot_user: c.slack.bot_user_id.clone(),
                source: source.clone(),
                purpose: purpose.clone(),
                peers: peers.clone(),
                origin_url,
            };
            let request =
                Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("huddle-root:{id}").as_bytes())
                    .to_string();
            // Network reads finish before reserving the writer and rereading authority.
            let tx = source_transaction(&mut s, c, job)?;
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM slack_huddles WHERE id=?1)",
                [&id],
                |r| r.get::<_, bool>(0),
            )? {
                put_operation(
                    &tx,
                    &id,
                    job,
                    &request,
                    "root",
                    &json!({"purpose":purpose,"peers":peers}),
                    (&text, &source.channel, ""),
                )?;
                let root_delivery: String = tx.query_row(
                    "SELECT delivery_id FROM slack_huddle_parts WHERE operation_id=?1",
                    [&request],
                    |r| r.get(0),
                )?;
                tx.execute("INSERT INTO slack_huddles(id,origin_key,origin_context,channel,root_delivery,binding,created) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,origin_key,source.context(),source.channel,root_delivery,serde_json::to_string(&binding)?,now_ms()])?;
            }
            tx.commit()?;
            let mut value = descriptor(&s.db, &load(&s.db, &id)?)?;
            value["reused"] = false.into();
            value["operation"] = operation(&s.db, &request)?.unwrap_or(Value::Null);
            Ok(value)
        }
        "slack_huddle_status" => {
            known_args(args, &["huddle_id"])?;
            let h = if args.get("huddle_id").is_some() {
                load(&s.db, &uuid_arg(args, "huddle_id")?)?
            } else {
                mapped(&s.db, c, &source)?
                    .context("No saved discussion thread for this conversation")?
            };
            ensure!(
                accessible(&s.db, c, &h, &source)?,
                "Discussion thread belongs to a different conversation"
            );
            crate::public_speech::reconcile(home, &client).await?;
            refresh_link(home, &h, &client).await?;
            descriptor(&s.db, &load(&s.db, &h.id)?)
        }
        "slack_huddle_post" | "slack_huddle_result" => {
            let result = name == "slack_huddle_result";
            known_args(
                args,
                if result {
                    &["huddle_id", "request_id", "text", "kind"]
                } else {
                    &["huddle_id", "request_id", "text"]
                },
            )?;
            let id = uuid_arg(args, "huddle_id")?;
            let request = uuid_arg(args, "request_id")?;
            let text = text_arg(args, "text", crate::model::MAX_REPLY_CHARS)?;
            let kind = if result {
                let kind = text_arg(args, "kind", 16)?;
                ensure!(
                    ["progress", "result", "blocked", "decision"].contains(&kind),
                    "Unknown result kind"
                );
                kind
            } else {
                "post"
            };
            let h = load(&s.db, &id)?;
            ensure!(
                accessible(&s.db, c, &h, &source)? && original_current(&s.db, c, &h)?,
                "Original discussion source or audience changed"
            );
            let payload = json!({"text":text,"kind":kind});
            if let Some(old) = operation(&s.db, &request)? {
                let stored: (String, String, String) = s.db.query_row(
                    "SELECT huddle_id,kind,payload FROM slack_huddle_operations WHERE id=?1",
                    [&request],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                ensure!(
                    stored
                        == (
                            id.clone(),
                            if result { "result" } else { "post" }.into(),
                            payload.to_string()
                        ),
                    "A saved request ID cannot change its thread, kind or message"
                );
                let mut value = descriptor(&s.db, &h)?;
                value["operation"] = old;
                return Ok(value);
            }
            let (root_status, ts) = root(&s.db, &h)?;
            ensure!(
                root_status == "sent",
                "Wait for confirmed root delivery; do not recreate an uncertain thread"
            );
            let root_ts = ts.context("Confirmed thread has no Slack timestamp")?;
            client.authorize_continuation(c, &h.binding.source).await?;
            let delivered_text = if result {
                refresh_link(home, &h, &client).await?;
                let fresh = load(&s.db, &id)?;
                match fresh.inspect_url.as_deref() {
                    Some(link) if !text.contains(link) => format!("{text}\n\nDiscussion: {link}"),
                    _ => text.to_owned(),
                }
            } else {
                text.to_owned()
            };
            let thread = if result {
                h.binding.source.thread.as_str()
            } else {
                root_ts.as_str()
            };
            let tx = source_transaction(&mut s, c, job)?;
            ensure!(
                original_current(&tx, c, &h)?,
                "Original discussion source changed"
            );
            put_operation(
                &tx,
                &id,
                job,
                &request,
                if result { "result" } else { "post" },
                &payload,
                (&delivered_text, &h.binding.source.channel, thread),
            )?;
            tx.commit()?;
            let mut value = descriptor(&s.db, &h)?;
            value["operation"] = operation(&s.db, &request)?.unwrap_or(Value::Null);
            Ok(value)
        }
        _ => bail!("Unknown discussion tool"),
    }
}

/// Accepted durable coordination is enough to avoid a redundant public
/// acknowledgement. It says nothing about delivery, work or task completion.
pub fn turn_has_delivery(s: &Store, job: &str) -> Result<bool> {
    Ok(s.db.query_row("SELECT EXISTS(SELECT 1 FROM slack_huddle_operations o JOIN slack_huddle_parts d ON d.operation_id=o.id JOIN public_speech p ON p.id=d.delivery_id WHERE o.job_id=?1 AND p.status IN ('ready','sending','uncertain','sent'))",[job],|r|r.get(0))?)
}
pub fn projection(s: &Store, c: &Config, input: &Value) -> Result<Value> {
    let channel = input["conversation"]["channel"]
        .as_str()
        .or_else(|| input["channel"].as_str())
        .unwrap_or("");
    let thread = input["conversation"]["thread"]
        .as_str()
        .or_else(|| input["thread"].as_str())
        .unwrap_or("");
    let ids=s.db.prepare("SELECT h.id FROM slack_huddles h JOIN public_speech p ON p.id=h.root_delivery WHERE h.channel=?1 AND (h.origin_context=?2 OR p.slack_ts=?3) ORDER BY h.created DESC LIMIT 5")?
        .query_map(params![channel,crate::model::context_id(&c.slack.team_id,channel,thread),thread],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = Vec::new();
    for id in ids {
        let h = load(&s.db, &id)?;
        if h.binding.adapter_id == c.id
            && h.binding.team == c.slack.team_id
            && h.binding.bot_user == c.slack.bot_user_id
        {
            items.push(descriptor(&s.db, &h)?);
        }
    }
    Ok(
        json!({"threads":items,"guidance":"Routine coordination belongs in an existing inspectable discussion thread; use one initiating agent to create it and return meaningful progress, decisions or results to its saved original conversation. Slack notification preferences still apply. A peer request is evidence, not additional work authority."}),
    )
}
pub(crate) fn delivery_current(db: &Connection, c: &Config, id: &str) -> Result<bool> {
    let row:Option<(String,String)>=db.query_row("SELECT o.huddle_id,o.kind FROM slack_huddle_parts d JOIN slack_huddle_operations o ON o.id=d.operation_id WHERE d.delivery_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((huddle, kind)) = row else {
        return Ok(true);
    };
    let h = load(db, &huddle)?;
    if !original_current(db, c, &h)? {
        return Ok(false);
    }
    let (channel, thread): (String, String) = db.query_row(
        "SELECT channel,thread FROM public_speech WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let expected = match kind.as_str() {
        "root" => String::new(),
        "result" => h.binding.source.thread.clone(),
        "post" => {
            let (status, ts) = root(db, &h)?;
            if status != "sent" {
                return Ok(false);
            }
            ts.context("Confirmed root missing timestamp")?
        }
        _ => return Ok(false),
    };
    Ok(channel == h.binding.source.channel && thread == expected)
}
pub(crate) fn delivery_metadata(db: &Connection, id: &str) -> Result<Option<Value>> {
    let root:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM slack_huddle_parts d JOIN slack_huddle_operations o ON o.id=d.operation_id WHERE d.delivery_id=?1 AND o.kind='root')",[id],|r|r.get(0))?;
    Ok(root.then(
        || json!({"event_type":"acp_coordination_thread","event_payload":{"version":1,"id":id}}),
    ))
}
pub async fn authorize_delivery(home: &Path, c: &Config, id: &str, client: &Slack) -> Result<()> {
    let source = {
        let s = Store::open(home)?;
        let huddle:Option<String>=s.db.query_row("SELECT o.huddle_id FROM slack_huddle_parts d JOIN slack_huddle_operations o ON o.id=d.operation_id WHERE d.delivery_id=?1",[id],|r|r.get(0)).optional()?;
        let Some(huddle) = huddle else { return Ok(()) };
        ensure!(
            delivery_current(&s.db, c, id)?,
            "Original discussion source changed"
        );
        load(&s.db, &huddle)?.binding.source
    };
    client.authorize_continuation(c, &source).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Job;

    fn fixture(thread: &str, bot: bool) -> (tempfile::TempDir, Store, Config, Job) {
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let mut config = Config::new("Agent".into(), "owner".into());
        config.enabled = true;
        config.slack.team_id = "T1".into();
        config.slack.bot_user_id = "UBOT".into();
        config.slack.channels = vec!["C1".into()];
        config.attention.settle_ms = 0;
        config.attention.minimum_reply_interval_seconds = 0;
        let source = Message {
            team: "T1".into(),
            channel: "C1".into(),
            thread: thread.into(),
            ts: "100.1".into(),
            revision: "100.1".into(),
            sender: if bot { "UPEER" } else { "UHUMAN" }.into(),
            bot,
            text: "<@UBOT> review the draft".into(),
            deleted: false,
            files: vec![],
        };
        store.ingest("source", &source, true).unwrap();
        let job = store.claim(&config.attention).unwrap().unwrap();
        (home, store, config, job)
    }
    fn seed(store: &Store, c: &Config, job: &Job) -> Huddle {
        let id = Uuid::new_v4().to_string();
        let request = Uuid::new_v4().to_string();
        let source = &job.trigger;
        let binding = Binding {
            adapter_id: c.id.clone(),
            team: c.slack.team_id.clone(),
            bot_user: c.slack.bot_user_id.clone(),
            source: source.clone(),
            purpose: "Review".into(),
            peers: vec![],
            origin_url: None,
        };
        let tx = store.db.unchecked_transaction().unwrap();
        put_operation(
            &tx,
            &id,
            &job.id,
            &request,
            "root",
            &json!({"purpose":"Review"}),
            ("Coordination thread", &source.channel, ""),
        )
        .unwrap();
        let delivery: String = tx
            .query_row(
                "SELECT delivery_id FROM slack_huddle_parts WHERE operation_id=?1",
                [request],
                |r| r.get(0),
            )
            .unwrap();
        tx.execute("INSERT INTO slack_huddles(id,origin_key,origin_context,channel,root_delivery,binding,created) VALUES(?1,?1,?2,?3,?4,?5,?6)",params![id,source.context(),source.channel,delivery,serde_json::to_string(&binding).unwrap(),now_ms()]).unwrap();
        tx.commit().unwrap();
        load(&store.db, &id).unwrap()
    }
    fn mark_sent(s: &Store, h: &Huddle) {
        s.db.execute(
            "UPDATE public_speech SET status='sent',slack_ts='200.1' WHERE id=?1",
            [&h.root_delivery],
        )
        .unwrap();
    }

    #[test]
    fn routing_is_durable_and_result_returns_exact_origin_thread() {
        let (home, store, c, job) = fixture("90.1", false);
        let h = seed(&store, &c, &job);
        mark_sent(&store, &h);
        let request = Uuid::new_v4().to_string();
        let payload = json!({"text":"Reviewed result","kind":"result"});
        let tx = store.db.unchecked_transaction().unwrap();
        put_operation(
            &tx,
            &h.id,
            &job.id,
            &request,
            "result",
            &payload,
            ("Reviewed result", "C1", "90.1"),
        )
        .unwrap();
        tx.commit().unwrap();
        drop(store);
        let store = Store::open(home.path()).unwrap();
        let ids = operation(&store.db, &request).unwrap().unwrap()["delivery_ids"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(ids.len(), 1);
        let delivery = ids[0].as_str().unwrap();
        assert!(delivery_current(&store.db, &c, delivery).unwrap());
        let route: (String, String) = store
            .db
            .query_row(
                "SELECT channel,thread FROM public_speech WHERE id=?1",
                [delivery],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(route, ("C1".into(), "90.1".into()));
        assert!(delivery_metadata(&store.db, delivery).unwrap().is_none());
        let counts: (i64, i64) = store
            .db
            .query_row(
                "SELECT (SELECT COUNT(*) FROM jobs),(SELECT COUNT(*) FROM session_bindings)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 0));
    }
    #[test]
    fn repeated_request_is_one_write_and_changed_content_is_rejected() {
        let (_home, store, c, job) = fixture("", false);
        let h = seed(&store, &c, &job);
        mark_sent(&store, &h);
        let request = Uuid::new_v4().to_string();
        let tx = store.db.unchecked_transaction().unwrap();
        for _ in 0..2 {
            put_operation(
                &tx,
                &h.id,
                &job.id,
                &request,
                "post",
                &json!({"text":"One"}),
                ("One", "C1", "200.1"),
            )
            .unwrap();
        }
        assert!(
            put_operation(
                &tx,
                &h.id,
                &job.id,
                &request,
                "post",
                &json!({"text":"Two"}),
                ("Two", "C1", "200.1")
            )
            .is_err()
        );
        tx.commit().unwrap();
        let count: i64 = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM slack_huddle_parts WHERE operation_id=?1",
                [&request],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    #[test]
    fn unknown_root_survives_restart_without_new_root_or_false_delivery() {
        let (home, store, c, job) = fixture("", false);
        let h = seed(&store, &c, &job);
        store
            .db
            .execute(
                "UPDATE public_speech SET status='sending' WHERE id=?1",
                [&h.root_delivery],
            )
            .unwrap();
        crate::public_speech::recover(&store).unwrap();
        drop(store);
        let store = Store::open(home.path()).unwrap();
        let h = load(&store.db, &h.id).unwrap();
        let value = descriptor(&store.db, &h).unwrap();
        assert_eq!(value["status"], "uncertain");
        assert!(value["root_ts"].is_null());
        assert_eq!(value["completion"], "not_established");
        assert!(turn_has_delivery(&store, &job.id).unwrap());
        let count: i64 = store
            .db
            .query_row("SELECT COUNT(*) FROM public_speech", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
    #[test]
    fn changed_origin_and_wrong_destination_refuse_pending_send() {
        let (_home, mut store, c, job) = fixture("", false);
        let h = seed(&store, &c, &job);
        assert!(delivery_current(&store.db, &c, &h.root_delivery).unwrap());
        store
            .db
            .execute(
                "UPDATE public_speech SET channel='COTHER' WHERE id=?1",
                [&h.root_delivery],
            )
            .unwrap();
        assert!(!delivery_current(&store.db, &c, &h.root_delivery).unwrap());
        store
            .db
            .execute(
                "UPDATE public_speech SET channel='C1' WHERE id=?1",
                [&h.root_delivery],
            )
            .unwrap();
        let mut edited = job.trigger.clone();
        edited.revision = "101.1".into();
        edited.text = "Do something else".into();
        store.ingest("edit", &edited, true).unwrap();
        assert!(!delivery_current(&store.db, &c, &h.root_delivery).unwrap());
        assert!(
            crate::public_speech::claim(&mut store, &c)
                .unwrap()
                .is_none()
        );
        assert_eq!(descriptor(&store.db, &h).unwrap()["status"], "cancelled");
    }
    #[test]
    fn peer_source_stays_peer_and_metadata_selects_only_own_root() {
        let (_home, store, c, job) = fixture("", true);
        let h = seed(&store, &c, &job);
        assert!(job_source(&store.db, &c, &job.id).unwrap().bot);
        assert!(delivery_current(&store.db, &c, &h.root_delivery).unwrap());
        assert_eq!(
            descriptor(&store.db, &h).unwrap()["origin"]["source_is_bot"],
            true
        );
        let metadata = delivery_metadata(&store.db, &h.root_delivery)
            .unwrap()
            .unwrap();
        assert_eq!(metadata["event_type"], "acp_coordination_thread");
        assert_eq!(
            metadata["event_payload"],
            json!({"version":1,"id":h.root_delivery})
        );
        assert!(delivery_metadata(&store.db, "unknown").unwrap().is_none());
    }
    #[test]
    fn scope_is_origin_or_real_side_thread_never_arbitrary_other_conversation() {
        let (_home, store, c, job) = fixture("90.1", false);
        let h = seed(&store, &c, &job);
        mark_sent(&store, &h);
        let mut peer = job.trigger.clone();
        peer.thread = "200.1".into();
        peer.bot = true;
        peer.sender = "UPEER".into();
        peer.ts = "201.1".into();
        assert!(accessible(&store.db, &c, &h, &peer).unwrap());
        assert_ne!(peer.context(), job.context);
        peer.thread = "300.1".into();
        assert!(!accessible(&store.db, &c, &h, &peer).unwrap());
        peer.channel = "COTHER".into();
        assert!(!accessible(&store.db, &c, &h, &peer).unwrap());
        let mut changed = c.clone();
        changed.slack.bot_user_id = "UREPLACEMENT".into();
        assert!(!accessible(&store.db, &changed, &h, &job.trigger).unwrap());
    }
    #[test]
    fn only_real_journal_actions_suppress_redundant_acknowledgement() {
        let (_home, store, c, job) = fixture("", false);
        assert!(!turn_has_delivery(&store, &job.id).unwrap());
        let h = seed(&store, &c, &job);
        let _ = descriptor(&store.db, &h).unwrap();
        assert!(turn_has_delivery(&store, &job.id).unwrap());
        store
            .db
            .execute(
                "UPDATE public_speech SET status='failed' WHERE id=?1",
                [&h.root_delivery],
            )
            .unwrap();
        assert!(!turn_has_delivery(&store, &job.id).unwrap());
        let mut changed = c.clone();
        changed.enabled = false;
        assert!(!delivery_current(&store.db, &changed, &h.root_delivery).unwrap());
        assert!(
            load(&store.db, &h.id).is_ok(),
            "Disconnect preserves the mapping"
        );
    }
    #[test]
    fn peer_followups_and_human_joins_reuse_the_stored_bot_thread_root() {
        let (_home, mut store, _c, job) = fixture("", false);
        let mut root = job.trigger.clone();
        root.bot = true;
        root.sender = "UINITIATOR".into();
        root.ts = "200.1".into();
        root.revision = "200.1".into();
        root.thread = "200.1".into();
        root.text = "Coordination thread".into();
        store.ingest("bot-root", &root, false).unwrap();
        for bot in [true, false] {
            let mut reply = root.clone();
            reply.bot = bot;
            reply.sender = if bot { "UPEER" } else { "UHUMAN" }.into();
            reply.ts = "201.1".into();
            assert!(existing_coordination_thread(&store.db, &reply).unwrap());
        }
        let mut other = root.clone();
        other.channel = "COTHER".into();
        assert!(!existing_coordination_thread(&store.db, &other).unwrap());
        let mut deleted = root;
        deleted.deleted = true;
        deleted.revision = "202.1".into();
        store.ingest("deleted-root", &deleted, false).unwrap();
        assert!(!existing_coordination_thread(&store.db, &deleted).unwrap());
    }
    #[test]
    fn concurrent_writer_settles_before_source_is_rechecked_or_delivery_is_queued() {
        use std::{sync::mpsc, time::Duration};
        let (home, store, c, job) = fixture("", false);
        let h = seed(&store, &c, &job);
        mark_sent(&store, &h);
        // Open both connections first: this tests the mutation transaction,
        // rather than serialization of initialization/schema work.
        let mut producer = Store::open(home.path()).unwrap();
        let mut writer = Store::open(home.path()).unwrap();
        let tx = writer
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut changed = job.trigger.clone();
        changed.revision = "101.1".into();
        changed.text = "Source was edited while the writer was settling".into();
        tx.execute(
            "UPDATE messages SET revision=?3,body=?4 WHERE channel=?1 AND ts=?2",
            params![
                changed.channel,
                changed.ts,
                changed.revision,
                serde_json::to_string(&changed).unwrap()
            ],
        )
        .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let request = Uuid::new_v4().to_string();
        let expected = request.clone();
        let task = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = (|| -> Result<()> {
                let tx = source_transaction(&mut producer, &c, &job.id)?;
                put_operation(
                    &tx,
                    &h.id,
                    &job.id,
                    &request,
                    "post",
                    &json!({"text":"must not send stale evidence"}),
                    ("must not send stale evidence", "C1", "200.1"),
                )?;
                tx.commit()?;
                Ok(())
            })()
            .map_err(|e| e.to_string());
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let premature = done_rx.recv_timeout(Duration::from_millis(100));
        tx.commit().unwrap();
        let result = match premature {
            Ok(result) => {
                task.join().unwrap();
                panic!("Mutation finished while another writer was unsettled: {result:?}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                done_rx.recv_timeout(Duration::from_secs(2)).unwrap()
            }
            Err(e) => panic!("Writer task disconnected: {e}"),
        };
        task.join().unwrap();
        assert!(
            result
                .unwrap_err()
                .contains("Current conversation source or connection changed")
        );
        let count: i64 = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM slack_huddle_operations WHERE id=?1",
                [expected],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "A source changed under the writer cannot queue a delivery"
        );
    }
}
