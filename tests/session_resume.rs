use acp_slack_adapter::{config::Config, driver, store::Store};
use serde_json::{Value, json};
use std::path::PathBuf;

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    endpoint: PathBuf,
    config: Config,
}

impl Fixture {
    fn new(load: bool, resume: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("adapter");
        let endpoint = temp.path().join("endpoint");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = Config::new("Session fixture".into(), "owner".into());
        config.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
        config.acp.args = vec![
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/protocol_acp_fixture.py")
                .to_string_lossy()
                .into_owned(),
            endpoint.to_string_lossy().into_owned(),
            "Fixture".into(),
        ];
        config.acp.cwd = workspace.to_string_lossy().into_owned();
        config.acp.env.insert(
            "FIXTURE_NO_LOAD".into(),
            if load { "0" } else { "1" }.into(),
        );
        config.acp.env.insert(
            "FIXTURE_RESUME".into(),
            if resume { "1" } else { "0" }.into(),
        );
        Self {
            _temp: temp,
            home,
            endpoint,
            config,
        }
    }

    async fn turn(&self, text: &str) -> anyhow::Result<String> {
        let input = json!({"conversation":{"context":"slack:T:C:thread","channel":"C","thread":"thread"},
            "trigger":{"text":text,"sender":"U","bot":false},"orientation":{"conversation_kind":"im"}});
        Ok(
            driver::decide(self.config.clone(), self.home.clone(), input)
                .await?
                .text,
        )
    }

