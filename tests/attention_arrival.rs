use acp_slack_adapter::{
    config::Attention,
    model::{Decision, Message},
    store::Store,
};

fn peer(n: u64) -> Message {
    Message {
        team: "team".into(),
        channel: "channel".into(),
        thread: "".into(),
        ts: format!("{n}.000"),
        revision: format!("{n}.000"),
        sender: format!("peer{n}"),
        bot: true,
        text: format!("Agreement {n}"),
        deleted: false,
        files: vec![],
    }
}
fn attention() -> Attention {
    Attention {
        settle_ms: 0,
        ..Attention::default()
    }
}
fn hold() -> anyhow::Result<Decision> {
    Ok(Decision {
        decision: "pass".into(),
        text: String::new(),
        reason: "Attention evidence changed before admission".into(),
        media: vec![],
    })
}

#[test]
fn peer_arriving_during_attention_gets_a_fresh_candidate_not_lost_awareness() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.ingest("rook-agrees", &peer(1), true).unwrap();
    let evaluating = s.claim(&attention()).unwrap().unwrap();
    s.ingest("maker-agrees", &peer(2), true).unwrap();
    s.ingest("maker-more", &peer(3), true).unwrap();
    // The attention snapshot is now stale, and no ACP prompt was attempted.
    s.finish(&evaluating, hold(), &attention()).unwrap();
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    let fresh = s
        .claim(&attention())
        .unwrap()
        .expect("newer unsubmitted evidence still needs attention");
    assert_eq!(fresh.trigger.ts, "3.000");
    assert_eq!(s.history(&fresh.trigger).unwrap().len(), 3);
    assert_ne!(fresh.id, evaluating.id);
    assert!(!s.ingest("duplicate", &peer(3), true).unwrap());
    s.finish(&fresh, hold(), &attention()).unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
}

#[test]
fn attempted_prompt_does_not_replay_peer_awareness_as_new_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.ingest("first", &peer(1), true).unwrap();
    let active = s.claim(&attention()).unwrap().unwrap();
    s.db.execute("INSERT INTO prompt_attempts VALUES(?1,1)", [&active.id])
        .unwrap();
    s.ingest("second", &peer(2), true).unwrap();
    s.finish(&active, hold(), &attention()).unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
}

#[test]
fn restart_between_attention_and_candidate_handoff_retains_one_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.ingest("first", &peer(1), true).unwrap();
    let active = s.claim(&attention()).unwrap().unwrap();
    s.ingest("second", &peer(2), true).unwrap();
    s.db.execute("UPDATE jobs SET status='pass' WHERE id=?1", [&active.id])
        .unwrap();
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.recover().unwrap();
    s.recover().unwrap();
    let candidate = s.claim(&attention()).unwrap().unwrap();
    assert_eq!(candidate.trigger.ts, "2.000");
    s.finish(&candidate, hold(), &attention()).unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
}

#[test]
fn edit_or_stop_cannot_resurrect_a_pre_admission_candidate() {
    for stop in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        s.ingest("first", &peer(1), true).unwrap();
        let active = s.claim(&attention()).unwrap().unwrap();
        s.ingest("second", &peer(2), true).unwrap();
        if stop {
            s.db.execute("UPDATE contexts SET control_version=version+1", [])
                .unwrap();
        } else {
            let mut deleted = peer(2);
            deleted.deleted = true;
            deleted.revision = "deleted".into();
            s.ingest("delete", &deleted, false).unwrap();
        }
        s.finish(&active, hold(), &attention()).unwrap();
        // Existing source-edit reconsideration may revisit the original human
        // request, but must never resurrect the deleted/stopped peer input.
        if let Some(next) = s.claim(&attention()).unwrap() {
            assert_ne!(next.trigger.ts, "2.000", "stop={stop}");
        }
        let n: i64 =
            s.db.query_row("SELECT COUNT(*) FROM prompt_attempts", [], |r| r.get(0))
                .unwrap();
        assert_eq!(n, 0);
    }
}
