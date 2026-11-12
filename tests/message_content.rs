use acp_slack_adapter::{config::Attention, slack, store::Store};
use serde_json::{Value, json};

fn table_message() -> Value {
    json!({"type":"message","ts":"1000.000001","user":"UPEER","bot_id":"BPEER",
    "text":"Saved results. No new run is needed.",
    "blocks":[
        {"type":"rich_text","elements":[{"type":"rich_text_section","elements":[
            {"type":"text","text":"Saved results. No new run is needed."}]}]},
        {"type":"table","rows":[
            [{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"File"}]}]}, {"type":"raw_text","text":"sha256"}],
            [{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"clip.mp4","style":{"code":true}}]}]}, {"type":"raw_text","text":"fixture-sha256"}]
        ]},
        {"type":"table","rows":[[{"type":"raw_text","text":"Seek"},{"type":"raw_number","value":4.5}]]}
    ]})
}

#[test]
fn incoming_tables_preserve_values_without_repeating_fallback_prose() {
    let raw = table_message();
    let m = slack::normalize("T", "C", &raw, None).unwrap();
    assert!(m.text.starts_with(raw["text"].as_str().unwrap()));
    assert_eq!(m.text.matches("Saved results.").count(), 1);
    assert!(m.text.contains("| clip.mp4 | fixture-sha256 |"));
    assert!(m.text.contains("Seek") && m.text.contains("4.5"));
    assert_eq!(m.ts, "1000.000001");
    assert_eq!(m.revision, m.ts);
    assert_eq!(m.sender, "UPEER");
}

#[test]
fn edited_tables_follow_existing_revision_and_delete_rules() {
    let raw = table_message();
    let before = slack::normalize("T", "C", &raw, None).unwrap();
    let mut changed = raw.clone();
    changed["edited"] = json!({"ts":"1001.000001"});
    changed["blocks"][1]["rows"][1][1]["text"] = json!("revised-sha256");
    let event = json!({"subtype":"message_changed","message":changed});
    let after = slack::normalize("T", "C", &event, Some(before.clone())).unwrap();
    assert!(after.text.contains("revised-sha256"));
    assert!(!after.text.contains("fixture-sha256"));
    assert_eq!(after.revision, "1001.000001");
    let deleted = slack::normalize(
        "T",
        "C",
        &json!({"subtype":"message_deleted","deleted_ts":before.ts,"event_ts":"1002.000001"}),
        Some(after),
    )
    .unwrap();
    assert!(deleted.deleted && deleted.text.is_empty());
}

#[test]
fn richer_same_revision_after_restart_never_rewrites_source_or_replays_job() {
    let home = tempfile::tempdir().unwrap();
    let raw = table_message();
    let mut legacy = slack::normalize("T", "C", &raw, None).unwrap();
    legacy.text = raw["text"].as_str().unwrap().into();
    {
        let mut s = Store::open(home.path()).unwrap();
        assert!(s.ingest("old", &legacy, false).unwrap());
    }
    let mut s = Store::open(home.path()).unwrap();
    let enriched = slack::normalize("T", "C", &raw, None).unwrap();
    assert!(enriched.text.contains("fixture-sha256"));
    assert!(!s.ingest("new-hydration", &enriched, true).unwrap());
    assert_eq!(
        serde_json::to_value(s.history(&legacy).unwrap()).unwrap(),
        json!([legacy])
    );
    assert!(
        s.claim(&Attention {
            settle_ms: 0,
            ..Attention::default()
        })
        .unwrap()
        .is_none()
    );
    assert_eq!(
        s.db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        s.db.query_row("SELECT version FROM contexts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn attachment_tables_keep_boundaries_links_unknown_cells_and_exact_duplicate_rows() {
    let raw = json!({"ts":"1000.000001","user":"UPEER","text":"Existing table:\n| Already | present |",
    "blocks":[{"type":"table","rows":[[{"type":"raw_text","text":"Already"},{"type":"raw_text","text":"present"}]]}],
    "attachments":[{"blocks":[{"type":"table","rows":[[
        {"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"link","text":"Evidence","url":"https://example.com/proof"}]}]},
        {"type":"raw_text","text":"a|b\nc\\d"},
        {"type":"future_cell","opaque":{"source":"retained"}},
        {"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"user","user_id":"U123"}]}]}
    ]]}]}]});
    let m = slack::normalize("T", "C", &raw, None).unwrap();
    assert_eq!(m.text.matches("| Already | present |").count(), 1);
    assert!(m.text.contains("Evidence (https://example.com/proof)"));
    assert!(m.text.contains("a\\|b\\nc\\\\d"));
    assert!(m.text.contains("\"opaque\":{\"source\":\"retained\"}"));
    assert!(m.text.contains("\"user_id\":\"U123\""));
    assert_eq!(m.text.matches("[Slack table]").count(), 1);
}

#[test]
fn no_table_keeps_original_fallback_exactly() {
    let raw = json!({"ts":"1000.000001","user":"UPEER","text":"  Source\nwith <@U123> and spaces.  ","blocks":[{"type":"rich_text","elements":[{"type":"text","text":"duplicate prose"}]}]});
    assert_eq!(
        slack::normalize("T", "C", &raw, None).unwrap().text,
        raw["text"].as_str().unwrap()
    );
}

#[test]
fn identical_native_tables_are_distinct_occurrences() {
    let table = json!({"type":"table","rows":[[{"type":"raw_text","text":"same result"}]]});
    let raw = json!({"ts":"1000.000001","user":"UPEER","text":"Two independently recorded results.","blocks":[table.clone(),table]});
    let m = slack::normalize("T", "C", &raw, None).unwrap();
    assert_eq!(m.text.matches("| same result |").count(), 2);
    assert_eq!(m.text.matches("[Slack table]").count(), 2);
}
