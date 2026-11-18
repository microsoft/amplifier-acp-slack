//! A private message from the agent to its configured owner. The connector
//! fixes the recipient and the destination; delivery stays in the ordinary
//! public-speech journal, so an uncertain send is reconciled, never repeated.
//!
//! The tool is offered only for a response to a person in a group conversation,
//! never to another agent's message, and a call is accepted only while the
//! owner is a member of that conversation. The
//! message therefore tells the owner nothing they could not already read at its
//! source, and it carries a link back to that source.
//!
//! The journal can hold a message for pacing or a Slack rate limit. What the
//! message was accepted for is saved with it, and both that record and the
//! owner's membership of the source are checked again before every send.
use crate::{
    config::Config,
    model::{Message, now_ms},
    slack::{Presence, Slack},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

pub const TOOL: &str = "slack_owner_message";
const MAX_CHARS: usize = 8_000;
const MAX_PER_RESPONSE: i64 = 3;
const PREFIX: &str = "owner-message:";
/// How long a message may wait for Slack to confirm its audience, counted from
/// its first claim for sending. One response's messages are sent in order, so
/// this also bounds how long an unconfirmed one can hold back what follows it.
const UNCONFIRMED_FOR_MS: i64 = 120_000;
const RECHECK_MS: i64 = 15_000;
// Receipt notes, written for the agent that reads the receipt.
const CHANGED: &str = "Not sent: the source message, the configured owner or this connection changed after the message was accepted";
const OWNER_LEFT: &str = "Not sent: your owner is no longer a member of the source conversation";
const UNCONFIRMED: &str = "Not sent: Slack could not confirm who can read the source conversation";
const WAITING: &str = "Not sent yet: Slack could not confirm who can read the source conversation";

/// What a saved message was accepted for: this installation, this owner, the
/// owner's direct conversation and the message that asked.
#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    adapter_id: String,
    team: String,
    bot_user: String,
    owner: String,
    direct: String,
    source: Message,
}

impl Binding {
    fn new(c: &Config, direct: &str, source: &Message) -> Self {
        Self {
            adapter_id: c.id.clone(),
            team: c.slack.team_id.clone(),
            bot_user: c.slack.bot_user_id.clone(),
            owner: c.context.owner_slack_user_id.clone(),
            direct: direct.to_owned(),
            source: source.clone(),
        }
    }
}

pub fn initialize(s: &Store) -> Result<()> {
    s.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS slack_owner_messages(
        job_id TEXT NOT NULL,message_id TEXT NOT NULL,binding TEXT NOT NULL,
        PRIMARY KEY(job_id,message_id));",
    )?;
    Ok(())
}

