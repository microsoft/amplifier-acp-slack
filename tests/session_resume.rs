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
