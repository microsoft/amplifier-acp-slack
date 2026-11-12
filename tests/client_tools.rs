use acp_slack_adapter::{client_tools::ClientTools, config::Config};
#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
fn setup() -> (tempfile::TempDir, Config, ClientTools) {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir(t.path().join("adapter")).unwrap();
    std::fs::create_dir(t.path().join("workspace")).unwrap();
    let mut c = Config::new("Client tools".into(), "owner".into());
    c.acp.cwd = t.path().join("workspace").to_str().unwrap().into();
    c.acp.client.workspace_roots = vec![c.acp.cwd.clone()];
    c.acp.client.read_text_file = true;
    c.acp.client.write_text_file = true;
    c.acp.client.terminal = cfg!(target_os = "linux");
    c.acp.client.terminal_timeout_seconds = 2;
    let tools = ClientTools::new(&c, &t.path().join("adapter"), "session").unwrap();
    (t, c, tools)
}
#[cfg(unix)]
#[tokio::test]
async fn bounded_files_and_paths() {
    let (t, c, tools) = setup();
    let path = format!("{}/sub/example.txt", c.acp.cwd);
    tools
        .call(
            "fs/write_text_file",
            &json!({"sessionId":"session","path":path,"content":"first\nsecond\nthird\n"}),
        )
        .await
        .unwrap();
    assert_eq!(
        tools
            .call(
                "fs/read_text_file",
                &json!({"sessionId":"session","path":path,"line":2,"limit":1})
            )
            .await
            .unwrap()["content"],
        "second\n"
    );
    assert!(
        tools
            .call(
                "fs/read_text_file",
                &json!({"sessionId":"wrong","path":path})
            )
            .await
            .is_err()
    );
    for path in [
        t.path().join("adapter/state.sqlite3"),
        t.path().join("workspace/../adapter/state.sqlite3"),
    ] {
        assert!(
            tools
                .call(
                    "fs/read_text_file",
                    &json!({"sessionId":"session","path":path})
                )
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(t.path().join("adapter"), t.path().join("workspace/escape"))
            .unwrap();
        assert!(tools.call("fs/write_text_file",&json!({"sessionId":"session","path":t.path().join("workspace/escape/no.txt"),"content":"bad"})).await.is_err());
        assert!(!t.path().join("adapter/no.txt").exists());
    }
}
#[cfg(unix)]
#[tokio::test]
async fn capabilities_are_opt_in_and_cannot_expose_adapter_home() {
    let (t, mut c, _) = setup();
    c.acp.client.workspace_roots = vec![t.path().to_str().unwrap().into()];
    assert!(ClientTools::new(&c, &t.path().join("adapter"), "session").is_err());
    c.acp.client = Default::default();
    let tools = ClientTools::new(&c, &t.path().join("adapter"), "session").unwrap();
    assert!(
        tools
            .call(
                "fs/read_text_file",
                &json!({"sessionId":"session","path":format!("{}/example",c.acp.cwd)})
            )
            .await
            .is_err()
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn terminal_isolation_limits_kill_and_release() {
    let (_t, c, tools) = setup();
    let started=tools.call("terminal/create",&json!({"sessionId":"session","command":"/bin/sh","args":["-c","printf 'one two three'; test ! -e /etc/passwd && test ! -e /home/operator/.codex/auth.json"],"cwd":c.acp.cwd,"outputByteLimit":5})).await.unwrap();
    let params = json!({"sessionId":"session","terminalId":started["terminalId"]});
    let exit = tools.call("terminal/wait_for_exit", &params).await.unwrap();
    let output = tools.call("terminal/output", &params).await.unwrap();
    assert_eq!(exit["exitCode"], 0, "{output}");
    assert_eq!(output["output"], "three");
    assert_eq!(output["truncated"], true);
    tools.call("terminal/release", &params).await.unwrap();
    assert!(tools.call("terminal/output", &params).await.is_err());
    let started = tools
        .call(
            "terminal/create",
            &json!({"sessionId":"session","command":"/bin/sleep","args":["20"]}),
        )
        .await
        .unwrap();
    let params = json!({"sessionId":"session","terminalId":started["terminalId"]});
    tools.call("terminal/kill", &params).await.unwrap();
    let exit = tools.call("terminal/wait_for_exit", &params).await.unwrap();
    assert_ne!(exit["exitCode"], Value::from(0));
    assert!(
        tools
            .call(
                "terminal/create",
                &json!({"sessionId":"session","command":"/bin/true","cwd":"/"})
            )
            .await
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn read_only_terminal_and_lifetime_are_enforced() {
    let (t, mut c, _) = setup();
    c.acp.client.write_text_file = false;
    c.acp.client.terminal_timeout_seconds = 1;
    let tools = ClientTools::new(&c, &t.path().join("adapter"), "session").unwrap();
    let started=tools.call("terminal/create",&json!({"sessionId":"session","command":"/bin/sh","args":["-c",format!("printf forbidden > {}/forbidden.txt",c.acp.cwd)]})).await.unwrap();
    let params = json!({"sessionId":"session","terminalId":started["terminalId"]});
    assert_ne!(
        tools.call("terminal/wait_for_exit", &params).await.unwrap()["exitCode"],
        0
    );
    assert!(!t.path().join("workspace/forbidden.txt").exists());
    let started = tools
        .call(
            "terminal/create",
            &json!({"sessionId":"session","command":"/bin/sleep","args":["30"]}),
        )
        .await
        .unwrap();
    let params = json!({"sessionId":"session","terminalId":started["terminalId"]});
    let exit = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        tools.call("terminal/wait_for_exit", &params),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(exit["exitCode"], 0);
}

#[cfg(windows)]
#[test]
fn windows_does_not_advertise_unavailable_filesystem_or_terminal() {
    let mut c = Config::new("Native".into(), "owner".into());
    c.acp.client.read_text_file = true;
    c.acp.client.write_text_file = true;
    c.acp.client.terminal = true;
    assert_eq!(
        acp_slack_adapter::client_tools::advertised(&c)["fs"]["readTextFile"],
        false
    );
    assert_eq!(
        acp_slack_adapter::client_tools::advertised(&c)["terminal"],
        false
    );
    assert!(ClientTools::new(&c, std::path::Path::new("unused"), "session").is_err());
}
