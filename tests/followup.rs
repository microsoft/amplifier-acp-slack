use acp_slack_adapter::{
    config::{Attention, Config},
    followup,
    model::{Decision, Message, now_ms},
    store::Store,
};
use serde_json::json;
fn fixture() -> (
    tempfile::TempDir,
    Config,
    Store,
    acp_slack_adapter::store::Job,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Config::new("Fixture".into(), "tester".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.slack.allow_dms = true;
    let mut s = Store::open(dir.path()).unwrap();
    let m = Message {
        team: "Ttest".into(),
        channel: "Dhuman".into(),
        thread: "".into(),
        ts: "1.001".into(),
        revision: "1.001".into(),
        sender: "Uhuman".into(),
        bot: false,
        text: "Please finish this task".into(),
        deleted: false,
        files: vec![],
    };
    s.ingest("human", &m, true).unwrap();
    let j = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    (dir, c, s, j)
}
fn propose(dir: &std::path::Path, c: &Config, s: &mut Store, j: &acp_slack_adapter::store::Job) {
    let text = followup::capture(
        dir,
        c,
        &json!({"job_id":j.id}),
        "Progress\nSLACK_FOLLOW_UP {\"after_seconds\":5,\"goal\":\"Verify the existing artifact\"}",
    )
    .unwrap();
    assert_eq!(text, "Progress");
    s.finish(
        j,
        Ok(Decision {
            decision: "speak".into(),
            text,
            reason: "accepted work".into(),
            media: vec![],
        }),
        &Attention::default(),
    )
    .unwrap();
    s.db.execute("UPDATE followups SET available=0", [])
        .unwrap();
}
#[test]
fn wake_survives_restart_but_waits_for_delivery_and_never_duplicates() {
    let (dir, c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    followup::pump(dir.path(), &c).unwrap();
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    s.db.execute("UPDATE outbox SET status='sent',slack_ts='2.001'", [])
        .unwrap();
    drop(s);
    followup::pump(dir.path(), &c).unwrap();
    followup::pump(dir.path(), &c).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let wake = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert_eq!(wake.context, j.context);
    assert_eq!(wake.trigger.sender, j.trigger.sender);
    assert_eq!(wake.trigger.ts, j.trigger.ts);
    assert_eq!(
        followup::input(dir.path(), &wake.id).unwrap().unwrap()["turn"],
        1
    );
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn new_human_message_cancels_wake_and_cannot_inherit_old_goal() {
    let (dir, c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    s.db.execute("UPDATE outbox SET status='sent'", []).unwrap();
    followup::pump(dir.path(), &c).unwrap();
    let mut m = j.trigger.clone();
    m.ts = "3.001".into();
    m.revision = m.ts.clone();
    m.text = "Stop; do this instead".into();
    s.ingest("newhuman", &m, true).unwrap();
    let next = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert_eq!(next.trigger.text, m.text);
    assert!(followup::input(dir.path(), &next.id).unwrap().is_none());
}
#[test]
fn endpoint_change_cancels_the_old_agents_continuation() {
    let (dir, mut c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    s.db.execute("UPDATE outbox SET status='sent'", []).unwrap();
    c.acp.command = "other-agent".into();
    followup::pump(dir.path(), &c).unwrap();
    assert_eq!(
        s.db.query_row("SELECT status FROM followups", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "cancelled"
    );
}

#[test]
fn maximum_depth_expiry_and_bot_source_refuse_another_continuation() {
    for cause in ["depth", "expiry", "bot"] {
        let (dir, c, s, j) = fixture();
        if cause == "bot" {
            let mut m = j.trigger.clone();
            m.bot = true;
            s.db.execute(
                "UPDATE jobs SET trigger=?2 WHERE id=?1",
                rusqlite::params![j.id, serde_json::to_string(&m).unwrap()],
            )
            .unwrap();
        } else {
            s.db.execute("INSERT INTO followups(parent,root,depth,deadline,available,goal,endpoint,child,status) VALUES('prior','root',?1,?2,0,'accepted','endpoint',?3,'queued')",rusqlite::params![if cause=="depth" {8}else{1},if cause=="expiry" {now_ms()-1}else{now_ms()+86_400_000},j.id]).unwrap();
        }
        let answer=followup::capture(dir.path(),&c,&json!({"job_id":j.id}),"{\"decision\":\"speak\",\"text\":\"Progress\"}\nSLACK_FOLLOW_UP {\"after_seconds\":5,\"goal\":\"Next step\"}").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(parsed["decision"], "speak");
        assert_eq!(
            s.db.query_row(
                "SELECT COUNT(*) FROM followups WHERE parent=?1",
                [&j.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
#[test]
fn malformed_footer_preserves_the_answer_without_scheduling_work() {
    let (dir, c, s, j) = fixture();
    let answer = followup::capture(
        dir.path(),
        &c,
        &json!({"job_id":j.id}),
        "Completed the first step.\nSLACK_FOLLOW_UP invalid",
    )
    .unwrap();
    assert!(answer.starts_with("Completed the first step."));
    assert!(answer.contains("no later turn was scheduled"));
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM followups", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn incorporated_update_preserves_a_later_continuation_without_changing_authority_version() {
    let (dir, c, mut s, j) = fixture();
    let original_version = j.version;
    let mut m = j.trigger.clone();
    m.ts = "2.001".into();
    m.revision = m.ts.clone();
    m.text = "Also verify the output".into();
    s.ingest("steer", &m, true).unwrap();
    let input = acp_slack_adapter::steering::next(&s, &j.context, &j.id)
        .unwrap()
        .unwrap();
    acp_slack_adapter::steering::submit(&s, &input, &j.id, "session").unwrap();
    acp_slack_adapter::steering::injected(&s, &input.id, &json!({})).unwrap();
    acp_slack_adapter::steering::receipt(
        &s,
        &j.id,
        &json!({"inputId":input.id,"state":"incorporated"}),
    )
    .unwrap();
    propose(dir.path(), &c, &mut s, &j);
    s.db.execute("UPDATE outbox SET status='sent'", []).unwrap();
    followup::pump(dir.path(), &c).unwrap();
    assert_eq!(
        s.db.query_row("SELECT version FROM jobs WHERE id=?1", [&j.id], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        original_version
    );
    assert_eq!(
        s.db.query_row("SELECT status FROM followups", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "queued"
    );
}
#[test]
fn queued_or_unknown_input_prevents_automatic_continuation() {
    for unknown in [false, true] {
        let (dir, c, mut s, j) = fixture();
        let mut m = j.trigger.clone();
        m.ts = "2.001".into();
        m.revision = m.ts.clone();
        s.ingest("steer", &m, true).unwrap();
        if unknown {
            let input = acp_slack_adapter::steering::next(&s, &j.context, &j.id)
                .unwrap()
                .unwrap();
            acp_slack_adapter::steering::submit(&s, &input, &j.id, "session").unwrap();
            acp_slack_adapter::steering::fail(&s, &input.id, "unknown").unwrap();
        }
        let text = followup::capture(
            dir.path(),
            &c,
            &json!({"job_id":j.id}),
            "Progress\nSLACK_FOLLOW_UP {\"after_seconds\":5,\"goal\":\"Continue\"}",
        )
        .unwrap();
        assert!(text.contains("No automatic follow-up"));
        assert_eq!(
            s.db.query_row("SELECT COUNT(*) FROM followups", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
