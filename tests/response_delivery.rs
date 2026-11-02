use acp_slack_adapter::{
    config::{Attention, Config},
    model::{Decision, Message},
    store::Store,
};
use serde_json::Value;

fn message(ts: &str) -> Message {
    Message {
        team: "Ttest".into(),
        channel: "Croom".into(),
        thread: String::new(),
        ts: ts.into(),
        revision: ts.into(),
        sender: "Uhuman".into(),
        bot: false,
        text: "PRIVATE SOURCE".into(),
        deleted: false,
        files: vec![],
    }
}
fn attention() -> Attention {
    Attention {
        settle_ms: 0,
        minimum_reply_interval_seconds: 0,
        ..Attention::default()
    }
}
fn speak() -> anyhow::Result<Decision> {
    Ok(Decision {
        decision: "speak".into(),
        text: "PRIVATE DRAFT".into(),
        reason: "Useful addition".into(),
        media: vec![],
    })
}
fn config() -> Config {
    let mut c = Config::new("Fixture".into(), "tester".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.slack.channels = vec!["Croom".into()];
    c
}
fn receipt<'a>(delivery: &'a Value, id: &str) -> &'a Value {
    delivery["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["job_id"] == id)
        .unwrap()
}

#[test]
fn newer_queued_message_keeps_its_trigger_and_learns_earlier_draft_was_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = attention();
    let first = message("1.000001");
    s.ingest("first", &first, true).unwrap();
    let old = s.claim(&a).unwrap().unwrap();
    let newer = message("2.000001");
    s.ingest("newer", &newer, true).unwrap();
    s.finish(&old, speak(), &a).unwrap();
    let next = s.claim(&a).unwrap().unwrap();
    assert_eq!(next.trigger.ts, newer.ts);
    assert!(next.reconsidered);
    let facts = s.response_delivery(&next.context, &next.id).unwrap();
    assert_eq!(receipt(&facts, &old.id)["status"], "not_delivered");
    assert_eq!(receipt(&facts, &old.id)["slack_message_id"], Value::Null);
    assert!(!facts.to_string().contains("PRIVATE"));
    assert!(s.claim_outgoing(&config()).unwrap().is_none());
    // The next result may be published; the old response is never replayed.
    s.finish(&next, speak(), &a).unwrap();
    let out = s.claim_outgoing(&config()).unwrap().unwrap();
    s.delivery(&out.id, "sent", Some("3.000001"), "Confirmed", 0)
        .unwrap();
    let facts = s.response_delivery(&next.context, "future").unwrap();
    assert_eq!(receipt(&facts, &next.id)["status"], "delivered");
    assert_eq!(receipt(&facts, &next.id)["slack_message_id"], "3.000001");
    assert_eq!(receipt(&facts, &old.id)["status"], "not_delivered");
}

#[test]
fn cancelled_queued_send_is_reported_and_notice_survives_coalescing() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = attention();
    let first = message("1.000001");
    s.ingest("first", &first, true).unwrap();
    let old = s.claim(&a).unwrap().unwrap();
    s.finish(&old, speak(), &a).unwrap();
    s.ingest("second", &message("2.000001"), true).unwrap();
    let running = s.claim(&a).unwrap().unwrap();
    s.ingest("third", &message("3.000001"), true).unwrap();
    assert!(s.claim_outgoing(&config()).unwrap().is_none());
    let next = s.prepare(running).unwrap();
    assert_eq!(next.trigger.ts, "3.000001");
    assert!(next.reconsidered);
    let facts = s.response_delivery(&next.context, &next.id).unwrap();
    assert_eq!(receipt(&facts, &old.id)["status"], "not_delivered");
}

#[test]
fn pending_and_uncertain_are_not_treated_as_failed_or_automatically_resent() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = attention();
    let first = message("1.000001");
    s.ingest("first", &first, true).unwrap();
    let old = s.claim(&a).unwrap().unwrap();
    s.finish(&old, speak(), &a).unwrap();
    assert_eq!(
        receipt(
            &s.response_delivery(&old.context, "future").unwrap(),
            &old.id
        )["status"],
        "pending"
    );
    let out = s.claim_outgoing(&config()).unwrap().unwrap();
    s.recover().unwrap();
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    assert_eq!(
        receipt(
            &s.response_delivery(&old.context, "future").unwrap(),
            &old.id
        )["status"],
        "uncertain"
    );
    assert!(s.claim_outgoing(&config()).unwrap().is_none());
    s.echo(&out.id, "2.000001").unwrap();
    assert_eq!(
        receipt(
            &s.response_delivery(&old.context, "future").unwrap(),
            &old.id
        )["status"],
        "delivered"
    );
    let mut other = first.clone();
    other.channel = "Cother".into();
    assert!(
        s.response_delivery(&other.context(), "future").unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    other.channel = first.channel.clone();
    other.thread = first.ts.clone();
    assert!(
        s.response_delivery(&other.context(), "future").unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