pub fn tool() -> Value {
    json!({"name":TOOL,
        "description":"Send a private message to this agent's configured owner, in the owner's direct conversation with you. Use it when the owner asks to be told privately, or when something here needs the owner's decision and should not be said in this conversation. The connector fixes the recipient and adds a link to this conversation; it cannot message anyone else. It works only while the owner is a member of this conversation. At most three per response. A pending or uncertain receipt is not delivery: call again with the same request_id to check it, never to send it again.",
        "inputSchema":{"type":"object","properties":{
            "request_id":{"type":"string","format":"uuid","description":"A new UUID for a new message; the same one to check a message already asked for"},
            "text":{"type":"string","minLength":1,"maxLength":MAX_CHARS,"description":"The message, in standard Markdown"}},
            "required":["request_id","text"],"additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}})
}

/// Whether the tool belongs in this response's tool list: a response to a
/// person in a group conversation, for a connection with an owner to reach.
/// Membership of the owner is checked on each call, where a refusal can say why.
pub fn offered(c: &Config, group_origin: bool, from_person: bool) -> bool {
    group_origin && from_person && !c.context.owner_slack_user_id.is_empty() && c.slack.allow_dms
}

fn refusal(reason: &str) -> Value {
    json!({"accepted":false,"reason":reason})
}

struct Request {
    id: String,
    text: String,
}

fn parse(args: &Value) -> std::result::Result<Request, &'static str> {
    let Some(object) = args.as_object() else {
        return Err("The arguments must be an object with request_id and text.");
    };
    if object
        .keys()
        .any(|key| !["request_id", "text"].contains(&key.as_str()))
    {
        return Err("Only request_id and text are accepted; the recipient is fixed.");
    }
    let Some(id) = args["request_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
    else {
        return Err("request_id must be a UUID.");
    };
    let Some(text) = args["text"].as_str() else {
        return Err("text must be the message to send.");
    };
    if text.trim().is_empty() {
        return Err("text must not be empty.");
    }
    if text.chars().count() > MAX_CHARS {
        return Err(
            "The message is over 8,000 characters. Send the owner the point and the decision needed; the link added to it leads to the rest.",
        );
    }
    Ok(Request {
        id: id.to_string(),
        text: text.to_owned(),
    })
}

/// The saved state of one requested message, or `None` when nothing was saved.
fn receipt(db: &Connection, job: &str, request: &str) -> Result<Option<Value>> {
    let message = format!("{PREFIX}{request}");
    let receipts = db
        .prepare(
            "SELECT p.id,p.status,p.slack_ts,p.note,p.channel FROM public_speech p JOIN slack_owner_messages b ON b.job_id=p.job_id AND b.message_id=p.message_id WHERE p.job_id=?1 AND p.message_id=?2 ORDER BY p.part",
        )?
        .query_map(params![job, message], |r| {
            Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,
                "slack_message_id":r.get::<_,Option<String>>(2)?,"note":r.get::<_,String>(3)?,
                "conversation_id":r.get::<_,String>(4)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if receipts.is_empty() {
        return Ok(None);
    }
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
    let delivery = match status {
        "sent" => "Delivered to your owner's direct conversation.",
        "failed" | "cancelled" => {
            "Not delivered, and it will not be sent later: your owner has not been told. The receipt notes say why."
        }
        _ => {
            "A pending or uncertain receipt is not delivery. Check it with this request_id; do not send it again under another."
        }
    };
    Ok(Some(
        json!({"accepted":true,"request_id":request,"recipient":"owner",
        "conversation_id":receipts[0]["conversation_id"],"status":status,"receipts":receipts,
        "delivery":delivery}),
    ))
}

fn sent_for(db: &Connection, job: &str) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COUNT(*) FROM slack_owner_messages WHERE job_id=?1",
        [job],
        |r| r.get(0),
    )?)
}

fn binding(db: &Connection, delivery: &str) -> Result<Option<Binding>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT b.binding FROM public_speech p JOIN slack_owner_messages b ON b.job_id=p.job_id AND b.message_id=p.message_id WHERE p.id=?1",
            [delivery],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?)
}

