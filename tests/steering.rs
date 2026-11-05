use acp_slack_adapter::{
    config::{Attention, Config},
    model::{Decision, Message},
    steering,
    store::Store,
};
use serde_json::json;
fn message(n: u64) -> Message {
    Message {
        team: "T".into(),
        channel: "C".into(),
        thread: "root".into(),
        ts: format!("{n}.000"),
        revision: format!("{n}.000"),
        sender: format!("U{n}"),
        bot: false,
        text: format!("update {n}"),
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
        text: "Done".into(),
        reason: "verified".into(),
        media: vec![],
    })
}
#[test]
fn durable_fifo_keeps_equal_text_distinct_and_deduplicates_source_revision() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    for n in 2..=4 {
        let mut m = message(n);
        m.text = "same text".into();
        s.ingest(&format!("e{n}"), &m, true).unwrap();
        assert!(!s.ingest(&format!("duplicate{n}"), &m, true).unwrap());
    }
    drop(s);
    let mut s = Store::open(d.path()).unwrap();
    for n in 2..=4 {
        let item = steering::next(&s, &root.context, &root.id)
            .unwrap()
            .unwrap();
        assert_eq!(item.message.ts, message(n).ts);
        assert!(steering::submit(&s, &item, &root.id, "session").unwrap());
        steering::injected(&s, &item.id, &json!({})).unwrap();
        steering::receipt(
            &s,
            &root.id,
            &json!({"inputId":item.id,"state":"incorporated","turnId":"turn"}),
        )
        .unwrap();
    }
    assert!(
        steering::next(&s, &root.context, &root.id)
            .unwrap()
            .is_none()
    );
    s.finish(&root, done(), &attention()).unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
    let mut c = Config::new("test".into(), "owner".into());
    c.enabled = true;
    c.participation = "ambient".into();
    c.slack.channels = vec!["C".into()];
    assert_eq!(s.claim_outgoing(&c).unwrap().unwrap().thread, "root");
}
#[test]
fn fallback_preserves_each_trigger_and_original_reply() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    for n in 2..=4 {
        s.ingest(&format!("e{n}"), &message(n), true).unwrap();
    }
    s.finish(&root, done(), &attention()).unwrap();
    for n in 2..=4 {
        let job = s.claim(&attention()).unwrap().unwrap();
        let job = s.prepare(job).unwrap();
        assert_eq!(job.trigger.ts, message(n).ts);
        s.finish(&job, done(), &attention()).unwrap();
    }
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
}
#[test]
fn unknown_submission_is_visible_scoped_and_never_replayed_after_restart() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    s.ingest("update", &message(2), true).unwrap();
    let update = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    steering::submit(&s, &update, &root.id, "session").unwrap();
    drop(s);
    let mut s = Store::open(d.path()).unwrap();
    s.recover().unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
    assert_eq!(
        s.db.query_row("SELECT state FROM steering_inputs", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "unknown"
    );
    let (channel, thread, text): (String, String, String) =
        s.db.query_row("SELECT channel,thread,text FROM outbox", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((channel.as_str(), thread.as_str()), ("C", "root"));
    assert!(text.contains("not sent again"));
    assert!(!text.contains("update 2"));
}
#[test]
fn receipt_scope_and_delayed_ack_cannot_regress_incorporation() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    s.ingest("update", &message(2), true).unwrap();
    let update = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    steering::submit(&s, &update, &root.id, "session").unwrap();
    steering::receipt(
        &s,
        "wrong-turn",
        &json!({"inputId":update.id,"state":"unknown"}),
    )
    .unwrap();
    steering::receipt(
        &s,
        &root.id,
        &json!({"inputId":update.id,"state":"incorporated"}),
    )
    .unwrap();
    steering::injected(&s, &update.id, &json!({})).unwrap();
    assert_eq!(
        s.db.query_row("SELECT state FROM steering_inputs", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "incorporated"
    );
}
#[test]
fn only_explicitly_restored_unexpired_attention_offer_is_reusable() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    s.ingest("update", &message(2), true).unwrap();
    let input = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    let offer = json!({"id":"offer","ticket":"private-ticket"});
    steering::save_attention(&s, &input.id, &offer).unwrap();
    steering::submit(&s, &input, &root.id, "session").unwrap();
    steering::defer(&s, &input.id, &root.id).unwrap();
    let until = acp_slack_adapter::model::now_ms() + 60000;
    steering::restore_attention(&s,&input.id,&json!({"_meta":{"com.microsoft.amplifier.attention":{"id":"wrong","ticketState":"restored","expiresAt":until}}})).unwrap();
    s.finish(&root, done(), &attention()).unwrap();
    let next = s.claim(&attention()).unwrap().unwrap();
    assert!(steering::restored_attention(&s, &next.id).is_err());
    steering::restore_attention(&s,&input.id,&json!({"_meta":{"com.microsoft.amplifier.attention":{"id":"offer","ticketState":"restored","expiresAt":until}}})).unwrap();
    assert_eq!(
        steering::restored_attention(&s, &next.id).unwrap(),
        Some(offer)
    );
    steering::save_attention(
        &s,
        &input.id,
        &json!({"id":"offer","ticket":"private-ticket","restoredUntil":1}),
    )
    .unwrap();
    assert!(steering::restored_attention(&s, &next.id).is_err());
    assert!(
        !steering::status(&s)
            .unwrap()
            .to_string()
            .contains("private-ticket")
    );
}
#[test]
fn pending_human_approval_survives_message_but_not_source_stop_or_revocation() {
    use acp_slack_adapter::{model::now_ms, permissions};
    for change in ["message", "source", "stopped", "policy", "paused"] {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path()).unwrap();
        let mut c = Config::new("fixture".into(), "owner".into());
        c.enabled = true;
        c.slack.team_id = "T".into();
        c.slack.channels = vec!["C".into()];
        let source = message(1);
        s.ingest("root", &source, true).unwrap();
        let root = s.claim(&attention()).unwrap().unwrap();
        let now = now_ms();
        s.db.execute("INSERT INTO permissions(id,job_id,context,session_id,approver,config_revision,created,expires,status,channel,thread,body) VALUES('approval',?1,?2,'session',?3,?4,?5,?6,'pending','C','root',?7)",rusqlite::params![root.id,root.context,source.sender,c.revision as i64,now,now+60000,json!({"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"}]}).to_string()]).unwrap();
        s.ingest("new-message", &message(2), true).unwrap();
        match change {
            "source" => {
                let mut edited = source.clone();
                edited.revision = "3.000".into();
                edited.text = "Changed request".into();
                s.ingest("edit", &edited, false).unwrap();
            }
            "stopped" => {
                s.db.execute(
                    "UPDATE jobs SET status='interrupted' WHERE id=?1",
                    [&root.id],
                )
                .unwrap();
            }
            "policy" => c.revision += 1,
            "paused" => c.enabled = false,
            _ => {}
        }
        let result = permissions::resolve(
            &s,
            &c,
            "approval",
            Some("once"),
            "authenticated administrator fixture",
            None,
        );
        assert_eq!(result.is_ok(), change == "message", "{change}");
        if change == "stopped" {
            s.finish(&root, done(), &attention()).unwrap();
            assert_eq!(
                s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
fn queue_is_conversation_local_and_cannot_submit_a_retracted_source() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    let mut other = message(2);
    other.thread = "other-thread".into();
    s.ingest("other-root", &other, true).unwrap();
    let other_job = s.claim(&attention()).unwrap().unwrap();
    s.ingest("update", &message(3), true).unwrap();
    assert!(
        steering::next(&s, &other_job.context, &other_job.id)
            .unwrap()
            .is_none()
    );
    let update = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    assert!(!steering::submit(&s, &update, &other_job.id, "other-session").unwrap());
    let mut retracted = message(3);
    retracted.revision = "4.000".into();
    retracted.deleted = true;
    s.ingest("retract", &retracted, false).unwrap();
    assert!(!steering::submit(&s, &update, &root.id, "session").unwrap());
}

#[test]
fn steering_burst_uses_one_lifecycle_capacity_slot_but_fallback_prompts_still_count() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    let mut limit = attention();
    limit.max_decisions_per_agent_per_hour = 2;
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&limit).unwrap().unwrap();
    for n in 2..=8 {
        s.ingest(&format!("event{n}"), &message(n), true).unwrap();
        let input = steering::next(&s, &root.context, &root.id)
            .unwrap()
            .unwrap();
        steering::submit(&s, &input, &root.id, "session").unwrap();
        if n == 8 {
            steering::fail(&s, &input.id, "unknown").unwrap();
        } else {
            steering::injected(&s, &input.id, &json!({})).unwrap();
        }
    }
    s.ingest("fallback", &message(9), true).unwrap();
    s.finish(&root, done(), &limit).unwrap();
    let fallback = s
        .claim(&limit)
        .unwrap()
        .expect("one prompt slot remains after the steering burst");
    assert_eq!(fallback.trigger.ts, message(9).ts);
    s.finish(&fallback, done(), &limit).unwrap();
    s.ingest("third-lifecycle", &message(10), true).unwrap();
    assert!(s.claim(&limit).unwrap().is_none());
}

