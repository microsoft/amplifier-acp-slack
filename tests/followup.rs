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
    acp_slack_adapter::handoff::start(&s, &j.id, "fixture-endpoint", &json!({"trigger":m}))
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
fn wake_survives_restart_without_waiting_for_public_delivery_and_never_duplicates() {
    let (dir, c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    // A pending or unknown public send is a separate delivery concern. The
    // continuation receives its actual receipt and must not blindly resend it.
    s.db.execute("UPDATE outbox SET status='uncertain'", [])
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
fn retained_legacy_depth_and_expiry_refuse_another_continuation() {
    for cause in ["depth", "expiry"] {
        let (dir, c, s, j) = fixture();
        s.db.execute("INSERT INTO followups(parent,root,depth,deadline,available,goal,endpoint,child,status) VALUES('prior','root',?1,?2,0,'accepted','endpoint',?3,'queued')",rusqlite::params![if cause=="depth" {8}else{1},if cause=="expiry" {now_ms()-1}else{now_ms()+86_400_000},j.id]).unwrap();
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
fn upgrading_preserves_all_old_chain_rows_and_their_finite_bounds() {
    for source_version in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
        db.execute_batch("CREATE TABLE followups(parent TEXT PRIMARY KEY,root TEXT NOT NULL,depth INTEGER NOT NULL,deadline INTEGER NOT NULL,available INTEGER NOT NULL,goal TEXT NOT NULL,endpoint TEXT NOT NULL,child TEXT UNIQUE,status TEXT NOT NULL DEFAULT 'proposed');
            INSERT INTO followups VALUES('a','root',8,17,9,'existing work','same endpoint','b','queued');
            INSERT INTO followups VALUES('b','root',8,17,9,'cancelled work','same endpoint',NULL,'cancelled');
            INSERT INTO followups VALUES('c','root',2,17,9,'unknown work','same endpoint',NULL,'unknown');") .unwrap();
        if source_version {
            db.execute_batch("ALTER TABLE followups ADD COLUMN source_version INTEGER NOT NULL DEFAULT -1; UPDATE followups SET source_version=19;").unwrap();
        }
        let before: Vec<Vec<rusqlite::types::Value>> = db.prepare("SELECT parent,root,depth,deadline,available,goal,endpoint,child,status FROM followups ORDER BY parent").unwrap().query_map([], |r| (0..9).map(|i|r.get(i)).collect()).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        drop(db);
        for _ in 0..2 {
            let s = Store::open(dir.path()).unwrap();
            let after: Vec<Vec<rusqlite::types::Value>> = s.db.prepare("SELECT parent,root,depth,deadline,available,goal,endpoint,child,status FROM followups ORDER BY parent").unwrap().query_map([], |r| (0..9).map(|i|r.get(i)).collect()).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert_eq!(before, after);
            let bounds = followup::input(dir.path(), "b").unwrap().unwrap();
            assert_eq!(bounds["maximum_turns"], 8);
            assert_eq!(bounds["deadline"], 17);
            assert_eq!(
                s.db.query_row(
                    "SELECT source_version FROM followups WHERE parent='a'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                if source_version { 19 } else { -1 }
            );
        }
    }
}

#[test]
fn a_pending_check_stays_bound_when_an_ordinary_job_is_prepared_first() {
    let (dir, c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    followup::pump(dir.path(), &c).unwrap();
    s.db.execute(
        "UPDATE jobs SET available=?1 WHERE status='pending'",
        [now_ms() + 10_000],
    )
    .unwrap();
    let mut peer = j.trigger.clone();
    peer.ts = "3.001".into();
    peer.revision = peer.ts.clone();
    peer.bot = true;
    peer.sender = "Upeer".into();
    peer.text = "Separate peer request".into();
    s.ingest("peer", &peer, true).unwrap();
    let first = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.trigger.ts, peer.ts);
    let prepared = s.prepare(first).unwrap();
    assert_eq!(prepared.trigger.ts, peer.ts);
    assert!(followup::input(dir.path(), &prepared.id).unwrap().is_none());
    assert_eq!(s.db.query_row("SELECT count(*) FROM jobs WHERE status='pending' AND id IN (SELECT child FROM followups)",[],|r|r.get::<_,i64>(0)).unwrap(),1);
}

#[test]
fn accepted_work_can_continue_past_eight_turns_in_its_original_conversation() {
    let (dir, c, mut s, mut job) = fixture();
    let original = serde_json::to_value(&job.trigger).unwrap();
    for turn in 1..=10 {
        propose(dir.path(), &c, &mut s, &job);
        drop(s);
        followup::pump(dir.path(), &c).unwrap();
        s = Store::open(dir.path()).unwrap();
        job = s
            .claim(&Attention {
                settle_ms: 0,
                ..Default::default()
            })
            .unwrap()
            .unwrap();
        let input = followup::input(dir.path(), &job.id).unwrap().unwrap();
        assert_eq!(input["turn"], turn);
        assert_eq!(serde_json::to_value(&job.trigger).unwrap(), original);
        acp_slack_adapter::handoff::start(
            &s,
            &job.id,
            "fixture-endpoint",
            &json!({"trigger":job.trigger}),
        )
        .unwrap();
    }
    let input = followup::input(dir.path(), &job.id).unwrap().unwrap();
    assert!(input["maximum_turns"].is_null());
    assert!(input["deadline"].is_null());
}

#[test]
fn new_work_is_not_expired_by_its_older_release_rollback_fallback() {
    let (dir, c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    s.db.execute("UPDATE followups SET deadline=?1", [now_ms() - 1])
        .unwrap();
    drop(s);
    followup::pump(dir.path(), &c).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let next = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    let continuation = followup::input(dir.path(), &next.id).unwrap().unwrap();
    assert!(continuation["deadline"].is_null());
    assert!(continuation["maximum_turns"].is_null());
    assert_eq!(next.trigger.ts, j.trigger.ts);
}

#[test]
fn accepted_peer_work_can_schedule_a_quiet_same_source_check_after_restart() {
    let (dir, c, mut s, mut j) = fixture();
    j.trigger.bot = true;
    j.trigger.sender = "Upeer".into();
    s.db.execute(
        "UPDATE jobs SET trigger=?2 WHERE id=?1",
        rusqlite::params![j.id, serde_json::to_string(&j.trigger).unwrap()],
    )
    .unwrap();
    assert_eq!(
        followup::schedule(dir.path(), &c, &j.id, 5, "Inspect the existing test result").unwrap()["scheduled"],
        true
    );
    s.finish(
        &j,
        Ok(Decision {
            decision: "pass".into(),
            text: String::new(),
            reason: "waiting for the existing run".into(),
            media: vec![],
        }),
        &c.attention,
    )
    .unwrap();
    s.db.execute("UPDATE followups SET available=0", [])
        .unwrap();
    drop(s);
    followup::pump(dir.path(), &c).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let next = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(next.trigger).unwrap(),
        serde_json::to_value(j.trigger).unwrap()
    );
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn a_peer_arriving_beside_a_scheduled_check_cannot_replace_its_source() {
    let (dir, c, mut s, j) = fixture();
    propose(dir.path(), &c, &mut s, &j);
    followup::pump(dir.path(), &c).unwrap();
    let mut peer = j.trigger.clone();
    peer.ts = "3.001".into();
    peer.revision = peer.ts.clone();
    peer.sender = "Upeer".into();
    peer.bot = true;
    peer.text = "A different peer request".into();
    s.ingest("peer", &peer, true).unwrap();
    let check = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert!(followup::input(dir.path(), &check.id).unwrap().is_some());
    let check = s.prepare(check).unwrap();
    assert_eq!(
        serde_json::to_value(check.trigger).unwrap(),
        serde_json::to_value(j.trigger).unwrap()
    );
    assert_eq!(
        s.db.query_row(
            "SELECT count(*) FROM jobs WHERE status='pending' AND json_extract(trigger,'$.ts')=?1",
            [&peer.ts],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
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
fn peer_control_is_private_even_when_malformed_or_after_a_pass() {
    for answer in [
        "The updated plan is ready.",
        r#"{"decision":"pass","reason":"already settled"}"#,
    ] {
        for footer in ["invalid", r#"{"after_seconds":-1,"goal":"Check result"}"#] {
            let (dir, c, s, j) = fixture();
            let mut trigger = j.trigger.clone();
            trigger.bot = true;
            s.db.execute(
                "UPDATE jobs SET trigger=?2 WHERE id=?1",
                rusqlite::params![j.id, serde_json::to_string(&trigger).unwrap()],
            )
            .unwrap();
            let result = followup::capture(
                dir.path(),
                &c,
                &json!({"job_id":j.id}),
                &format!("{answer}\nSLACK_FOLLOW_UP {footer}"),
            )
            .unwrap();
            assert_eq!(result, answer);
            assert_eq!(
                s.db.query_row("SELECT count(*) FROM followups", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
            let event: String =
                s.db.query_row(
                    "SELECT data FROM audit WHERE kind='continuation_not_scheduled'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let event: serde_json::Value = serde_json::from_str(&event).unwrap();
            assert_eq!(
                event,
                json!({"job":j.id,"reason":"The follow-up request was invalid, so no later turn was scheduled."})
            );
        }
    }
}

#[test]
fn followup_control_is_offered_for_known_human_and_peer_sources() {
    assert!(followup::guidance(&json!({"trigger":{"bot":false}})).contains("SLACK_FOLLOW_UP"));
    assert!(followup::guidance(&json!({"trigger":{"bot":true}})).contains("SLACK_FOLLOW_UP"));
    for input in [json!({}), json!({"trigger":{"bot":"false"}})] {
        assert!(followup::guidance(&input).is_empty());
    }
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

#[test]
fn quiet_background_result_check_does_not_require_a_public_progress_message() {
    let (dir, c, mut s, j) = fixture();
    let output = followup::capture(dir.path(), &c, &json!({"job_id":j.id}),
        "{\"decision\":\"pass\",\"reason\":\"The existing process is still running\"}\nSLACK_FOLLOW_UP {\"after_seconds\":5,\"goal\":\"Inspect the existing run and report its result, including failure. Do not start it again.\"}").unwrap();
    s.finish(
        &j,
        Ok(serde_json::from_str(&output).unwrap()),
        &Attention::default(),
    )
    .unwrap();
    s.db.execute("UPDATE followups SET available=0", [])
        .unwrap();
    drop(s);
    followup::pump(dir.path(), &c).unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(
        s.db.query_row(
            "SELECT count(*) FROM jobs WHERE status='pending'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn new_status_question_keeps_existing_result_check_in_the_work_handoff() {
    use acp_slack_adapter::{driver, handoff};
    let (dir, c, mut s, j) = fixture();
    let mut input = driver::request(&c, &j.trigger, s.history(&j.trigger).unwrap());
    input["job_id"] = json!(j.id);
    let endpoint = "fixture-endpoint";
    handoff::start(&s, &j.id, endpoint, &input).unwrap();
    propose(dir.path(), &c, &mut s, &j);
    let mut m = j.trigger.clone();
    m.ts = "3.001".into();
    m.revision = m.ts.clone();
    m.text = "Any result yet?".into();
    s.ingest("status-question", &m, true).unwrap();
    let next = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    let pending = handoff::attach(&s, &next, endpoint).unwrap().unwrap();
    assert_eq!(
        pending["earlier_requests"][0]["pending_check"],
        "Verify the existing artifact"
    );
    assert!(followup::input(dir.path(), &next.id).unwrap().is_none());
    handoff::start(
        &s,
        &next.id,
        endpoint,
        &json!({"trigger":m,"work_handoff":pending}),
    )
    .unwrap();
    assert!(handoff::attach(&s, &next, endpoint).unwrap().is_some()); // same-job recovery
}
