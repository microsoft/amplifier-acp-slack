use acp_slack_adapter::{
    config::{Attention, Config},
    model::{Decision, Message},
    slack, steering,
    store::Store,
};
use serde_json::json;
fn message(n: u64, bot: bool) -> Message {
    Message {
        team: "T".into(),
        channel: "C".into(),
        thread: "root".into(),
        ts: format!("{n}.000"),
        revision: format!("{n}.000"),
        sender: format!("U{n}"),
        bot,
        text: format!("private source {n}"),
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
fn done() -> anyhow::Result<Decision> {
    Ok(Decision {
        decision: "speak".into(),
        text: "Original result".into(),
        reason: "done".into(),
        media: vec![],
    })
}
#[test]
fn peer_burst_shares_fifo_without_work_jobs_or_changed_authority() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    s.ingest("root", &message(1, false), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    for n in 2..=5 {
        s.ingest(&format!("e{n}"), &message(n, n != 3), true)
            .unwrap();
    }
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    for n in 2..=5 {
        let item = steering::next(&s, &root.context, &root.id)
            .unwrap()
            .unwrap();
        assert_eq!(item.message.ts, message(n, false).ts);
        assert_eq!(item.awareness, n != 3);
        assert!(steering::submit(&s, &item, &root.id, "session").unwrap());
        steering::injected(&s, &item.id, &json!({})).unwrap();
        steering::receipt(
            &s,
            &root.id,
            &json!({"inputId":item.id,"state":"incorporated"}),
        )
        .unwrap();
    }
    s.finish(&root, done(), &attention()).unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let trace =
        s.db.query_row(
            "SELECT data FROM audit WHERE kind='turn_context_coverage' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert!(!trace.contains("private source"));
    assert!(trace.contains("awareness"));
    assert!(trace.contains("incorporated"));
}
#[test]
fn idle_unknown_and_completion_race_never_promote_peer_awareness() {
    for result in ["promptRequired", "unknown", "unsupported", "terminalRace"] {
        let home = tempfile::tempdir().unwrap();
        let mut s = Store::open(home.path()).unwrap();
        s.ingest("root", &message(1, false), true).unwrap();
        let root = s.claim(&attention()).unwrap().unwrap();
        s.ingest("peer", &message(2, true), true).unwrap();
        let item = steering::next(&s, &root.context, &root.id)
            .unwrap()
            .unwrap();
        assert!(
            steering::next(&s, &root.context, "later-job")
                .unwrap()
                .is_none()
        );
        match result {
            "promptRequired" => {
                assert!(steering::submit(&s, &item, &root.id, "session").unwrap());
                steering::defer(&s, &item.id, &root.id).unwrap();
            }
            "unknown" => {
                assert!(steering::submit(&s, &item, &root.id, "session").unwrap());
                steering::fail(&s, &item.id, "unknown").unwrap();
            }
            "unsupported" => steering::history_only(&s, &item.id).unwrap(),
            _ => {}
        }
        s.finish(&root, done(), &attention()).unwrap();
        assert!(s.claim(&attention()).unwrap().is_none());
        assert_eq!(s.history(&root.trigger).unwrap().len(), 2);
        assert_eq!(
            s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let state: String =
            s.db.query_row("SELECT state FROM steering_inputs", [], |r| r.get(0))
                .unwrap();
        assert_eq!(
            state,
            if result == "unknown" {
                "unknown"
            } else {
                "historyOnly"
            }
        );
    }
}
#[test]
fn new_peer_before_delivery_preserves_result_and_traces_uncovered_revision() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    s.ingest("root", &message(1, false), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    s.ingest("late", &message(2, true), true).unwrap();
    s.finish(&root, done(), &attention()).unwrap();
    let mut c = Config::new("test".into(), "owner".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.slack.channels = vec!["C".into()];
    let out = s.claim_outgoing(&c).unwrap().unwrap();
    assert_eq!(out.text, "Original result");
    let data: String =
        s.db.query_row(
            "SELECT data FROM audit WHERE kind='turn_context_coverage' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(v["stage"], "delivery_claimed");
    assert_eq!(v["latest_message_id"], "2.000");
    assert_eq!(v["inputs"][0]["stage"], "historyOnly");
    assert_eq!(v["guaranteed_final_reconsideration"], false);
    assert_eq!(v["outbox_id"], out.id);
}
#[test]
fn controls_from_any_author_never_become_context_or_candidates() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let mut c = Config::new("test".into(), "owner".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.slack.team_id = "T".into();
    c.slack.channels = vec!["C".into()];
    for (n, kind) in ["acp_work_status", "acp_permission", "acp_question"]
        .iter()
        .enumerate()
    {
        let raw = json!({"type":"message","channel":"C","ts":format!("{n}.000"),"user":"Upeer","bot_id":"Bpeer","text":"Thinking","metadata":{"event_type":kind}});
        for e in [
            raw.clone(),
            json!({"type":"message","channel":"C","subtype":"message_changed","message":raw}),
            json!({"type":"message","channel":"C","subtype":"message_deleted","deleted_ts":format!("{n}.000"),"previous_message":raw}),
        ] {
            assert!(slack::normalize("T", "C", &e, None).is_none());
            slack::receive(&c, &mut s, &json!({"event_id":format!("e{n}"),"event":e})).unwrap();
        }
        slack::receive(&c,&mut s,&json!({"event_id":format!("delete{n}"),"event":{"type":"message","channel":"C","subtype":"message_deleted","deleted_ts":format!("{n}.000")}})).unwrap();
    }
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let human =
        json!({"type":"message","channel":"C","ts":"99.000","user":"Uhuman","text":"Thinking"});
    slack::receive(&c, &mut s, &json!({"event_id":"human","event":human})).unwrap();
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn unmentioned_peer_is_awareness_only_in_the_existing_reply_location() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    s.ingest("root", &message(1, false), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    let mut c = Config::new("test".into(), "owner".into());
    c.enabled = true;
    c.participation = "mentions".into();
    c.slack.team_id = "T".into();
    c.slack.bot_user_id = "Uself".into();
    c.slack.bot_id = "Bself".into();
    c.slack.channels = vec!["C".into()];
    for (n, author, thread) in [
        (2, "peer", "root"),
        (3, "peer", "elsewhere"),
        (4, "self", "root"),
    ] {
        slack::receive(&c,&mut s,&json!({"event_id":format!("event{n}"),"event":{"type":"message","channel":"C","thread_ts":thread,"ts":format!("{n}.000"),"user":format!("U{author}"),"bot_id":format!("B{author}"),"text":"Additional context, without a mention"}})).unwrap();
    }
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let input = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    assert!(input.awareness);
    assert_eq!(input.message.ts, "2.000");
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM steering_inputs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let mut changed = input.message.clone();
    changed.revision = "5.000".into();
    changed.deleted = true;
    s.ingest("retract", &changed, false).unwrap();
    assert!(!steering::submit(&s, &input, &root.id, "session").unwrap());
    assert!(
        steering::next(&s, &root.context, &root.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn permission_control_echo_still_confirms_human_ui_delivery() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    let mut c = Config::new("test".into(), "owner".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.slack.team_id = "T".into();
    c.slack.bot_user_id = "Uself".into();
    c.slack.bot_id = "Bself".into();
    c.slack.channels = vec!["C".into()];
    s.db.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,channel,thread,body) VALUES('card','job','context','session','Uhuman',1,0,9999999999999,'pending','C','root','{}')",[]).unwrap();
    let mut echo = json!({"event_id":"echo","event":{"type":"message","channel":"C","ts":"2.000","user":"Uself","bot_id":"Bself","client_msg_id":"card","text":"Human permission card","metadata":{"event_type":"acp_permission"}}});
    echo["event"]["user"] = "Upeer".into();
    echo["event"]["bot_id"] = "Bpeer".into();
    slack::receive(&c, &mut s, &echo).unwrap();
    assert!(
        s.db.query_row(
            "SELECT slack_ts FROM permissions WHERE id='card'",
            [],
            |r| r.get::<_, Option<String>>(0)
        )
        .unwrap()
        .is_none()
    );
    echo["event"]["user"] = "Uself".into();
    echo["event"]["bot_id"] = "Bself".into();
    slack::receive(&c, &mut s, &echo).unwrap();
    assert_eq!(
        s.db.query_row(
            "SELECT slack_ts FROM permissions WHERE id='card'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "2.000"
    );
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn explicit_remote_queue_needs_incorporation_before_final_completion() {
    for bot in [false, true] {
        for incorporated in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let mut s = Store::open(home.path()).unwrap();
            s.ingest("root", &message(1, false), true).unwrap();
            let root = s.claim(&attention()).unwrap().unwrap();
            s.ingest("update", &message(2, bot), true).unwrap();
            let input = steering::next(&s, &root.context, &root.id)
                .unwrap()
                .unwrap();
            assert!(steering::submit(&s, &input, &root.id, "session").unwrap());
            steering::receipt(
                &s,
                &root.id,
                &json!({"inputId":input.id,"state":"remoteQueued","turnId":"turn"}),
            )
            .unwrap();
            // A delayed transport acknowledgment cannot erase the explicit queue.
            steering::injected(
                &s,
                &input.id,
                &json!({"inputId":input.id,"state":"received"}),
            )
            .unwrap();
            steering::receipt(
                &s,
                &root.id,
                &json!({"inputId":input.id,"state":"received"}),
            )
            .unwrap();
            assert_eq!(
                s.db.query_row("SELECT state FROM steering_inputs", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "remoteQueued"
            );
            if incorporated {
                steering::receipt(
                    &s,
                    &root.id,
                    &json!({"inputId":input.id,"state":"incorporated"}),
                )
                .unwrap();
            }
            steering::complete(&s, &root.id).unwrap();
            s.finish(&root, done(), &attention()).unwrap();
            assert_eq!(
                s.db.query_row("SELECT state FROM steering_inputs", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                if incorporated {
                    "incorporated"
                } else {
                    "unknown"
                }
            );
            assert!(s.claim(&attention()).unwrap().is_none());
            assert_eq!(
                s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                if bot || incorporated { 1 } else { 2 }
            );
            if !incorporated {
                assert_eq!(
                    s.db.query_row("SELECT reason FROM steering_inputs", [], |r| r
                        .get::<_, String>(0))
                        .unwrap(),
                    "remote_queue_unincorporated_at_end"
                );
            }
        }
    }
}

#[test]
fn peer_count_overflow_is_history_only_without_consuming_human_capacity() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    s.ingest("root", &message(1, false), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    for n in 2..=258 {
        s.ingest(&format!("peer{n}"), &message(n, true), true)
            .unwrap();
    }
    assert_eq!(
        s.db.query_row(
            "SELECT COUNT(*) FROM steering_inputs WHERE state='queued'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        steering::MAX_PENDING_AWARENESS
    );
    assert_eq!(
        s.db.query_row(
            "SELECT reason FROM steering_inputs WHERE state='historyOnly'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "awareness_queue_capacity"
    );
    assert_eq!(
        s.message("C", "258.000").unwrap().unwrap().text,
        message(258, true).text
    );
    assert!(
        steering::status(&s).unwrap()["oldest_queued_age_ms"]
            .as_i64()
            .is_some()
    );
    s.ingest("human", &message(259, false), true).unwrap();
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let first = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    assert!(steering::submit(&s, &first, &root.id, "session").unwrap());
    steering::receipt(
        &s,
        &root.id,
        &json!({"inputId":first.id,"state":"incorporated"}),
    )
    .unwrap();
    s.ingest("new-peer", &message(260, true), true).unwrap();
    assert_eq!(
        s.db.query_row(
            "SELECT COUNT(*) FROM steering_inputs WHERE intent='awareness' AND state='queued'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        steering::MAX_PENDING_AWARENESS
    );
    assert_eq!(
        s.db.query_row(
            "SELECT COUNT(*) FROM steering_inputs WHERE state='historyOnly'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn peer_byte_limit_counts_utf8_bytes_and_already_admitted_unincorporated_input() {
    let home = tempfile::tempdir().unwrap();
    let mut s = Store::open(home.path()).unwrap();
    s.ingest("root", &message(1, false), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    for n in 2..=7 {
        let mut peer = message(n, true);
        peer.text = "é".repeat(400_000);
        s.ingest(&format!("peer{n}"), &peer, true).unwrap();
        if n == 2 {
            let first = steering::next(&s, &root.context, &root.id)
                .unwrap()
                .unwrap();
            assert!(steering::submit(&s, &first, &root.id, "session").unwrap());
            steering::injected(&s, &first.id, &json!({})).unwrap();
        }
    }
    let (count,bytes):(i64,i64)=s.db.query_row("SELECT COUNT(*),SUM(length(CAST(body AS BLOB))) FROM steering_inputs WHERE state IN ('queued','received')",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(count, 5);
    assert!(bytes <= steering::MAX_PENDING_AWARENESS_BYTES);
    assert_eq!(s.db.query_row("SELECT COUNT(*) FROM steering_inputs WHERE state='historyOnly' AND reason='awareness_queue_capacity'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let trace: String =
        s.db.query_row(
            "SELECT data FROM audit WHERE kind='steering_delivery' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(trace.contains("awareness_queue_capacity"));
    assert!(!trace.contains('é'));
}