#[test]
fn disconnected_attention_evaluation_is_not_blindly_reoffered() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    s.ingest("update", &message(2), true).unwrap();
    let input = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    steering::offering(&s, &input.id, &root.id, "session").unwrap();
    s.recover().unwrap();
    assert!(s.claim(&attention()).unwrap().is_none());
    assert_eq!(
        s.db.query_row("SELECT state FROM steering_inputs", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "unknown"
    );
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn material_edit_retires_old_queued_revision_instead_of_prompting_it_later() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path()).unwrap();
    s.ingest("root", &message(1), true).unwrap();
    let root = s.claim(&attention()).unwrap().unwrap();
    s.ingest("update", &message(2), true).unwrap();
    let original = steering::next(&s, &root.context, &root.id)
        .unwrap()
        .unwrap();
    let mut edited = message(2);
    edited.revision = "3.000".into();
    edited.text = "Replacement guidance".into();
    s.ingest("edit", &edited, true).unwrap();
    assert_eq!(
        s.db.query_row("SELECT status FROM jobs WHERE id=?1", [original.id], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "superseded"
    );
    s.db.execute(
        "UPDATE jobs SET status='interrupted' WHERE id=?1",
        [root.id],
    )
    .unwrap();
    let next = s.claim(&attention()).unwrap().unwrap();
    assert_eq!(next.trigger.revision, edited.revision);
    assert_eq!(next.trigger.text, edited.text);
}
