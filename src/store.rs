use crate::{
    config::{Attention, Config},
    model::{Decision, Message, now_ms},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

pub struct Store {
    pub db: Connection,
}
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub context: String,
    pub trigger: Message,
    pub version: i64,
    pub reconsidered: bool,
}
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub id: String,
    pub channel: String,
    pub thread: String,
    pub text: String,
    pub media: Vec<crate::media::OutputFile>,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self> {
        let db = Connection::open(home.join("state.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, received INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS messages(channel TEXT, ts TEXT, context TEXT NOT NULL, revision TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(channel,ts));
            CREATE TABLE IF NOT EXISTS contexts(id TEXT PRIMARY KEY, version INTEGER NOT NULL DEFAULT 0, bot_replies INTEGER NOT NULL DEFAULT 0, last_human TEXT NOT NULL DEFAULT '');
            CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, context TEXT NOT NULL, trigger TEXT NOT NULL, status TEXT NOT NULL, created INTEGER NOT NULL, updated INTEGER NOT NULL, version INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '');
            CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status,updated);
            CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY, job_id TEXT NOT NULL UNIQUE, context TEXT NOT NULL, channel TEXT NOT NULL, thread TEXT NOT NULL, text TEXT NOT NULL, status TEXT NOT NULL, created INTEGER NOT NULL, available INTEGER NOT NULL, slack_ts TEXT, note TEXT NOT NULL DEFAULT '');
            CREATE TABLE IF NOT EXISTS audit(seq INTEGER PRIMARY KEY AUTOINCREMENT, time INTEGER NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS session_bindings(context TEXT PRIMARY KEY, endpoint TEXT NOT NULL, session_id TEXT NOT NULL, resumable INTEGER NOT NULL, channel TEXT NOT NULL, thread TEXT NOT NULL, updated INTEGER NOT NULL);
        ")?;
        let store = Self { db };
        let has_media: bool = store
            .db
            .prepare("PRAGMA table_info(outbox)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|v| v.is_ok_and(|v| v == "media"));
        if !has_media {
            store.db.execute(
                "ALTER TABLE outbox ADD COLUMN media TEXT NOT NULL DEFAULT '[]'",
                [],
            )?;
        }
        crate::permissions::initialize(&store)?;
        Ok(store)
    }
    pub fn recover(&self) -> Result<()> {
        crate::permissions::cancel_all(self, "Connector restarted")?;
        self.db.execute("UPDATE jobs SET status='interrupted', note='Process stopped during a model turn; not replayed automatically' WHERE status='running'",[])?;
        self.db.execute("UPDATE outbox SET status='uncertain', note='Process stopped during delivery; inspect Slack before resending' WHERE status='sending'",[])?;
        self.audit(
            "process_started",
            json!({"version":env!("CARGO_PKG_VERSION"),"pid":std::process::id()}),
        )
    }
    /// A narrower connection scope also applies to work queued before a pause
    /// or reconfiguration. Previously attempted sends remain uncertain.
    pub fn apply_scope(&self, c: &Config) -> Result<()> {
        let mut stmt = self
            .db
            .prepare("SELECT id,trigger FROM jobs WHERE status IN ('pending','speak')")?;
        let jobs = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut cancelled = 0;
        for (id, body) in jobs {
            let m: Message = serde_json::from_str(&body)?;
            if !c.allows(&m.channel, "") || !c.should_wake(&m.channel, &m.text) {
                cancelled+=self.db.execute("UPDATE jobs SET status='superseded',note='Conversation no longer in the configured scope' WHERE id=?1 AND status='pending'",[&id])?;
                cancelled+=self.db.execute("UPDATE outbox SET status='cancelled',note='Conversation no longer in the configured scope' WHERE job_id=?1 AND status='ready'",[&id])?;
            }
        }
        if cancelled > 0 {
            self.audit(
                "queued_work_cancelled",
                json!({"count":cancelled,"reason":"The configured conversation scope changed"}),
            )?;
        }
        Ok(())
    }
    pub fn bind_identity(&self, c: &Config) -> Result<()> {
        let id = format!("{}:{}:{}", c.id, c.slack.team_id, c.slack.bot_user_id);
        let old: Option<String> = self
            .db
            .query_row("SELECT value FROM settings WHERE key='identity'", [], |r| {
                r.get(0)
            })
            .optional()?;
        anyhow::ensure!(
            old.as_ref().is_none_or(|x| x == &id),
            "This state belongs to a different identity; install a fresh folder instead"
        );
        self.db
            .execute("INSERT OR IGNORE INTO settings VALUES('identity',?1)", [id])?;
        Ok(())
    }
    pub fn audit(&self, kind: &str, data: impl Serialize) -> Result<()> {
        self.db.execute(
            "INSERT INTO audit(time,kind,data) VALUES(?1,?2,?3)",
            params![now_ms(), kind, serde_json::to_string(&data)?],
        )?;
        self.db.execute(
            "DELETE FROM audit WHERE seq <= (SELECT COALESCE(MAX(seq),0)-5000 FROM audit)",
            [],
        )?;
        Ok(())
    }
    pub fn session(&self, context: &str, endpoint: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT session_id FROM session_bindings WHERE context=?1 AND endpoint=?2 AND resumable=1",params![context,endpoint],|r|r.get(0)).optional()?)
    }
    pub fn bind_session(
        &self,
        context: &str,
        endpoint: &str,
        sid: &str,
        resumable: bool,
        conversation: &Value,
    ) -> Result<()> {
        self.db.execute("INSERT INTO session_bindings VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(context) DO UPDATE SET endpoint=excluded.endpoint,session_id=excluded.session_id,resumable=excluded.resumable,updated=excluded.updated",params![context,endpoint,sid,resumable,conversation["channel"].as_str().unwrap_or(""),conversation["thread"].as_str().unwrap_or(""),now_ms()])?;
        Ok(())
    }
    pub fn activity(&self) -> Result<Value> {
        let mut stmt=self.db.prepare("SELECT seq,time,kind,data FROM audit WHERE kind!='message' ORDER BY seq DESC LIMIT 200")?;
        let items=stmt.query_map([],|r|{ let raw:String=r.get(3)?; Ok(json!({"id":r.get::<_,i64>(0)?,"time":r.get::<_,i64>(1)?,"kind":r.get::<_,String>(2)?,"data":serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null)})) })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"items":items}))
    }
    pub fn conversations(&self) -> Result<Value> {
        let mut stmt=self.db.prepare("SELECT c.id,COALESCE(b.channel,''),COALESCE(b.thread,''),b.session_id,b.resumable,b.updated,(SELECT COUNT(*) FROM jobs j WHERE j.context=c.id),(SELECT status FROM jobs j WHERE j.context=c.id ORDER BY updated DESC LIMIT 1) FROM contexts c LEFT JOIN session_bindings b ON c.id=b.context ORDER BY b.updated DESC LIMIT 200")?;
        let items=stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"channel":r.get::<_,String>(1)?,"thread":r.get::<_,String>(2)?,"session_id":r.get::<_,Option<String>>(3)?,"resumable":r.get::<_,Option<bool>>(4)?,"updated_at":r.get::<_,Option<i64>>(5)?,"turns":r.get::<_,i64>(6)?,"last_outcome":r.get::<_,Option<String>>(7)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"items":items}))
    }
    pub fn assert_endpoint_change_allowed(&self, old: &Config, new: &Config) -> Result<()> {
        let bound: i64 = self.db.query_row(
            "SELECT (SELECT COUNT(*) FROM messages)+(SELECT COUNT(*) FROM session_bindings)",
            [],
            |r| r.get(0),
        )?;
        if bound > 0 {
            ensure!(
                old.slack.team_id == new.slack.team_id
                    && old.slack.app_id == new.slack.app_id
                    && old.slack.bot_user_id == new.slack.bot_user_id
                    && old.slack.bot_id == new.slack.bot_id,
                "This connection has conversation history; use a new adapter for a different Slack identity"
            );
            ensure!(
                old.acp.command == new.acp.command
                    && old.acp.args == new.acp.args
                    && old.acp.cwd == new.acp.cwd
                    && old.acp.env == new.acp.env
                    && old.acp.secret_env == new.acp.secret_env,
                "This connection has conversation history; use a new adapter to connect a different agent endpoint"
            );
        }
        Ok(())
    }
    pub fn message(&self, channel: &str, ts: &str) -> Result<Option<Message>> {
        let body: Option<String> = self
            .db
            .query_row(
                "SELECT body FROM messages WHERE channel=?1 AND ts=?2",
                params![channel, ts],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    /// Commit before acknowledging Slack. Duplicate events and duplicate delivery
    /// via app_mention + message are both harmless.
    pub fn ingest(&mut self, event_id: &str, m: &Message, wake: bool) -> Result<bool> {
        let tx = self.db.transaction()?;
        if tx.execute(
            "INSERT OR IGNORE INTO events VALUES(?1,?2)",
            params![event_id, now_ms()],
        )? == 0
        {
            return Ok(false);
        }
        let old: Option<String> = tx
            .query_row(
                "SELECT revision FROM messages WHERE channel=?1 AND ts=?2",
                params![m.channel, m.ts],
                |r| r.get(0),
            )
            .optional()?;
        if old.as_ref().is_some_and(|v| v >= &m.revision) {
            tx.commit()?;
            return Ok(false);
        }
        let ctx = m.context();
        tx.execute("INSERT INTO messages VALUES(?1,?2,?3,?4,?5) ON CONFLICT(channel,ts) DO UPDATE SET context=excluded.context,revision=excluded.revision,body=excluded.body",params![m.channel,m.ts,ctx,m.revision,serde_json::to_string(m)?])?;
        tx.execute("INSERT INTO contexts(id,version,bot_replies,last_human) VALUES(?1,1,0,CASE WHEN ?2 THEN '' ELSE ?3 END) ON CONFLICT(id) DO UPDATE SET version=version+1,bot_replies=CASE WHEN NOT ?2 AND ?3>last_human THEN 0 ELSE bot_replies END,last_human=CASE WHEN NOT ?2 AND ?3>last_human THEN ?3 ELSE last_human END",params![ctx,m.bot||m.deleted,m.ts])?;
        if old.is_some() {
            tx.execute("UPDATE outbox SET status='cancelled',note='Source edited or deleted before delivery' WHERE status='ready' AND job_id IN (SELECT id FROM jobs WHERE json_extract(trigger,'$.channel')=?1 AND json_extract(trigger,'$.ts')=?2)",params![m.channel,m.ts])?;
        }
        // A root edit also changes the source snapshot of its child thread.
        if old.is_some() && m.thread.is_empty() {
            let thread_ctx = crate::model::context_id(&m.team, &m.channel, &m.ts);
            tx.execute(
                "UPDATE contexts SET version=version+1 WHERE id=?1",
                [thread_ctx],
            )?;
        }
        if wake && !m.deleted {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM jobs WHERE context=?1 AND status='pending'",
                    [&ctx],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                tx.execute(
                    "UPDATE jobs SET trigger=?1,updated=?2 WHERE id=?3",
                    params![serde_json::to_string(m)?, now_ms(), id],
                )?;
            } else {
                tx.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES(?1,?2,?3,'pending',?4,?4)",params![Uuid::new_v4().to_string(),ctx,serde_json::to_string(m)?,now_ms()])?;
            }
        }
        if m.deleted {
            tx.execute("UPDATE jobs SET status='superseded',note='Source message deleted' WHERE context=?1 AND status='pending' AND json_extract(trigger,'$.ts')=?2",params![ctx,m.ts])?;
        }
        tx.execute("INSERT INTO audit(time,kind,data) VALUES(?1,'message',?2)",params![now_ms(),json!({"context":ctx,"channel":m.channel,"ts":m.ts,"sender":m.sender,"deleted":m.deleted,"wake":wake}).to_string()])?;
        tx.commit()?;
        Ok(true)
    }
    pub fn history(&self, m: &Message) -> Result<Vec<Message>> {
        let mut stmt=self.db.prepare("SELECT body FROM messages WHERE channel=?1 AND (context=?2 OR ts=?3) ORDER BY ts DESC LIMIT 80")?;
        let rows = stmt.query_map(params![m.channel, m.context(), m.thread], |r| {
            r.get::<_, String>(0)
        })?;
        let mut result = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let msg: Message = serde_json::from_str(&row?)?;
            if msg.deleted {
                continue;
            }
            bytes += msg.text.len();
            if bytes > 60000 {
                break;
            }
            result.push(msg);
        }
        result.reverse();
        Ok(result)
    }
    /// Provider history includes generated replies even when Slack never received
    /// them. Supply bounded, conversation-local delivery facts on every turn;
    /// neither a private draft nor an attempted send establishes publication.
    pub fn response_delivery(&self, context: &str, current_job: &str) -> Result<Value> {
        let mut stmt = self.db.prepare(
            "SELECT j.id,json_extract(j.trigger,'$.ts'),j.status,o.status,o.slack_ts
             FROM jobs j LEFT JOIN outbox o ON o.job_id=j.id
             WHERE j.context=?1 AND j.id!=?2 AND j.status NOT IN ('pending','running')
             ORDER BY j.updated DESC,j.rowid DESC LIMIT 12",
        )?;
        let items = stmt
            .query_map(params![context, current_job], |row| {
                let job_status: String = row.get(2)?;
                let outgoing: Option<String> = row.get(3)?;
                let status = match outgoing.as_deref() {
                    Some("sent") => "delivered",
                    Some("ready" | "sending") => "pending",
                    Some("uncertain") => "uncertain",
                    Some("cancelled" | "failed") => "not_delivered",
                    None if job_status == "pass" => "no_response",
                    None if matches!(
                        job_status.as_str(),
                        "superseded" | "limited" | "failed" | "interrupted"
                    ) =>
                    {
                        "not_delivered"
                    }
                    _ => "unknown",
                };
                Ok(json!({"job_id":row.get::<_,String>(0)?,
                "trigger_message_id":row.get::<_,String>(1)?,"status":status,
                "slack_message_id":row.get::<_,Option<String>>(4)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"scope":"this_agent_in_this_conversation","bounded":true,
            "observed_at":now_ms(),"items":items}),
        )
    }
    pub fn prepare(&self, mut job: Job) -> Result<Job> {
        // Backfill and messages received while fetching are folded into one
        // snapshot before the model starts. Later events get their own turn.
        let pending: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT trigger,note FROM jobs WHERE context=?1 AND status='pending'",
                [&job.context],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((p, note)) = pending {
            job.trigger = serde_json::from_str(&p)?;
            job.reconsidered |= note.starts_with("Reconsider");
            self.db.execute("UPDATE jobs SET status='superseded',note='Coalesced into the current turn' WHERE context=?1 AND status='pending'",[&job.context])?;
        }
        job.version = self.db.query_row(
            "SELECT version FROM contexts WHERE id=?1",
            [&job.context],
            |r| r.get(0),
        )?;
        self.db.execute(
            "UPDATE jobs SET trigger=?1,version=?2 WHERE id=?3",
            params![serde_json::to_string(&job.trigger)?, job.version, job.id],
        )?;
        Ok(job)
    }
    pub fn claim(&mut self, a: &Attention) -> Result<Option<Job>> {
        let now = now_ms();
        let tx = self.db.transaction()?;
        let candidate:Option<(String,String,String,String)>=tx.query_row("SELECT id,context,trigger,note FROM jobs j WHERE status='pending' AND updated<=?1 AND NOT EXISTS(SELECT 1 FROM jobs x WHERE x.context=j.context AND x.status='running') ORDER BY created LIMIT 1",[now-a.settle_ms as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let Some((id, context, trigger, note)) = candidate else {
            return Ok(None);
        };
        let decisions:i64=tx.query_row("SELECT COUNT(*) FROM jobs WHERE status NOT IN ('pending','superseded','budget') AND updated>?1",[now-3600000],|r|r.get(0))?;
        if decisions >= a.max_decisions_per_agent_per_hour as i64 {
            tx.execute("UPDATE jobs SET status='budget',note='Hourly model decision budget reached' WHERE id=?1",[id])?;
            tx.commit()?;
            return Ok(None);
        }
        let version = tx.query_row(
            "SELECT version FROM contexts WHERE id=?1",
            [&context],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE jobs SET status='running',version=?1,updated=?2 WHERE id=?3",
            params![version, now, id],
        )?;
        tx.commit()?;
        Ok(Some(Job {
            id,
            context,
            trigger: serde_json::from_str(&trigger)?,
            version,
            reconsidered: note.starts_with("Reconsider"),
        }))
    }
    pub fn finish(&mut self, job: &Job, result: Result<Decision>, a: &Attention) -> Result<()> {
        let d = match result {
            Ok(d) => d,
            Err(error) => {
                let auth = crate::acp_auth::authentication_required(&error);
                let note = if auth {
                    error.to_string()
                } else {
                    "Driver failed; see local diagnostic category in audit".into()
                };
                self.db.execute(
                    "UPDATE jobs SET status='failed',updated=?1,note=?3 WHERE id=?2",
                    params![now_ms(), job.id, note],
                )?;
                self.audit(
                    if auth {
                        "agent_sign_in_required"
                    } else {
                        "driver_failed"
                    },
                    json!({"job":job.id,"context":job.context,"reason":note}),
                )?;
                return Ok(());
            }
        };
        let version: i64 = self.db.query_row(
            "SELECT version FROM contexts WHERE id=?1",
            [&job.context],
            |r| r.get(0),
        )?;
        if version != job.version {
            self.db.execute("UPDATE jobs SET status='superseded',updated=?1,note='Conversation changed during this turn; contribution withheld' WHERE id=?2",params![now_ms(),job.id])?;
            self.audit("stale_contribution_withheld", json!({"job":job.id}))?;
            if let Some(current) = self.message(&job.trigger.channel, &job.trigger.ts)?
                && !current.deleted
            {
                schedule_reconsider(&self.db, &current)?;
            }
            return Ok(());
        }
        let mut state = d.decision.clone();
        let mut note = d.reason.clone();
        if d.decision == "speak"
            && let Some(why) = self.reply_limit(&job.context, &job.trigger, a)?
        {
            state = "limited".into();
            note = why;
        }
        let tx = self.db.transaction()?;
        if state == "speak" {
            let id = Uuid::new_v4().to_string();
            tx.execute("INSERT INTO outbox(id,job_id,context,channel,thread,text,status,created,available,media) VALUES(?1,?2,?3,?4,?5,?6,'ready',?7,?7,?8)",params![id,job.id,job.context,job.trigger.channel,job.trigger.thread,d.text,now_ms(),serde_json::to_string(&d.media)?])?;
            tx.execute(
                "UPDATE contexts SET bot_replies=bot_replies+1 WHERE id=?1",
                [&job.context],
            )?;
        }
        tx.execute(
            "UPDATE jobs SET status=?1,updated=?2,note=?3 WHERE id=?4",
            params![state, now_ms(), note, job.id],
        )?;
        tx.execute(
            "INSERT INTO audit(time,kind,data) VALUES(?1,'decision',?2)",
            params![
                now_ms(),
                json!({"job":job.id,"context":job.context,"decision":state,"reason":note})
                    .to_string()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn reply_limit(&self, ctx: &str, m: &Message, a: &Attention) -> Result<Option<String>> {
        let (total,room,last):(i64,i64,i64)=self.db.query_row("SELECT COUNT(*),COALESCE(SUM(CASE WHEN context=?1 THEN 1 ELSE 0 END),0),COALESCE(MAX(CASE WHEN context=?1 THEN created ELSE 0 END),0) FROM outbox WHERE created>?2 AND status!='failed'",params![ctx,now_ms()-3600000],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let burst: i64 =
            self.db
                .query_row("SELECT bot_replies FROM contexts WHERE id=?1", [ctx], |r| {
                    r.get(0)
                })?;
        let why = if total >= a.max_replies_per_agent_per_hour as i64 {
            "Agent hourly contribution limit"
        } else if room >= a.max_replies_per_context_per_hour as i64 {
            "Conversation hourly contribution limit"
        } else if m.bot && burst >= a.max_bot_replies_without_human as i64 {
            "Bot exchange limit; waiting for a human"
        } else if now_ms() - last < (a.minimum_reply_interval_seconds * 1000) as i64 {
            "Contribution cooldown"
        } else {
            return Ok(None);
        };
        Ok(Some(why.into()))
    }
    pub fn claim_outgoing(&mut self, c: &Config) -> Result<Option<Outgoing>> {
        let tx = self.db.transaction()?;
        let candidate:Option<(Outgoing,String,i64,i64)>=tx.query_row("SELECT o.id,o.channel,o.thread,o.text,j.trigger,j.version,c.version,o.media FROM outbox o JOIN jobs j ON o.job_id=j.id JOIN contexts c ON c.id=o.context WHERE o.status='ready' AND o.available<=?1 ORDER BY o.created LIMIT 1",[now_ms()],|r| {
            let raw:String=r.get(7)?;
            let media=serde_json::from_str(&raw).map_err(|e|rusqlite::Error::FromSqlConversionFailure(7,rusqlite::types::Type::Text,Box::new(e)))?;
            Ok((Outgoing{id:r.get(0)?,channel:r.get(1)?,thread:r.get(2)?,text:r.get(3)?,media},r.get(4)?,r.get(5)?,r.get(6)?))
        }).optional()?;
        let Some((out, trigger, original_version, current_version)) = candidate else {
            return Ok(None);
        };
        let trigger: Message = serde_json::from_str(&trigger)?;
        let source: Option<String> = tx
            .query_row(
                "SELECT body FROM messages WHERE channel=?1 AND ts=?2",
                params![trigger.channel, trigger.ts],
                |r| r.get(0),
            )
            .optional()?;
        let source = source
            .map(|s| serde_json::from_str::<Message>(&s))
            .transpose()?;
        let allowed = c.enabled
            && c.allows(&out.channel, "")
            && source
                .as_ref()
                .is_some_and(|s| !s.deleted && c.should_wake(&s.channel, &s.text));
        let unchanged = original_version == current_version
            && source
                .as_ref()
                .is_some_and(|s| s.revision == trigger.revision);
        if !allowed || !unchanged {
            tx.execute("UPDATE outbox SET status='cancelled',note='Conversation or connection policy changed before delivery' WHERE id=?1 AND status='ready'",[&out.id])?;
            if allowed && let Some(m) = source {
                schedule_reconsider(&tx, &m)?;
            }
            tx.execute("INSERT INTO audit(time,kind,data) VALUES(?1,'queued_reply_withheld',?2)",params![now_ms(),json!({"id":out.id,"reason":if allowed{"Conversation changed before delivery"}else{"Conversation is outside the configured scope"}}).to_string()])?;
            tx.commit()?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE outbox SET status='sending',note='' WHERE id=?1 AND status='ready'",
            [&out.id],
        )?;
        tx.commit()?;
        Ok(Some(out))
    }
    pub fn delivery(
        &self,
        id: &str,
        status: &str,
        ts: Option<&str>,
        note: &str,
        delay_ms: i64,
    ) -> Result<()> {
        self.db.execute("UPDATE outbox SET status=?1,slack_ts=COALESCE(?2,slack_ts),note=?3,available=?4 WHERE id=?5",params![status,ts,note,now_ms()+delay_ms,id])?;
        self.audit(
            "delivery",
            json!({"id":id,"status":status,"ts":ts,"note":note}),
        )
    }
    pub fn echo(&self, id: &str, ts: &str) -> Result<()> {
        // A matching Slack echo resolves a lost HTTP reply without sending again.
        self.db.execute("UPDATE outbox SET status='sent',slack_ts=?1,note='Confirmed by Slack event' WHERE id=?2 AND status IN ('sending','uncertain') AND media='[]'",params![ts,id])?;
        Ok(())
    }
    pub fn status(&self) -> Result<Value> {
        let mut jobs = self
            .db
            .prepare("SELECT status,COUNT(*) FROM jobs GROUP BY status")?;
        let job_counts = jobs
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<std::collections::BTreeMap<_, _>>>()?;
        let mut outgoing = self
            .db
            .prepare("SELECT status,COUNT(*) FROM outbox GROUP BY status")?;
        let counts = outgoing
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<std::collections::BTreeMap<_, _>>>()?;
        let contexts: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM contexts", [], |r| r.get(0))?;
        let connection: Option<String> = self
            .db
            .query_row(
                "SELECT data FROM audit WHERE kind='connection' ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(
            json!({"contexts":contexts,"jobs":job_counts,"outbox":counts,"slack_connection":connection.and_then(|s|serde_json::from_str::<Value>(&s).ok())}),
        )
    }
}

fn schedule_reconsider(db: &Connection, m: &Message) -> Result<()> {
    // A newer message normally queued the next job before the old draft was
    // withheld. Preserve its newer trigger and attach the delivery notice to it.
    db.execute("UPDATE jobs SET note='Reconsider the latest conversation; an earlier response was not delivered. Check work already performed before repeating actions.' WHERE context=?1 AND status='pending'",[m.context()])?;
    db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated,note) SELECT ?1,?2,?3,'pending',?4,?4,'Reconsider the latest conversation; an earlier turn may already have performed work. Do not repeat actions without checking the existing result.' WHERE NOT EXISTS(SELECT 1 FROM jobs WHERE context=?2 AND status IN ('pending','running'))",params![Uuid::new_v4().to_string(),m.context(),serde_json::to_string(m)?,now_ms()])?;
    Ok(())
}