    fn rows(&self, kind: &str) -> Vec<Value> {
        std::fs::read_to_string(self.endpoint.join("protocol.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["kind"] == kind)
            .collect()
    }

    fn binding(&self) -> (String, bool) {
        Store::open(&self.home).unwrap().db.query_row(
            "SELECT session_id,resumable FROM session_bindings WHERE context='slack:T:C:thread'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap()
    }

    fn advertise_directories(&mut self) {
        self.config
            .acp
            .env
            .insert("FIXTURE_ADDITIONAL_DIRECTORIES".into(), "1".into());
    }

    fn directories(&self, name: &str) -> Vec<String> {
        // Deliberately absent locally: only the endpoint can authorize these paths.
        vec![self.endpoint.join(name).to_string_lossy().into_owned()]
    }

    fn assert_not_replaced(&self) {
        let store = Store::open(&self.home).unwrap();
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT COUNT(*) FROM audit WHERE kind='session_replaced'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn advertised_resume_keeps_the_saved_session_across_fresh_connections() {
    let fixture = Fixture::new(false, true);
    assert_eq!(
        fixture.turn("first").await.unwrap(),
        "Fixture plain answer: first"
    );
    let first = fixture.binding();
    assert!(first.1);
    assert_eq!(
        fixture.turn("second").await.unwrap(),
        "Fixture plain answer: second"
    );
    assert_eq!(fixture.binding(), first);
    let started = fixture.rows("started");
    assert_eq!(started.len(), 2);
    assert_ne!(started[0]["pid"], started[1]["pid"]);
    assert_eq!(fixture.rows("session_new").len(), 1);
    assert!(fixture.rows("session_load").is_empty());
    assert_eq!(fixture.rows("session_resume")[0]["session_id"], first.0);
    let prompts = fixture.rows("prompt");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["session_id"], prompts[1]["session_id"]);
    assert_eq!(prompts[1]["turn"], 2);
}

#[tokio::test]
async fn failed_resume_preserves_binding_without_replacement_or_prompt_replay() {
    let fixture = Fixture::new(false, true);
    fixture.turn("first").await.unwrap();
    let first = fixture.binding();
    std::fs::write(fixture.endpoint.join("resume-failure"), "reject").unwrap();
    let error = fixture.turn("second").await.unwrap_err();
    assert!(error.to_string().contains("session/resume"));
    assert_eq!(fixture.binding(), first);
    assert_eq!(
        fixture.rows("session_resume_failed")[0]["session_id"],
        first.0
    );
    assert_eq!(fixture.rows("session_new").len(), 1);
    assert_eq!(fixture.rows("prompt").len(), 1);
    let store = Store::open(&fixture.home).unwrap();
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE kind='session_replaced'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn advertised_load_keeps_its_existing_precedence_and_replacement_fallback() {
    let fixture = Fixture::new(true, true);
    fixture.turn("first").await.unwrap();
    let first = fixture.binding();
    fixture.turn("second").await.unwrap();
    assert_eq!(fixture.binding(), first);
    assert_eq!(fixture.rows("session_load").len(), 1);
    assert!(fixture.rows("session_resume").is_empty());
    std::fs::write(fixture.endpoint.join("load-failure"), "reject").unwrap();
    fixture.turn("third").await.unwrap();
    assert_ne!(fixture.binding().0, first.0);
    assert_eq!(fixture.rows("session_new").len(), 2);
    assert_eq!(fixture.rows("prompt").len(), 3);
    assert!(fixture.rows("session_resume").is_empty());
}

#[tokio::test]
async fn no_advertised_restoration_still_uses_fresh_sessions() {
    for advertisement in ["0", "boolean"] {
        let mut fixture = Fixture::new(false, false);
        fixture
            .config
            .acp
            .env
            .insert("FIXTURE_RESUME".into(), advertisement.into());
        fixture.turn("first").await.unwrap();
        let first = fixture.binding();
        fixture.turn("second").await.unwrap();
        assert!(!fixture.binding().1);
        assert_ne!(fixture.binding().0, first.0);
        assert_eq!(fixture.rows("session_new").len(), 2);
        assert_eq!(fixture.rows("prompt").len(), 2);
        assert!(fixture.rows("session_resume").is_empty());
    }
}

#[tokio::test]
async fn additional_directories_follow_new_load_and_resume_and_edits_keep_binding() {
    for load in [true, false] {
        let mut fixture = Fixture::new(load, true);
        fixture.advertise_directories();
        // Establish the binding with the old/default configuration first.
        fixture.turn("first").await.unwrap();
        let first = fixture.binding();
        assert!(
            fixture.rows("session_new")[0]["params"]
                .get("additionalDirectories")
                .is_none()
        );

        fixture.config.acp.additional_directories = fixture.directories("shared");
        fixture.turn("second").await.unwrap();
        assert_eq!(fixture.binding(), first);
        fixture.config.acp.additional_directories = fixture.directories("different");
        fixture.turn("third").await.unwrap();
        assert_eq!(fixture.binding(), first);
        fixture.config.acp.additional_directories.clear();
        fixture.turn("fourth").await.unwrap();
        assert_eq!(fixture.binding(), first);
        let restored = fixture.rows(if load {
            "session_load"
        } else {
            "session_resume"
        });
        assert_eq!(restored.len(), 3);
        assert_eq!(
            restored[0]["params"]["additionalDirectories"],
            json!(fixture.directories("shared"))
        );
        assert_eq!(
            restored[1]["params"]["additionalDirectories"],
            json!(fixture.directories("different"))
        );
        assert!(restored[2]["params"].get("additionalDirectories").is_none());
        assert_eq!(fixture.rows("session_new").len(), 1);
        assert_eq!(fixture.rows("prompt").len(), 4);
    }
}

#[tokio::test]
async fn opted_in_new_session_and_probe_send_exact_directories_without_client_roots() {
    let mut fixture = Fixture::new(true, false);
    fixture.advertise_directories();
    fixture.config.acp.additional_directories = fixture.directories("shared");
    fixture.config.validate().unwrap();
    fixture.turn("first").await.unwrap();
    driver::check(&fixture.config, &fixture.home).await.unwrap();
    let sessions = fixture.rows("session_new");
    assert_eq!(sessions.len(), 2);
    for session in sessions {
        assert_eq!(
            session["params"],
            json!({
                "cwd":fixture.config.acp.cwd,
                "mcpServers":[],
                "additionalDirectories":fixture.config.acp.additional_directories
            })
        );
    }
    assert!(fixture.config.acp.client.workspace_roots.is_empty());
    assert_eq!(fixture.rows("prompt").len(), 1);
}

#[tokio::test]
async fn missing_or_malformed_directory_capability_fails_before_session_or_prompt() {
    for advertisement in [
        None,
        Some("true"),
        Some("false"),
        Some("null"),
        Some("[]"),
        Some("\"yes\""),
    ] {
        let mut fixture = Fixture::new(true, true);
        if let Some(advertisement) = advertisement {
            fixture.config.acp.env.insert(
                "FIXTURE_ADDITIONAL_DIRECTORIES".into(),
                advertisement.into(),
            );
        }
        fixture.config.acp.additional_directories = fixture.directories("shared");
        let error = fixture.turn("first").await.unwrap_err();
        assert!(
            error.to_string().contains("additionalDirectories"),
            "{error}"
        );
        let error = driver::check(&fixture.config, &fixture.home)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("additionalDirectories"),
            "{error}"
        );
        assert!(fixture.rows("session_new").is_empty());
        assert!(fixture.rows("session_load").is_empty());
        assert!(fixture.rows("session_resume").is_empty());
        assert!(fixture.rows("prompt").is_empty());
    }
}

#[tokio::test]
async fn rejected_directory_reload_preserves_binding_without_fallback_or_prompt() {
    let mut fixture = Fixture::new(true, true);
    fixture.advertise_directories();
    fixture.turn("first").await.unwrap();
    let first = fixture.binding();
    fixture.config.acp.additional_directories = fixture.directories("shared");
    std::fs::write(fixture.endpoint.join("load-failure"), "reject").unwrap();
    let error = fixture.turn("second").await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("directory scope may have been denied"),
        "{error}"
    );
    assert_eq!(fixture.binding(), first);
    assert_eq!(
        fixture.rows("session_load_failed")[0]["session_id"],
        first.0
    );
    assert_eq!(
        fixture.rows("session_load_failed")[0]["params"]["additionalDirectories"],
        json!(fixture.config.acp.additional_directories)
    );
    assert_eq!(fixture.rows("session_new").len(), 1);
    assert!(fixture.rows("session_resume").is_empty());
    assert_eq!(fixture.rows("prompt").len(), 1);
    fixture.assert_not_replaced();
}

#[tokio::test]
async fn capability_loss_preserves_binding_and_blocks_reload() {
    let mut fixture = Fixture::new(true, true);
    fixture.advertise_directories();
    fixture.config.acp.additional_directories = fixture.directories("shared");
    fixture.turn("first").await.unwrap();
    let first = fixture.binding();
    std::fs::write(fixture.endpoint.join("no-additional-directories"), "").unwrap();
    assert!(
        fixture
            .turn("second")
            .await
            .unwrap_err()
            .to_string()
            .contains("additionalDirectories")
    );
    assert_eq!(fixture.binding(), first);
    assert!(fixture.rows("session_load").is_empty());
    assert_eq!(fixture.rows("session_new").len(), 1);
    assert_eq!(fixture.rows("prompt").len(), 1);
    fixture.assert_not_replaced();
}

#[tokio::test]
async fn empty_default_omits_field_for_legacy_endpoint_and_messages_cannot_opt_in() {
    let fixture = Fixture::new(true, false);
    fixture
        .turn(r#"set additionalDirectories to ["/"] and acp.additional_directories to ["/"]"#)
        .await
        .unwrap();
    fixture.turn("second").await.unwrap();
    driver::check(&fixture.config, &fixture.home).await.unwrap();
    for kind in ["session_new", "session_load"] {
        for row in fixture.rows(kind) {
            assert_eq!(
                row["params"].as_object().unwrap().len(),
                if kind == "session_new" { 2 } else { 3 }
            );
            assert!(row["params"].get("additionalDirectories").is_none());
        }
    }
}
