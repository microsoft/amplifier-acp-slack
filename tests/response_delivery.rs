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

#[test]
fn bot_traffic_does_not_discard_a_completed_turn_but_source_edits_still_do() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = attention();
    let first = message("1.000001");
    s.ingest("first", &first, true).unwrap();
    let claimed = s.claim(&a).unwrap().unwrap();
    let job = s.prepare(claimed).unwrap();
    let mut bot = message("2.000001");
    bot.bot = true;
    bot.sender = "Uagent".into();
    s.ingest("bot", &bot, false).unwrap();
    s.finish(&job, speak(), &a).unwrap();
    let out = s.claim_outgoing(&config()).unwrap().unwrap();
    assert_eq!(out.text, "PRIVATE DRAFT");
    s.delivery(&out.id, "sent", Some("3.000001"), "", 0)
        .unwrap();
    s.ingest("third", &message("4.000001"), true).unwrap();
    let job = s.claim(&a).unwrap().unwrap();
    bot.revision = "5.000001".into();
    bot.text = "Corrected source".into();
    s.ingest("edit", &bot, false).unwrap();
    s.finish(&job, speak(), &a).unwrap();
    assert!(s.claim_outgoing(&config()).unwrap().is_none());
}

#[test]
fn completed_replies_wait_durably_without_rerunning_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let mut c = config();
    c.attention = attention();
    c.attention.minimum_reply_interval_seconds = 60;
    s.ingest("one", &message("1.000001"), true).unwrap();
    let j = s.claim(&c.attention).unwrap().unwrap();
    s.finish(&j, speak(), &c.attention).unwrap();
    let out = s.claim_outgoing(&c).unwrap().unwrap();
    s.delivery(&out.id, "sent", Some("2.000001"), "", 0)
        .unwrap();
    let mut bot = message("3.000001");
    bot.bot = true;
    s.ingest("two", &bot, true).unwrap();
    let j = s.claim(&c.attention).unwrap().unwrap();
    s.finish(&j, speak(), &c.attention).unwrap();
    assert!(s.claim_outgoing(&c).unwrap().is_none());
    let facts = s.response_delivery(&j.context, "future").unwrap();
    assert_eq!(receipt(&facts, &j.id)["status"], "pending");
    assert!(receipt(&facts, &j.id)["retry_at"].is_number());
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.recover().unwrap();
    s.db.execute(
        "UPDATE outbox SET attempted=attempted-61000 WHERE attempted>0",
        [],
    )
    .unwrap();
    s.db.execute("UPDATE outbox SET available=0 WHERE status='ready'", [])
        .unwrap();
    let resumed = s.claim_outgoing(&c).unwrap().unwrap();
    assert_eq!(resumed.text, "PRIVATE DRAFT");
    assert!(s.claim(&c.attention).unwrap().is_none());
}

#[test]
fn hourly_model_capacity_defers_the_same_job_instead_of_losing_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let mut a = attention();
    a.max_decisions_per_agent_per_hour = 1;
    s.ingest("one", &message("1.000001"), true).unwrap();
    let j = s.claim(&a).unwrap().unwrap();
    s.finish(
        &j,
        Ok(Decision {
            decision: "pass".into(),
            text: String::new(),
            reason: "done".into(),
            media: vec![],
        }),
        &a,
    )
    .unwrap();
    s.ingest("two", &message("2.000001"), true).unwrap();
    assert!(s.claim(&a).unwrap().is_none());
    let (id, available): (String, i64) =
        s.db.query_row(
            "SELECT id,available FROM jobs WHERE status='pending'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(available > acp_slack_adapter::model::now_ms());
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.recover().unwrap();
    assert!(s.claim(&a).unwrap().is_none());
    s.db.execute(
        "UPDATE jobs SET updated=updated-3601000 WHERE status!='pending'",
        [],
    )
    .unwrap();
    s.db.execute("UPDATE jobs SET available=0 WHERE status='pending'", [])
        .unwrap();
    assert_eq!(s.claim(&a).unwrap().unwrap().id, id);
}

#[test]
fn bot_continuation_is_owner_opt_in_and_keeps_hourly_limits() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let mut c = config();
    c.attention = attention();
    c.attention.max_bot_replies_without_human = 1;
    let mut bot = message("1.000001");
    bot.bot = true;
    s.ingest("one", &bot, true).unwrap();
    let j = s.claim(&c.attention).unwrap().unwrap();
    s.finish(&j, speak(), &c.attention).unwrap();
    let out = s.claim_outgoing(&c).unwrap().unwrap();
    s.delivery(&out.id, "sent", Some("2.000001"), "", 0)
        .unwrap();
    bot.ts = "3.000001".into();
    bot.revision = bot.ts.clone();
    s.ingest("two", &bot, true).unwrap();
    let j = s.claim(&c.attention).unwrap().unwrap();
    s.finish(&j, speak(), &c.attention).unwrap();
    assert!(s.claim_outgoing(&c).unwrap().is_none());
    s.db.execute(
        "UPDATE outbox SET attempted=attempted-61000 WHERE attempted>0",
        [],
    )
    .unwrap();
    s.db.execute("UPDATE outbox SET available=0", []).unwrap();
    assert!(s.claim_outgoing(&c).unwrap().is_none()); // legacy human gate still applies
    c.attention.bot_exchange_cooldown_seconds = 60;
    c.attention.max_replies_per_context_per_hour = 1;
    s.db.execute("UPDATE outbox SET available=0", []).unwrap();
    assert!(s.claim_outgoing(&c).unwrap().is_none()); // opting in never bypasses hourly budget
    c.attention.max_replies_per_context_per_hour = 30;
    s.db.execute("UPDATE outbox SET available=0", []).unwrap();
    assert!(s.claim_outgoing(&c).unwrap().is_some());
}

#[test]
fn reservations_wait_for_other_connections_instead_of_aborting_the_connector() {
    for reserve_delivery in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let a = attention();
        s.ingest("work", &message("1.000001"), true).unwrap();
        if reserve_delivery {
            let job = s.claim(&a).unwrap().unwrap();
            s.finish(&job, speak(), &a).unwrap();
        }
        // Permissions and status use their own connections. Hold a write open
        // while the worker reserves a job or reply against the same database.
        let mut other = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
        let tx = other
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "INSERT INTO settings VALUES('concurrent-check','active')",
            [],
        )
        .unwrap();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready.send(()).unwrap();
            if reserve_delivery {
                s.claim_outgoing(&config()).map(|out| out.map(|v| v.id))
            } else {
                s.claim(&a).map(|job| job.map(|v| v.id))
            }
        });
        started.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        tx.commit().unwrap();
        let result = worker.join().unwrap();
        assert!(result.is_ok(), "reservation {reserve_delivery}: {result:?}");
        assert!(result.unwrap().is_some());
    }
}

#[test]
fn restart_delivers_a_durable_completed_result_without_replaying_model_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let m = message("1.001");
    s.ingest("receipt-source", &m, true).unwrap();
    let job = s.claim(&attention()).unwrap().unwrap();
    let result = speak().unwrap();
    s.db.execute(
        "INSERT INTO turn_receipts VALUES(?1,?2,?3)",
        rusqlite::params![job.id, serde_json::to_string(&result).unwrap(), 1],
    )
    .unwrap();
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.recover().unwrap();
    s.recover().unwrap();
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        s.db.query_row("SELECT status FROM jobs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "speak"
    );
    assert!(s.claim(&attention()).unwrap().is_none());
}
