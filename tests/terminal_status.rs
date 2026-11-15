//! A job's terminal status and the activity row that explains it become visible
//! together. The status message is worded from that row and keeps its first
//! wording, so a reader that saw the status alone would publish the wrong outcome.
use acp_slack_adapter::{
    config::Attention,
    diagnostics::{self, Failure},
    model::{Decision, Message},
    store::Store,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const UNEXPLAINED: &str = "SELECT COUNT(*) FROM jobs j WHERE \
    (j.status='failed' AND NOT EXISTS(SELECT 1 FROM audit a WHERE a.kind='driver_failed' AND json_extract(a.data,'$.job')=j.id)) \
    OR (j.status='superseded' AND j.note LIKE 'Conversation changed during this turn%' \
        AND NOT EXISTS(SELECT 1 FROM audit a WHERE a.kind='stale_contribution_withheld' AND json_extract(a.data,'$.job')=j.id))";

#[test]
fn another_reader_never_sees_a_terminal_status_before_its_activity_row() {
    let home = tempfile::tempdir().unwrap();
    let mut store = Store::open(home.path()).unwrap();
    let attention = Attention {
        settle_ms: 0,
        minimum_reply_interval_seconds: 0,
        max_decisions_per_agent_per_hour: 100_000,
        ..Default::default()
    };
    let (stop, polls, unexplained) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    // A second connection, as the status loop and the management service use.
    let reader = {
        let (path, stop, polls, unexplained) = (
            home.path().to_owned(),
            stop.clone(),
            polls.clone(),
            unexplained.clone(),
        );
        std::thread::spawn(move || {
            let reader = Store::open(&path).unwrap();
            while !stop.load(Ordering::SeqCst) {
                let found: i64 = reader.db.query_row(UNEXPLAINED, [], |r| r.get(0)).unwrap();
                unexplained.fetch_add(found as usize, Ordering::SeqCst);
                polls.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    while polls.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    const TURNS: usize = 120;
    for turn in 0..TURNS {
        let ts = format!("{}.000001", turn + 1);
        let source = Message {
            team: "TTEST".into(),
            channel: format!("D{turn}"),
            thread: "".into(),
            ts: ts.clone(),
            revision: ts,
            sender: "UHUMAN".into(),
            bot: false,
            text: "Please do this".into(),
            deleted: false,
            files: vec![],
        };
        store
            .ingest(&format!("event-{turn}"), &source, true)
            .unwrap();
        let job = store.claim(&attention).unwrap().unwrap();
        if turn % 2 == 0 {
            // The agent's host did not forward the request.
            store
                .finish(&job, Err(Failure::not_forwarded().into()), &attention)
                .unwrap();
        } else {
            // The conversation changed while the agent was working.
            store
                .db
                .execute(
                    "UPDATE contexts SET control_version=version+1 WHERE id=?1",
                    [&job.context],
                )
                .unwrap();
            let reply = Decision {
                decision: "speak".into(),
                text: "A reply to the earlier state".into(),
                reason: "".into(),
                media: vec![],
            };
            store.finish(&job, Ok(reply), &attention).unwrap();
        }
    }
    stop.store(true, Ordering::SeqCst);
    reader.join().unwrap();

    // The reader really interleaved with the turns, and both outcomes happened.
    assert!(polls.load(Ordering::SeqCst) > TURNS);
    let count = |sql: &str| -> usize {
        store.db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM jobs WHERE status='failed'"),
        TURNS / 2
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM audit WHERE kind='stale_contribution_withheld'"),
        TURNS / 2
    );
    assert_eq!(count(UNEXPLAINED), 0);
    assert_eq!(unexplained.load(Ordering::SeqCst), 0);

    // The row is the one the status message is worded from.
    let failed: String = store
        .db
        .query_row(
            "SELECT id FROM jobs WHERE status='failed' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        diagnostics::recorded_summary(&store, &failed).unwrap(),
        Some(Failure::not_forwarded().summary())
    );
}
