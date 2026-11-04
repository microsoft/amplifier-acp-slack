use acp_slack_adapter::{
    config::{Attention, Config},
    handoff,
    model::{Decision, Message},
    slack,
    store::Store,
};
use serde_json::json;
fn message(ts: &str, text: &str) -> Message {
    Message {
        team: "Ttest".into(),
        channel: "Dtest".into(),
        thread: "".into(),
        ts: ts.into(),
        revision: ts.into(),
        sender: "Uhuman".into(),
        bot: false,
        text: text.into(),
        deleted: false,
        files: vec![],
    }
}
fn attention() -> Attention {
    Attention {
        settle_ms: 0,
        minimum_reply_interval_seconds: 0,
        ..Default::default()
    }
}
fn config() -> Config {
    let mut c = Config::new("Fixture".into(), "test".into());
    c.enabled = true;
    c.slack.team_id = "Ttest".into();
    c.slack.bot_user_id = "Uagent".into();
    c.slack.allow_dms = true;
    c
}
fn decision() -> anyhow::Result<Decision> {
    Ok(Decision {
        decision: "speak".into(),
        text: "result".into(),
        reason: "".into(),
        media: vec![],
    })
}
#[test]
fn unfinished_work_follows_latest_question_but_not_edits_or_other_endpoints() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let a = attention();
    let first = message("1.000001", "Create the branch, test it, and open a PR.");
    s.ingest("a", &first, true).unwrap();
    let old = s.claim(&a).unwrap().unwrap();
    handoff::start(&s, &old.id, "endpoint", &json!({"trigger":first})).unwrap();
    handoff::observe(&s,&old.id,&json!({"sessionUpdate":"tool_call","toolCallId":"branch","title":"Creating branch","status":"completed","rawInput":"PRIVATE"})).unwrap();
    handoff::observe(&s,&old.id,&json!({"sessionUpdate":"tool_call","toolCallId":"pr","title":"Creating PR","status":"pending"})).unwrap();
    handoff::observe(
        &s,
        &old.id,
        &json!({"sessionUpdate":"tool_call_update","toolCallId":"pr","status":"failed"}),
    )
    .unwrap();
    s.ingest(
        "b",
        &message("2.000001", "Can I talk while you work?"),
        true,
    )
    .unwrap();
    s.finish(&old, decision(), &a).unwrap();
    let next = s.claim(&a).unwrap().unwrap();
    let h = handoff::attach(&s, &next, "endpoint").unwrap().unwrap();
    assert_eq!(
        h["earlier_requests"][0]["observed_tools"][0]["status"],
        "completed"
    );
    assert_eq!(
        h["earlier_requests"][0]["observed_tools"][1]["status"],
        "failed"
    );
    assert!(!h.to_string().contains("PRIVATE"));
    assert!(handoff::attach(&s, &next, "other").unwrap().is_none());
    let receipt = s.response_delivery(&next.context, &next.id).unwrap();
    assert!(
        receipt["items"][0]["delivery_note"]
            .as_str()
            .is_some_and(|v| v.contains("changed")),
        "{receipt}"
    );
    let mut edit = first.clone();
    edit.revision = "3.000001".into();
    edit.text = "Stop; do not open the PR".into();
    s.ingest("edit", &edit, false).unwrap();
    assert!(handoff::attach(&s, &next, "endpoint").unwrap().is_none());
}
#[test]
fn status_creation_updates_and_deletion_never_change_conversation() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let c = config();
    let m = message("1.000001", "Build it");
    s.ingest("a", &m, true).unwrap();
    let job = s.claim(&attention()).unwrap().unwrap();
    for event in [
        json!({"type":"message","channel":"Dtest","channel_type":"im","user":"Uother","bot_id":"Bother","ts":"2.0","text":"Thinking…","metadata":{"event_type":"acp_work_status"}}),
        json!({"type":"message","subtype":"message_changed","channel":"Dtest","channel_type":"im","message":{"ts":"2.0","text":"Reading files…","bot_id":"Bother"}}),
        json!({"type":"message","subtype":"message_deleted","channel":"Dtest","channel_type":"im","deleted_ts":"2.0","previous_message":{"ts":"2.0","bot_id":"Bother"}}),
        json!({"type":"message","subtype":"message_deleted","channel":"Dtest","channel_type":"im","deleted_ts":"3.0","previous_message":{"ts":"3.0","metadata":{"event_type":"acp_work_status"}}}),
    ] {
        slack::receive(
            &c,
            &mut s,
            &json!({"event_id":uuid::Uuid::new_v4().to_string(),"event":event}),
        )
        .unwrap();
    }
    let version: i64 =
        s.db.query_row(
            "SELECT version FROM contexts WHERE id=?1",
            [&job.context],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, job.version);
    assert!(s.message("Dtest", "2.0").unwrap().is_none());
    // Real bot messages and their edits still affect context.
    slack::receive(&c,&mut s,&json!({"event_id":"real","event":{"type":"message","channel":"Dtest","channel_type":"im","ts":"4.0","user":"Uother","bot_id":"Bother","text":"Actual completed work"}})).unwrap();
    assert!(s.message("Dtest", "4.0").unwrap().is_some());
}
