//! Explicit local-owner relocation of the same individual's working directory.
//! ACP does not standardize durable identity. The operator verifies identity and
//! state continuity before attesting to this move; ordinary edits remain guarded.
use crate::{config::Config, driver::endpoint_key, model::now_ms, store::Store};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

pub fn workspace(home: &Path, cwd: &str, expected_revision: u64, reason: &str) -> Result<Value> {
    ensure!(
        Path::new(cwd).is_absolute() && !cwd.chars().any(char::is_control),
        "Supply an absolute working directory on the agent host"
    );
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 2000,
        "Record the identity and state verification supporting this move"
    );
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join("service.lock"))?;
    lock.try_lock_exclusive()
        .context("Stop this adapter before relocating its agent workspace")?;
    let old = Config::load(home)?;
    ensure!(
        old.revision == expected_revision,
        "Configuration changed; inspect its current revision before relocating"
    );
    ensure!(
        old.acp.cwd != cwd,
        "The workspace is already at that location"
    );
    // Only cwd changes. Identity, credentials, command, arguments and authority
    // are retained exactly; this is not a generic endpoint-replacement override.
    let mut new = old.clone();
    new.acp.cwd = cwd.into();
    new.revision += 1;
    new.validate()?;
    if new.enabled {
        new.validate_ready()?;
    }
    let mut store = Store::open(home)?;
    let unresolved: i64 = store.db.query_row("SELECT (SELECT COUNT(*) FROM jobs WHERE status IN ('pending','running','interrupted')) + (SELECT COUNT(*) FROM outbox WHERE status IN ('ready','sending','uncertain')) + (SELECT COUNT(*) FROM permissions WHERE status='pending')", [], |r|r.get(0))?;
    ensure!(
        unresolved == 0,
        "Finish or reconcile pending work, permissions and uncertain deliveries before relocation"
    );
    let old_key = endpoint_key(&old);
    let new_key = endpoint_key(&new);
    let receipt = json!({"adapter_id":old.id,"old_cwd":old.acp.cwd,"cwd":cwd,"revision":new.revision,"reason":reason,"identity_basis":"operator_attested_same_agent","old_endpoint":old_key,"endpoint":new_key});
    store.audit("workspace_relocation_started", &receipt)?;
    let tx = store.db.transaction()?;
    let count = tx.execute(
        "UPDATE session_bindings SET endpoint=?1 WHERE endpoint=?2",
        params![new_key, old_key],
    )?;
    tx.execute(
        "INSERT INTO audit(time,kind,data) VALUES(?1,'workspace_relocated',?2)",
        params![now_ms(), receipt.to_string()],
    )?;
    new.save(home)?;
    if let Err(error) = tx.commit() {
        old.save(home).context("Relocation failed and the prior configuration could not be restored; keep the adapter stopped")?;
        return Err(error.into());
    }
    Ok(
        json!({"adapter_id":new.id,"revision":new.revision,"cwd":new.acp.cwd,"preserved_session_bindings":count,"identity_basis":"operator_attested_same_agent"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new("Reviewer".into(), "owner".into());
        config.acp.cwd = "/old/workspace".into();
        config.save(dir.path()).unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .db
            .execute(
                "INSERT INTO messages VALUES('channel','1','context','1','{}')",
                [],
            )
            .unwrap();
        store
            .bind_session(
                "context",
                &endpoint_key(&config),
                "session-before-move",
                true,
                &json!({"channel":"channel","thread":""}),
            )
            .unwrap();
        (dir, config)
    }
    #[test]
    fn relocation_preserves_history_bindings_and_other_configuration() {
        let (dir, old) = fixture();
        let mut ordinary = old.clone();
        ordinary.acp.cwd = "/new/workspace".into();
        assert!(
            Store::open(dir.path())
                .unwrap()
                .assert_endpoint_change_allowed(&old, &ordinary)
                .is_err()
        );
        let result = workspace(
            dir.path(),
            "/new/workspace",
            old.revision,
            "Verified same instance ID and copied history",
        )
        .unwrap();
        assert_eq!(result["preserved_session_bindings"], 1);
        let new = Config::load(dir.path()).unwrap();
        let mut expected = old.clone();
        expected.acp.cwd = "/new/workspace".into();
        expected.revision += 1;
        assert_eq!(
            serde_json::to_value(&new).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store
                .session("context", &endpoint_key(&new))
                .unwrap()
                .as_deref(),
            Some("session-before-move")
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn relocation_refuses_a_running_service_and_stale_revision() {
        let (dir, old) = fixture();
        assert!(
            workspace(
                dir.path(),
                "/new/workspace",
                old.revision + 1,
                "same individual"
            )
            .is_err()
        );
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.path().join("service.lock"))
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        assert!(
            workspace(
                dir.path(),
                "/new/workspace",
                old.revision,
                "same individual"
            )
            .is_err()
        );
        assert_eq!(Config::load(dir.path()).unwrap().acp.cwd, old.acp.cwd);
    }
    #[test]
    fn relocation_refuses_pending_work_and_uncertain_delivery() {
        for status in ["pending", "running", "interrupted"] {
            let (dir, old) = fixture();
            Store::open(dir.path()).unwrap().db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES('job','context','{}',?1,0,0)", [status]).unwrap();
            assert!(
                workspace(
                    dir.path(),
                    "/new/workspace",
                    old.revision,
                    "same individual"
                )
                .is_err()
            );
            assert_eq!(Config::load(dir.path()).unwrap().revision, old.revision);
        }
        let (dir, old) = fixture();
        Store::open(dir.path()).unwrap().db.execute("INSERT INTO outbox(id,job_id,context,channel,thread,text,status,created,available) VALUES('out','job','context','channel','','reply','uncertain',0,0)", []).unwrap();
        assert!(
            workspace(
                dir.path(),
                "/new/workspace",
                old.revision,
                "same individual"
            )
            .is_err()
        );
    }
}
