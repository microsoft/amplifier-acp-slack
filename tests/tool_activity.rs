use acp_slack_adapter::work_status::ToolActivity;
use serde_json::json;

#[test]
fn partial_updates_keep_labels_and_completion_reveals_other_active_tools() {
    let mut activity = ToolActivity::default();
    assert_eq!(activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"a","title":"Run unit tests","name":"workspace_run","kind":"execute","status":"pending"})).unwrap(), "Starting · Run unit tests (workspace_run)");
    assert_eq!(
        activity
            .update(
                &json!({"sessionUpdate":"tool_call_update","toolCallId":"a","status":"in_progress"})
            )
            .unwrap(),
        "Running · Run unit tests (workspace_run)"
    );
    assert_eq!(activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"b","title":"Read documentation","kind":"read","status":"in_progress"})).unwrap(), "Reading · Read documentation · +1 tool");
    assert_eq!(
        activity
            .update(
                &json!({"sessionUpdate":"tool_call_update","toolCallId":"b","status":"completed"})
            )
            .unwrap(),
        "Running · Run unit tests (workspace_run)"
    );
    assert_eq!(
        activity
            .update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"a","status":"failed"}))
            .unwrap(),
        "Checking a tool failure…"
    );
    assert!(activity.update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"unknown","status":"completed"})).is_none());
}

#[test]
fn display_is_bounded_and_never_uses_arguments_outputs_or_reasoning() {
    let mut activity = ToolActivity::default();
    let line = activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"a","title":"Read\n\tproject\u{202e} notes","kind":"read","status":"in_progress","rawInput":{"password":"PRIVATE_ARGUMENT"},"rawOutput":"PRIVATE_RESULT","content":[{"type":"text","text":"PRIVATE_CONTENT"}]})).unwrap();
    assert_eq!(line, "Reading · Read project notes");
    let line = activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"b","title":"é".repeat(1000),"name":"x".repeat(100),"kind":"execute","status":"in_progress"})).unwrap();
    assert!(line.chars().count() <= 180);
    assert!(line.ends_with(" · +1 tool"));
    let line = activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"c","title":"PRIVATE_REASONING","kind":"think","status":"in_progress"})).unwrap();
    assert_eq!(line, "Thinking… · +2 tools");
}

#[test]
fn empty_labels_and_duplicate_names_have_useful_fallbacks() {
    let mut activity = ToolActivity::default();
    assert_eq!(activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"a","title":"workspace run","name":"workspace_run","kind":"execute","status":"in_progress"})).unwrap(), "Running · workspace run");
    assert_eq!(
        activity
            .update(
                &json!({"sessionUpdate":"tool_call_update","toolCallId":"a","status":"completed"})
            )
            .unwrap(),
        "Thinking…"
    );
    assert_eq!(activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"b","kind":"search","status":"in_progress"})).unwrap(), "Searching…");
}

#[test]
fn producer_present_tense_title_is_not_repeated() {
    let mut activity = ToolActivity::default();
    assert_eq!(activity.update(&json!({"sessionUpdate":"tool_call","toolCallId":"a","title":"Reading README.md","kind":"read","status":"in_progress"})).unwrap(), "Reading README.md");
}

#[test]
fn explicit_public_commentary_is_bounded_but_private_thoughts_and_arguments_are_never_status() {
    let mut progress = acp_slack_adapter::work_status::PublicProgress::default();
    let mut event = serde_json::json!({"sessionUpdate":"agent_thought_chunk","content":{"text":"private reasoning canary"},"_meta":{"com.microsoft.amplifier.message":{"phase":"commentary"}}});
    assert!(progress.update(&event).is_none());
    event["sessionUpdate"] = "tool_call".into();
    assert!(progress.update(&event).is_none());
    event["sessionUpdate"] = "agent_message_chunk".into();
    event["messageId"] = "public-1".into();
    event["content"]["text"] = "Checking the repository\nfor the reported issue.".into();
    assert_eq!(
        progress.update(&event).unwrap(),
        "Checking the repository for the reported issue."
    );
    assert!(
        progress.update(&event).is_none(),
        "rapid chunks are coalesced"
    );
    event["messageId"] = "public-2".into();
    event["content"]["text"] = serde_json::json!("x".repeat(1000));
    assert_eq!(progress.update(&event).unwrap().chars().count(), 180);
    event["_meta"]["com.microsoft.amplifier.message"]["phase"] = "final_answer".into();
    assert!(progress.update(&event).is_none());
    assert_eq!(
        progress
            .update(
                &serde_json::json!({"sessionUpdate":"compaction_update","status":"in_progress"})
            )
            .unwrap(),
        "Compacting conversation…"
    );
}
