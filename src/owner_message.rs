//! A private message from the agent to its configured owner. The connector
//! fixes the recipient and the destination; delivery stays in the ordinary
//! public-speech journal, so an uncertain send is reconciled, never repeated.
//!
//! The tool is offered only for a response to a person in a group conversation,
//! never to another agent's message, and a call is accepted only while the
//! owner is a member of that conversation. The
//! message therefore tells the owner nothing they could not already read at its
//! source, and it carries a link back to that source.
use crate::{
    config::Config,
    model::{Message, now_ms},
    slack::Slack,
    store::Store,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

pub const TOOL: &str = "slack_owner_message";
const MAX_CHARS: usize = 8_000;
const MAX_PER_RESPONSE: i64 = 3;
const PREFIX: &str = "owner-message:";

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
            "SELECT id,status,slack_ts,note,channel FROM public_speech WHERE job_id=?1 AND message_id=?2 ORDER BY part",
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
    Ok(Some(
        json!({"accepted":true,"request_id":request,"recipient":"owner",
        "conversation_id":receipts[0]["conversation_id"],"status":status,"receipts":receipts,
        "delivery":"A pending or uncertain receipt is not delivery. Check it with this request_id; do not send it again under another."}),
    ))
}

fn sent_for(db: &Connection, job: &str) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COUNT(DISTINCT message_id) FROM public_speech WHERE job_id=?1 AND message_id LIKE ?2",
        params![job, format!("{PREFIX}%")],
        |r| r.get(0),
    )?)
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
    direct: &str,
    text: &str,
) -> Result<Value> {
    let tx = crate::huddles::source_transaction(s, c, job)?;
    if let Some(existing) = receipt(&tx, job, &request.id)? {
        return Ok(existing);
    }
    if sent_for(&tx, job)? >= MAX_PER_RESPONSE {
        return Ok(refusal(
            "This response has already sent its owner three messages. Put what remains in one of them next time, or say it in this conversation.",
        ));
    }
    let ids = crate::public_speech::enqueue_routed(
        &tx,
        job,
        &format!("{PREFIX}{}", request.id),
        text,
        direct,
        "",
    )?;
    ensure!(!ids.is_empty(), "An owner message cannot be empty");
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
    save(&mut s, c, job, &request, &direct, &text)
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
        let first = save(
            &mut store,
            &config,
            &job.id,
            &request(FIRST, "The review needs your decision."),
            "DOWNER",
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
            "DOWNER",
            "Different words",
        )
        .unwrap();
        assert_eq!(again["receipts"], first["receipts"]);
        let rows: Vec<(String, String, String)> = store
            .db
            .prepare("SELECT channel,thread,text FROM public_speech")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![("DOWNER".into(), String::new(), text)]);
        assert_eq!(
            body("Plain", &source, None),
            "Plain\n\nFrom conversation C1 message 10.000001"
        );
    }

    #[test]
    fn a_response_sends_its_owner_at_most_three_messages() {
        let (_home, mut store, config, job) = fixture();
        for n in 1..=3 {
            let id = format!("{n}1111111-1111-4111-8111-111111111111");
            let saved = save(
                &mut store,
                &config,
                &job.id,
                &request(&id, "Note"),
                "DOWNER",
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
            "DOWNER",
            "Note 4",
        )
        .unwrap();
        assert_eq!(fourth["accepted"], false);
        assert_eq!(sent_for(&store.db, &job.id).unwrap(), 3);
    }

    #[test]
    fn a_changed_source_saves_nothing() {
        let (_home, mut store, config, job) = fixture();
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
                "DOWNER",
                "Note",
            )
            .is_err()
        );
        assert_eq!(sent_for(&store.db, &job.id).unwrap(), 0);
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
