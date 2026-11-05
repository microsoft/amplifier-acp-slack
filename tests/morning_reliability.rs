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
    // A real previously withheld draft still carries a handoff. Ordinary new
    // input no longer produces this state by itself.
    s.db.execute(
        "UPDATE jobs SET status='superseded',note='Source context changed' WHERE id=?1",
        [&old.id],
    )
    .unwrap();
    s.db.execute("DELETE FROM outbox WHERE job_id=?1", [&old.id])
        .unwrap();
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

#[tokio::test]
async fn automatic_approvals_survive_activity_writes_and_still_reject_stale_sources() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let home = tempfile::tempdir().unwrap();
    let mut c = config();
    c.acp.client.approvals_required = false;
    c.save(home.path()).unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let first = message("1.000001", "Build the project");
    s.ingest("first", &first, true).unwrap();
    let job = s.claim(&attention()).unwrap().unwrap();
    let input = json!({"job_id":job.id,"conversation":{"context":job.context,"channel":first.channel,"thread":first.thread},"trigger":first});
    let params = json!({"sessionId":"session","toolCall":{"toolCallId":"tool","title":"Build"},"options":[{"optionId":"yes","name":"Allow once","kind":"allow_once"}]});
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let other = Store::open(home.path()).unwrap();
    let writer = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            other
                .audit("activity_fixture", json!({"working":true}))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    let mut outcomes = Vec::new();
    for _ in 0..100 {
        outcomes.push(
            acp_slack_adapter::permissions::request(&c, home.path(), &input, "session", &params)
                .await,
        );
    }
    done.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    for outcome in outcomes {
        assert_eq!(outcome.unwrap()["outcome"]["outcome"], "selected");
    }
    s.ingest("followup", &message("2.000001", "How is it going?"), true)
        .unwrap();
    let live = acp_slack_adapter::permissions::request(&c, home.path(), &input, "session", &params)
        .await
        .unwrap();
    assert_eq!(live["outcome"]["outcome"], "selected");
    let mut edit = first.clone();
    edit.revision = "3.000001".into();
    edit.text = "Retracted source request".into();
    s.ingest("retract", &edit, false).unwrap();
    let stale =
        acp_slack_adapter::permissions::request(&c, home.path(), &input, "session", &params)
            .await
            .unwrap();
    assert_eq!(stale["outcome"]["outcome"], "cancelled");
    assert_eq!(
        stale["_meta"]["com.microsoft.amplifier.permission"]["reason"],
        "source_changed"
    );
}

#[tokio::test]
async fn thread_bookkeeping_preserves_work_but_actual_root_edits_revoke_it() {
    let home = tempfile::tempdir().unwrap();
    let mut c = config();
    c.acp.client.approvals_required = false;
    c.save(home.path()).unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let root = message("1.000001", "Build this result");
    s.ingest("root", &root, false).unwrap();
    let mut reply = message("2.000001", "Continue here");
    reply.thread = root.ts.clone();
    s.ingest("reply", &reply, true).unwrap();
    let job = s.claim(&attention()).unwrap().unwrap();
    let input = json!({"job_id":job.id,"conversation":{"context":job.context,"channel":reply.channel,"thread":reply.thread},"trigger":reply});
    let params = json!({"sessionId":"session","toolCall":{"toolCallId":"tool","title":"Build"},"options":[{"optionId":"yes","name":"Allow once","kind":"allow_once"}]});
    for n in 3..6 {
        slack::receive(&c,&mut s,&json!({"event_id":format!("bookkeeping-{n}"),"event":{"type":"message","subtype":"message_changed","channel":"Dtest","channel_type":"im","event_ts":format!("{n}.000001"),"message":{"ts":root.ts,"thread_ts":root.ts,"user":root.sender,"text":root.text,"reply_count":n,"latest_reply":format!("{n}.0")}}})).unwrap();
        let actual =
            acp_slack_adapter::permissions::request(&c, home.path(), &input, "session", &params)
                .await
                .unwrap();
        assert_eq!(actual["outcome"]["outcome"], "selected");
        assert_eq!(
            s.message(&root.channel, &root.ts)
                .unwrap()
                .unwrap()
                .revision,
            root.revision
        );
    }
    slack::receive(&c,&mut s,&json!({"event_id":"real-edit","event":{"type":"message","subtype":"message_changed","channel":"Dtest","channel_type":"im","event_ts":"6.000001","message":{"ts":root.ts,"thread_ts":root.ts,"user":root.sender,"text":"Stop this work"}}})).unwrap();
    let actual =
        acp_slack_adapter::permissions::request(&c, home.path(), &input, "session", &params)
            .await
            .unwrap();
    assert_eq!(actual["outcome"]["outcome"], "cancelled");
    s.finish(&job, decision(), &attention()).unwrap();
    let status: String =
        s.db.query_row("SELECT status FROM jobs WHERE id=?1", [job.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "superseded");
}
