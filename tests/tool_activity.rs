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