/// Whether everything a message was accepted for is still so in this
/// installation's own record. Slack is asked separately, in `authorize_delivery`.
fn holds(db: &Connection, c: &Config, delivery: &str, bound: &Binding) -> Result<bool> {
    let (channel, thread): (String, String) = db.query_row(
        "SELECT channel,thread FROM public_speech WHERE id=?1",
        [delivery],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(bound.adapter_id == c.id
        && bound.team == c.slack.team_id
        && bound.bot_user == c.slack.bot_user_id
        && !bound.owner.is_empty()
        && bound.owner == c.context.owner_slack_user_id
        && c.allows(&bound.direct, "im")
        && channel == bound.direct
        && thread.is_empty()
        && !bound.source.bot
        && crate::huddles::current_source(db, c, &bound.source)?)
}

/// Cancel every unsent part of the message `delivery` belongs to, and only
/// that message: the response's other speech does not depend on it.
fn withdraw(db: &Connection, delivery: &str, note: &str) -> Result<()> {
    let (job, message): (String, String) = db.query_row(
        "SELECT job_id,message_id FROM public_speech WHERE id=?1",
        [delivery],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    db.execute(
        "UPDATE public_speech SET status='cancelled',note=?3 WHERE job_id=?1 AND message_id=?2 AND status IN ('ready','sending')",
        params![job, message, note],
    )?;
    db.execute(
        "INSERT INTO audit(time,kind,data) VALUES(?1,'owner_message_withheld',?2)",
        params![
            now_ms(),
            json!({"job_id":job,"id":delivery,"reason":note}).to_string()
        ],
    )?;
    Ok(())
}

/// Called while the journal chooses its next delivery. `false` means it was an
/// owner message whose record no longer holds; it has been withdrawn.
pub(crate) fn claimable(db: &Connection, c: &Config, delivery: &str) -> Result<bool> {
    let Some(bound) = binding(db, delivery)? else {
        return Ok(true);
    };
    if holds(db, c, delivery, &bound)? {
        return Ok(true);
    }
    withdraw(db, delivery, CHANGED)?;
    Ok(false)
}

/// Called with a claimed delivery, immediately before each send, so also before
/// the retry of a rate-limited one. For an owner message it asks Slack again
/// whether the source stands and the owner can read it; `false` means the
/// delivery was settled here and nothing may be sent now. It never opens the
/// direct conversation again: the destination is the one saved with the message.
pub(crate) async fn authorize_delivery(
    home: &Path,
    c: &Config,
    delivery: &str,
    client: &Slack,
) -> Result<bool> {
    let (bound, first) = {
        let s = Store::open(home)?;
        let Some(bound) = binding(&s.db, delivery)? else {
            return Ok(true);
        };
        if !holds(&s.db, c, delivery, &bound)? {
            withdraw(&s.db, delivery, CHANGED)?;
            return Ok(false);
        }
        let first: i64 = s.db.query_row(
            "SELECT attempted FROM public_speech WHERE id=?1",
            [delivery],
            |r| r.get(0),
        )?;
        (bound, first)
    };
    // The owner first: their absence is an answer, and the source check below
    // cannot tell an answer from a failure to get one.
    let mut presence = client.owner_presence(c, &bound.source.channel).await;
    if presence == Presence::Member
        && client
            .authorize_continuation(c, &bound.source)
            .await
            .is_err()
    {
        presence = Presence::Unknown;
    }
    let s = Store::open(home)?;
    match presence {
        // The local record may have changed while Slack answered.
        Presence::Member if holds(&s.db, c, delivery, &bound)? => return Ok(true),
        Presence::Member => withdraw(&s.db, delivery, CHANGED)?,
        Presence::Absent => withdraw(&s.db, delivery, OWNER_LEFT)?,
        Presence::Unknown if now_ms() - first > UNCONFIRMED_FOR_MS => {
            withdraw(&s.db, delivery, UNCONFIRMED)?
        }
        Presence::Unknown => {
            crate::public_speech::settle(&s, delivery, "ready", None, WAITING, RECHECK_MS)?
        }
    }
    Ok(false)
}

fn body(text: &str, source: &Message, link: Option<&str>) -> String {
    let origin = match link {
        Some(link) => format!("[From this conversation]({link})"),
        None => format!("From conversation {} message {}", source.channel, source.ts),
    };
    format!("{}\n\n{origin}", text.trim_end())
}

fn save(
    s: &mut Store,
    c: &Config,
    job: &str,
    request: &Request,
    bound: &Binding,
    text: &str,
) -> Result<Value> {
    let tx = crate::huddles::source_transaction(s, c, job)?;
    if let Some(existing) = receipt(&tx, job, &request.id)? {
        return Ok(existing);
    }
    // Slack was asked about `bound.source`; save only if that is still the
    // message this response answers.
    let current = crate::huddles::job_source(&tx, c, job)?;
    ensure!(
        serde_json::to_value(&current)? == serde_json::to_value(&bound.source)?,
        "The source changed while the owner message was checked"
    );
    if sent_for(&tx, job)? >= MAX_PER_RESPONSE {
        return Ok(refusal(
            "This response has already sent its owner three messages. Put what remains in one of them next time, or say it in this conversation.",
        ));
    }
    let message = format!("{PREFIX}{}", request.id);
    let ids = crate::public_speech::enqueue_routed(&tx, job, &message, text, &bound.direct, "")?;
    ensure!(!ids.is_empty(), "An owner message cannot be empty");
    tx.execute(
        "INSERT INTO slack_owner_messages(job_id,message_id,binding) VALUES(?1,?2,?3)",
        params![job, message, serde_json::to_string(bound)?],
    )?;
    tx.execute(
        "INSERT INTO audit(time,kind,data) VALUES(?1,'owner_message_ready',?2)",
        params![
            now_ms(),
            json!({"job_id":job,"request_id":request.id,"parts":ids.len()}).to_string()
        ],
    )?;
    let saved = receipt(&tx, job, &request.id)?.context("Owner message was not saved")?;
    tx.commit()?;
    Ok(saved)
}

pub async fn call(c: &Config, home: &Path, job: &str, args: &Value) -> Result<Value> {
    let request = match parse(args) {
        Ok(request) => request,
        Err(reason) => return Ok(refusal(reason)),
    };
    let mut s = Store::open(home)?;
    // A repeated call reports the saved message without asking Slack again, so a
    // lost acknowledgment cannot change its text or send a second copy.
    if let Some(existing) = receipt(&s.db, job, &request.id)? {
        return Ok(existing);
    }
    let owner = c.context.owner_slack_user_id.as_str();
    if owner.is_empty() || !c.slack.allow_dms {
        return Ok(refusal(
            "This connection has no configured owner to message, or direct messages are turned off for it.",
        ));
    }
    let Ok(source) = crate::huddles::job_source(&s.db, c, job) else {
        return Ok(refusal(
            "This response's source conversation changed or is not a group conversation, so nothing was sent.",
        ));
    };
    if source.bot {
        return Ok(refusal(
            "This response answers another agent. Only a person's message can lead to a private message to your owner; say it in this conversation instead.",
        ));
    }
    let client = Slack::new(c, c.secrets(home)?)?;
    if client.authorize_continuation(c, &source).await.is_err() {
        return Ok(refusal(
            "The source message or its audience could not be confirmed with Slack, so nothing was sent.",
        ));
    }
    let direct = match client.owner_direct(c, &source.channel).await {
        Ok(direct) => direct,
        Err(reason) => return Ok(refusal(&reason)),
    };
    if !c.allows(&direct, "im") {
        return Ok(refusal(
            "This connection is limited to listed conversations and the owner's direct conversation is not one of them.",
        ));
    }
    let link = client.permalink(&source.channel, &source.ts).await.ok();
    let text = body(&request.text, &source, link.as_deref());
    let bound = Binding::new(c, &direct, &source);
    save(&mut s, c, job, &request, &bound, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Job;

    fn fixture() -> (tempfile::TempDir, Store, Config, Job) {
        let home = tempfile::tempdir().unwrap();
        let mut store = Store::open(home.path()).unwrap();
        let mut config = Config::new("Agent".into(), "owner".into());
        config.enabled = true;
        config.slack.team_id = "T1".into();
        config.slack.bot_user_id = "UBOT".into();
        config.context.owner_slack_user_id = "UOWNER".into();
        config.attention.settle_ms = 0;
        config.attention.minimum_reply_interval_seconds = 0;
        let source = Message {
            team: "T1".into(),
            channel: "C1".into(),
            thread: String::new(),
            ts: "10.000001".into(),
            revision: "10.000001".into(),
            sender: "UPEER".into(),
            bot: false,
            text: "<@UBOT> please tell your owner privately".into(),
            deleted: false,
            files: vec![],
        };
        store.ingest("source", &source, true).unwrap();
        let job = store.claim(&config.attention).unwrap().unwrap();
        (home, store, config, job)
    }

    fn request(id: &str, text: &str) -> Request {
        parse(&json!({"request_id":id,"text":text})).unwrap()
    }

    fn bound(store: &Store, config: &Config, job: &Job) -> Binding {
        let source = crate::huddles::job_source(&store.db, config, &job.id).unwrap();
        Binding::new(config, "DOWNER", &source)
    }

    fn rows(store: &Store) -> Vec<(String, String, String)> {
        store
            .db
            .prepare("SELECT channel,thread,text FROM public_speech")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    const FIRST: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn arguments_cannot_name_a_recipient_or_exceed_the_bound() {
        assert!(parse(&json!({"request_id":FIRST,"text":"Hello"})).is_ok());
        for refused in [
            json!({"request_id":FIRST,"text":"Hello","user_id":"UPEER"}),
            json!({"request_id":FIRST,"text":"Hello","channel":"C2"}),
            json!({"request_id":"not-a-uuid","text":"Hello"}),
            json!({"request_id":FIRST}),
            json!({"request_id":FIRST,"text":"   "}),
            json!({"request_id":FIRST,"text":"x".repeat(MAX_CHARS + 1)}),
            json!("Hello"),
        ] {
            assert!(parse(&refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn the_message_is_saved_once_for_the_owners_conversation_with_its_source() {
        let (_home, mut store, config, job) = fixture();
        let source = crate::huddles::job_source(&store.db, &config, &job.id).unwrap();
        let text = body(
            "The review needs your decision.  \n",
            &source,
            Some("https://example-team.slack.com/archives/C1/p10000001"),
        );
        assert_eq!(
            text,
            "The review needs your decision.\n\n[From this conversation](https://example-team.slack.com/archives/C1/p10000001)"
        );
        let bound = Binding::new(&config, "DOWNER", &source);
        let first = save(
            &mut store,
            &config,
            &job.id,
            &request(FIRST, "The review needs your decision."),
            &bound,
            &text,
        )
        .unwrap();
        assert_eq!(first["accepted"], true);
        assert_eq!(first["status"], "pending");
        assert_eq!(first["conversation_id"], "DOWNER");
        // The same request, even with other words, reports the saved message.
        let again = save(
            &mut store,
            &config,
            &job.id,
            &request(FIRST, "Different words"),
            &bound,
            "Different words",
        )
        .unwrap();
        assert_eq!(again["receipts"], first["receipts"]);
        assert_eq!(rows(&store), vec![("DOWNER".into(), String::new(), text)]);
        assert_eq!(
            body("Plain", &source, None),
            "Plain\n\nFrom conversation C1 message 10.000001"
        );
    }

    #[test]
    fn a_response_sends_its_owner_at_most_three_messages() {
        let (_home, mut store, config, job) = fixture();
        let bound = bound(&store, &config, &job);
        for n in 1..=3 {
            let id = format!("{n}1111111-1111-4111-8111-111111111111");
            let saved = save(
                &mut store,
                &config,
                &job.id,
                &request(&id, "Note"),
                &bound,
                &format!("Note {n}"),
            )
            .unwrap();
            assert_eq!(saved["accepted"], true);
        }
        let fourth = save(
            &mut store,
            &config,
            &job.id,
            &request("41111111-1111-4111-8111-111111111111", "Note"),
            &bound,
            "Note 4",
        )
        .unwrap();
        assert_eq!(fourth["accepted"], false);
        assert_eq!(sent_for(&store.db, &job.id).unwrap(), 3);
    }

    #[test]
    fn a_changed_source_saves_nothing() {
        let (_home, mut store, config, job) = fixture();
        let bound = bound(&store, &config, &job);
        // Slack was asked about another message than the one this response answers.
        let mut other = bound.clone();
        other.source.ts = "9.000001".into();
        assert!(
            save(
                &mut store,
                &config,
                &job.id,
                &request(FIRST, "Note"),
                &other,
                "Note"
            )
            .is_err()
        );
        store
            .db
            .execute("UPDATE jobs SET status='superseded' WHERE id=?1", [&job.id])
            .unwrap();
        assert!(
            save(
                &mut store,
                &config,
                &job.id,
                &request(FIRST, "Note"),
                &bound,
                "Note",
            )
            .is_err()
        );
        assert_eq!(sent_for(&store.db, &job.id).unwrap(), 0);
        assert_eq!(rows(&store), vec![]);
    }

    /// One owner message, then the response's ordinary reply, both waiting.
    fn queued() -> (tempfile::TempDir, Store, Config, Job, String) {
        let (home, mut store, config, job) = fixture();
        let bound = bound(&store, &config, &job);
        let saved = save(
            &mut store,
            &config,
            &job.id,
            &request(FIRST, "Note"),
            &bound,
            "Note for the owner",
        )
        .unwrap();
        let id = saved["receipts"][0]["id"].as_str().unwrap().to_owned();
        crate::public_speech::enqueue(
            &store,
            &job.id,
            &amplifier_acp_connection::speech::Utterance {
                id: "reply".into(),
                text: "Reply in the channel".into(),
            },
        )
        .unwrap();
        (home, store, config, job, id)
    }

    #[test]
    fn an_unchanged_message_is_claimed_for_the_owners_conversation() {
        let (_home, mut store, config, _job, id) = queued();
        let out = crate::public_speech::claim(&mut store, &config)
            .unwrap()
            .unwrap();
        assert_eq!(
            (out.id, out.channel, out.thread),
            (id, "DOWNER".into(), String::new())
        );
    }

    #[test]
    fn a_message_is_withdrawn_alone_when_what_it_was_accepted_for_no_longer_holds() {
        type Drift = fn(&mut Config, &Store);
        let drifts: [(&str, Drift); 7] = [
            ("another owner is configured", |c, _| {
                c.context.owner_slack_user_id = "UOTHER".into()
            }),
            ("no owner is configured", |c, _| {
                c.context.owner_slack_user_id.clear()
            }),
            ("direct messages are turned off", |c, _| {
                c.slack.allow_dms = false
            }),
            ("the owner's conversation is not a listed one", |c, _| {
                c.slack.channels = vec!["C1".into()]
            }),
            ("the saved record is another installation's", |c, _| {
                c.id = "another-installation".into()
            }),
            ("the queued destination is not the saved one", |_, s| {
                s.db.execute(
                    "UPDATE public_speech SET channel='DOTHER' WHERE channel='DOWNER'",
                    [],
                )
                .unwrap();
            }),
            ("the queued message was moved into a thread", |_, s| {
                s.db.execute(
                    "UPDATE public_speech SET thread='10.000001' WHERE channel='DOWNER'",
                    [],
                )
                .unwrap();
            }),
        ];
        for (name, drift) in drifts {
            let (_home, mut store, mut config, job, id) = queued();
            drift(&mut config, &store);
            assert!(
                crate::public_speech::claim(&mut store, &config)
                    .unwrap()
                    .is_none(),
                "{name}"
            );
            let (status, note): (String, String) = store
                .db
                .query_row(
                    "SELECT status,note FROM public_speech WHERE id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                (status.as_str(), note.as_str()),
                ("cancelled", CHANGED),
                "{name}"
            );
            let receipt = receipt(&store.db, &job.id, FIRST).unwrap().unwrap();
            assert_eq!(receipt["status"], "cancelled", "{name}");
            assert!(
                receipt["delivery"]
                    .as_str()
                    .unwrap()
                    .starts_with("Not delivered"),
                "{name}"
            );
            // The response's own reply is untouched and is the next to go.
            let next = crate::public_speech::claim(&mut store, &config)
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: the reply was lost"));
            assert_eq!(
                (next.channel.as_str(), next.text.as_str()),
                ("C1", "Reply in the channel"),
                "{name}"
            );
            assert_eq!(
                store
                    .db
                    .query_row(
                        "SELECT COUNT(*) FROM audit WHERE kind='owner_message_withheld'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                1,
                "{name}"
            );
        }
    }

    #[test]
    fn a_changed_source_withdraws_the_message_before_any_send() {
        for change in [
            "UPDATE messages SET body=json_set(body,'$.deleted',json('true'))",
            "UPDATE messages SET body=json_set(body,'$.revision','11.000001','$.text','<@UBOT> something else')",
        ] {
            let (_home, mut store, config, _job, id) = queued();
            store.db.execute(change, []).unwrap();
            assert!(
                crate::public_speech::claim(&mut store, &config)
                    .unwrap()
                    .is_none()
            );
            let status: String = store
                .db
                .query_row("SELECT status FROM public_speech WHERE id=?1", [&id], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(status, "cancelled", "{change}");
        }
    }

    #[test]
    fn only_a_message_saved_by_this_tool_is_treated_as_an_owner_message() {
        let (_home, mut store, config, job) = fixture();
        // The agent's own speech may carry any identifier, this prefix included.
        crate::public_speech::enqueue(
            &store,
            &job.id,
            &amplifier_acp_connection::speech::Utterance {
                id: format!("{PREFIX}{FIRST}"),
                text: "Reply in the channel".into(),
            },
        )
        .unwrap();
        assert!(receipt(&store.db, &job.id, FIRST).unwrap().is_none());
        assert_eq!(sent_for(&store.db, &job.id).unwrap(), 0);
        let out = crate::public_speech::claim(&mut store, &config)
            .unwrap()
            .unwrap();
        assert_eq!(out.channel, "C1");
        assert!(claimable(&store.db, &config, &out.id).unwrap());
    }

    /// A client whose every call fails without an answer from Slack.
    fn unreachable(config: &mut Config) -> Slack {
        config.slack.loopback_test = true;
        config.slack.api_base = "http://127.0.0.1:1/api/".into();
        Slack::new(
            config,
            crate::config::Secrets {
                app_token: "test-app".into(),
                bot_token: "test".into(),
            },
        )
        .unwrap()
    }

    fn state(store: &Store, id: &str) -> (String, String) {
        store
            .db
            .query_row(
                "SELECT status,note FROM public_speech WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn a_message_whose_audience_slack_cannot_confirm_waits_and_is_then_given_up() {
        let (home, mut store, mut config, _job, id) = queued();
        let client = unreachable(&mut config);
        let out = crate::public_speech::claim(&mut store, &config)
            .unwrap()
            .unwrap();
        assert_eq!(out.id, id);
        assert!(
            !authorize_delivery(home.path(), &config, &id, &client)
                .await
                .unwrap()
        );
        assert_eq!(state(&store, &id), ("ready".into(), WAITING.into()));
        // It is not offered again until the recheck is due.
        assert!(
            crate::public_speech::claim(&mut store, &config)
                .unwrap()
                .is_none()
        );
        store
            .db
            .execute(
                "UPDATE public_speech SET available=0,attempted=attempted-?2 WHERE id=?1",
                params![id, UNCONFIRMED_FOR_MS + 1],
            )
            .unwrap();
        let out = crate::public_speech::claim(&mut store, &config)
            .unwrap()
            .unwrap();
        assert_eq!(out.id, id);
        assert!(
            !authorize_delivery(home.path(), &config, &id, &client)
                .await
                .unwrap()
        );
        assert_eq!(state(&store, &id), ("cancelled".into(), UNCONFIRMED.into()));
        // What follows it in the response is no longer held back.
        let next = crate::public_speech::claim(&mut store, &config)
            .unwrap()
            .unwrap();
        assert_eq!(next.channel, "C1");
        // Speech that is not an owner message is never asked about here.
        assert!(
            authorize_delivery(home.path(), &config, &next.id, &client)
                .await
                .unwrap()
        );
    }

    #[test]
    fn the_tool_is_offered_only_to_answer_a_person_in_a_group_with_an_owner_to_reach() {
        let (_home, _store, mut config, _job) = fixture();
        assert!(offered(&config, true, true));
        assert!(!offered(&config, false, true));
        assert!(!offered(&config, true, false));
        config.slack.allow_dms = false;
        assert!(!offered(&config, true, true));
        config.slack.allow_dms = true;
        config.context.owner_slack_user_id.clear();
        assert!(!offered(&config, true, true));
    }
}
