//! Durable saved-connection lifecycle; configuration, credentials and sessions stay local.
use crate::{
    auth, config,
    error::{ApiError, ApiResult},
    state::AppState,
};
use axum::{Json, extract::State, http::HeaderMap};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub schema: u8,
    pub connection_id: String,
    pub revision: String,
    pub action: String,
    pub request_id: Option<String>,
}
pub fn id(c: &config::Config) -> String {
    format!("slack:{}", c.id)
}
pub fn revision(c: &config::Config) -> String {
    format!("slack-connection-v1:{}", c.id)
}
pub fn gated(home: &std::path::Path) -> bool {
    home.join("connection-gate.json").exists()
}
fn actual(s: &AppState) -> ApiResult<String> {
    let c = s.instance_config();
    let runtime: Value = std::fs::read(s.home.join("runtime.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({}));

    Ok(if !c.enabled && runtime["state"] == "stopped" && runtime["config_revision"]==c.revision {
        "disconnected"
    } else if crate::server::active_socket_health(s, &c)?["healthy"] == true {
        "connected"
    } else {
        "unknown"
    }
    .into())
}
fn result(
    c: &Command,
    state: &str,
    settled: bool,
    ready: bool,
    failed: bool,
    message: &str,
) -> Value {
    json!({"schema":1,"connection_id":c.connection_id,"request_id":c.request_id,"state":state,"settled":settled,"context_ready":ready,"failed":failed,"message":message})
}
pub fn advance(s: &AppState, c: Command) -> ApiResult<Value> {
    let config = s.instance_config();
    if c.schema != 1
        || c.connection_id != id(&config)
        || c.revision != revision(&config)
        || !["status", "connect", "disconnect"].contains(&c.action.as_str())
        || c.request_id
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
    {
        return Err(ApiError::bad("Invalid saved connection command"));
    }
    if c.request_id.is_none() {
        return if c.action == "status" {
            Ok(result(
                &c,
                &if gated(&s.home) && actual(s)? == "connected" {
                    "busy".into()
                } else {
                    actual(s)?
                },
                false,
                false,
                false,
                "Slack connector status; Slack's presence dot is controlled separately by Slack.",
            ))
        } else {
            Err(ApiError::bad("A request ID is required"))
        };
    }
    let _lock = s.connection_lock.lock().unwrap();
    let rid = c.request_id.as_deref().unwrap();
    let mut op = if let Some(old) = s.get_record("connection_operations", rid)? {
        if old["command"]["connection_id"] != c.connection_id
            || old["command"]["revision"] != c.revision
            || (c.action != "status" && old["command"]["action"] != c.action)
        {
            return Err(ApiError::conflict(json!({"error":"request_id_reused"})));
        }
        if !old["result"].is_null() {
            clear_pending(s, rid)?;
            if old["result"]["state"] == "connected" && old["result"]["failed"] == false {
                clear_gate(s, rid)?;
            }
            return Ok(old["result"].clone());
        }
        old
    } else {
        if c.action == "status" {
            return Ok(result(
                &c,
                "unknown",
                false,
                false,
                false,
                "No accepted operation with this request ID.",
            ));
        }
        if let Some(current) = s.get_record("connection_control", "pending")?
            && current["id"] != rid
        {
            return Err(ApiError::conflict(current));
        }
        let old = json!({"command":c,"phase":"waiting","created_at":crate::model::now_ms(),"initial_config":config});
        {
            let mut db = s.db.lock().unwrap();
            let tx = db.transaction()?;
            for (collection, key, value) in [
                ("connection_operations", rid, old.clone()),
                ("connection_control", "pending", json!({"id":rid})),
            ] {
                tx.execute("INSERT INTO records(collection,id,value,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(collection,id) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",rusqlite::params![collection,key,value.to_string(),crate::model::now_ms()])?;
            }
            tx.commit()?;
        }
        old
    };
    let current = json!(s.instance_config());
    if current != op["initial_config"] && current != op["desired_config"] {
        return Err(ApiError::conflict(
            json!({"error":"connection_configuration_changed"}),
        ));
    }
    config::private_dir(&s.home.join("updates"))?;
    let path = s.home.join("updates/admission.lock");
    let gate = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?,
        Err(e) => return Err(e.into()),
    };
    if gate.try_lock_exclusive().is_err() {
        return Ok(result(
            &c,
            "busy",
            false,
            false,
            false,
            "Waiting for the current installation operation.",
        ));
    }
    config::private_write(
        &s.home.join("connection-gate.json"),
        &serde_json::to_vec(&json!({"request_id":rid})).unwrap(),
    )?;
    let disconnect = op["command"]["action"] == "disconnect";
    let busy:i64=s.db.lock().unwrap().query_row("SELECT (SELECT COUNT(*) FROM jobs WHERE status='running')+(SELECT COUNT(*) FROM outbox WHERE status IN ('ready','sending','uncertain'))+(SELECT COUNT(*) FROM public_speech WHERE status IN ('ready','sending','uncertain'))",[],|r|r.get(0))?;
    if busy > 0 {
        return Ok(result(
            &c,
            "busy",
            false,
            !disconnect,
            false,
            "Waiting for current work and message delivery to settle.",
        ));
    }
    if !disconnect && op["phase"] == "waiting" {
        op["phase"] = json!("context_ready");
        s.set_record("connection_operations", rid, op)?;
        return Ok(result(
            &c,
            "busy",
            false,
            true,
            false,
            "Context ready; connection is still paused.",
        ));
    }
    if disconnect && op["phase"] == "waiting" {
        op["phase"] = json!("context_quiesced");
        s.set_record("connection_operations", rid, op)?;
        let mut response = result(
            &c,
            "busy",
            false,
            false,
            false,
            "Current work has settled; pausing the connection.",
        );
        response["context_quiesced"] = json!(true);
        return Ok(response);
    }
    let mut cfg = s.instance_config();
    // A current terminal supervisor failure is distinct from transport silence.
    // Admission is closed and all delivery above is settled. Return to paused
    // state before reporting failure, so credentials can be repaired explicitly.
    let runtime: Value = std::fs::read(s.home.join("runtime.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if !disconnect
        && op["phase"] == "observing"
        && runtime["state"] == "failed"
        && runtime["config_revision"] == cfg.revision
        && runtime["updated_at"]
            .as_i64()
            .is_some_and(|at| at >= s.started_at)
    {
        op["connect_failed"] = json!(true);
        s.set_record("connection_operations", rid, op.clone())?;
    }
    let disconnect = disconnect || op["connect_failed"] == true;
    if cfg.enabled == disconnect {
        // Save intent before config mutation. Config is the durable desired state;
        // on restart a completed config change is observed, never re-issued.
        if op["phase"] == "changing" {
            return Ok(result(
                &c,
                "unknown",
                false,
                !disconnect,
                false,
                "An interrupted configuration change needs reconciliation.",
            ));
        }
        op["phase"] = json!("changing");
        cfg.enabled = !disconnect;
        let mut desired = cfg.clone();
        desired.revision = cfg
            .revision
            .checked_add(1)
            .ok_or_else(|| ApiError::bad("Configuration revision limit reached"))?;
        op["desired_config"] = json!(desired);
        s.set_record("connection_operations", rid, op.clone())?;
        if let Err(error) = crate::server::replace_config(s, cfg.revision, cfg, "saved-connection")
        {
            let state = actual(s)?;
            if state == "unknown" {
                return Err(error);
            }
            let response = result(
                &c,
                &state,
                true,
                false,
                true,
                "Connection settings or authorization need attention. Open this connector's setup page.",
            );
            op["result"] = response.clone();
            s.set_record("connection_operations", rid, op)?;
            clear_pending(s, rid)?;
            return Ok(response);
        }
        op["phase"] = json!("observing");
        s.set_record("connection_operations", rid, op.clone())?;
    }
    let state = actual(s)?;
    if state
        == if disconnect {
            "disconnected"
        } else {
            "connected"
        }
    {
        let response = result(
            &c,
            &state,
            true,
            !disconnect,
            op["connect_failed"] == true,
            if op["connect_failed"] == true {
                "Slack could not connect. Saved settings are retained; inspect authorization in Settings before a new Connect."
            } else if disconnect {
                "Disconnected. Credentials and conversation history are retained; Slack may still display this app as active."
            } else {
                "Slack message connection is active."
            },
        );
        op["result"] = response.clone();
        s.set_record("connection_operations", rid, op)?;
        clear_pending(s, rid)?;
        if !disconnect {
            clear_gate(s, rid)?;
        }
        Ok(response)
    } else {
        Ok(result(
            &c,
            "busy",
            false,
            !disconnect,
            false,
            "Waiting for the Slack socket to confirm its state.",
        ))
    }
}
pub async fn api(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Json(c): Json<Command>,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &h, "admin", true)?;
    Ok(Json(advance(&s, c)?))
}

fn clear_pending(s: &AppState, id: &str) -> ApiResult<()> {
    if s.get_record("connection_control", "pending")?
        .is_some_and(|p| p["id"] == id)
    {
        s.db.lock().unwrap().execute(
            "DELETE FROM records WHERE collection='connection_control' AND id='pending'",
            [],
        )?;
    }
    Ok(())
}
fn clear_gate(s: &AppState, id: &str) -> ApiResult<()> {
    let path = s.home.join("connection-gate.json");
    if let Ok(bytes) = std::fs::read(&path) {
        let value: Value = serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
        if value["request_id"] == id {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, AppState) {
        let d = tempfile::tempdir().unwrap();
        let c = config::Config::new("Saved Slack".into(), "fixture".into());
        c.save(d.path()).unwrap();
        config::private_write(
            &d.path().join("runtime.json"),
            &serde_json::to_vec(&json!({"state":"stopped","config_revision":c.revision})).unwrap(),
        )
        .unwrap();
        let s = AppState::open(d.path().into()).unwrap();
        s.db.lock().unwrap().execute("INSERT INTO session_bindings(context,endpoint,session_id,resumable,channel,thread,updated) VALUES('room','unchanged','original-session',1,'C123','',1)",[]).unwrap();
        (d, s)
    }
    fn cmd(s: &AppState, action: &str) -> Command {
        let cfg = s.instance_config();
        Command {
            schema: 1,
            connection_id: id(&cfg),
            revision: revision(&cfg),
            action: action.into(),
            request_id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }
    #[test]
    fn disconnect_retains_original_session_and_is_idempotent_after_restart() {
        let (d, s) = fixture();
        let c = cmd(&s, "disconnect");
        assert_eq!(advance(&s, c.clone()).unwrap()["context_quiesced"], true);
        let result = advance(
            &s,
            Command {
                action: "status".into(),
                ..c.clone()
            },
        )
        .unwrap();
        assert_eq!(result["settled"], true);
        assert!(gated(d.path()));
        drop(s);
        let s = AppState::open(d.path().into()).unwrap();
        assert_eq!(advance(&s, c.clone()).unwrap(), result);
        assert_eq!(
            s.db.lock()
                .unwrap()
                .query_row(
                    "SELECT session_id FROM session_bindings WHERE context='room'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "original-session"
        );
        let mut changed = c;
        changed.action = "connect".into();
        assert!(advance(&s, changed).is_err());
        assert!(!s.instance_config().enabled);
    }
    #[test]
    fn active_work_is_drained_before_stopping_and_new_admission_is_blocked() {
        let (d, s) = fixture();
        s.db.lock()
            .unwrap()
            .execute(
                "INSERT INTO jobs(id,context,trigger,status,created,updated,version,note) VALUES('job','room','{}','running',1,1,0,'retained')",
                [],
            )
            .unwrap();
        let mut c = cmd(&s, "disconnect");
        assert_eq!(advance(&s, c.clone()).unwrap()["state"], "busy");
        assert!(crate::updates::admission(d.path()).unwrap().is_none());
        s.db.lock()
            .unwrap()
            .execute("UPDATE jobs SET status='done' WHERE id='job'", [])
            .unwrap();
        c.action = "status".into();
        assert_eq!(advance(&s, c.clone()).unwrap()["context_quiesced"], true);
        assert_eq!(advance(&s, c).unwrap()["state"], "disconnected");
        assert_eq!(
            s.db.lock()
                .unwrap()
                .query_row("SELECT note FROM jobs WHERE id='job'", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "retained"
        );
    }
    #[test]
    fn connect_exposes_readiness_before_configuration_and_missing_auth_is_settled_failure() {
        let (d, s) = fixture();
        let mut c = cmd(&s, "connect");
        let r = advance(&s, c.clone()).unwrap();
        assert_eq!(r["context_ready"], true);
        assert_eq!(r["settled"], false);
        assert!(!s.instance_config().enabled);
        c.action = "status".into();
        let r = advance(&s, c).unwrap();
        assert_eq!(r["failed"], true);
        assert_eq!(r["settled"], true);
        assert!(gated(d.path()));
        assert!(!s.instance_config().enabled);
    }
    #[test]
    fn changed_configuration_during_pending_operation_does_not_continue() {
        let (_d, s) = fixture();
        let mut c = cmd(&s, "connect");
        assert_eq!(advance(&s, c.clone()).unwrap()["context_ready"], true);
        let mut cfg = s.instance_config();
        cfg.label = "Changed during pending operation".into();
        crate::server::replace_config(&s, cfg.revision, cfg, "fixture").unwrap();
        c.action = "status".into();
        assert!(advance(&s, c).is_err());
        assert!(gated(&s.home));
        assert!(!s.instance_config().enabled);
    }

    #[test]
    fn current_supervisor_failure_settles_paused_without_replaying_connect() {
        let (_d, s) = fixture();
        let mut cfg = s.instance_config();
        cfg.enabled = true;
        cfg.save(&s.home).unwrap();
        *s.config.write().unwrap() = cfg.clone();
        let mut c = cmd(&s, "connect");
        let rid = c.request_id.clone().unwrap();
        s.set_record(
            "connection_operations",
            &rid,
            json!({"command":c,"phase":"observing","initial_config":cfg}),
        )
        .unwrap();
        config::private_write(
            &s.home.join("runtime.json"),
            &serde_json::to_vec(
                &json!({"state":"failed","config_revision":cfg.revision,"updated_at":s.started_at}),
            )
            .unwrap(),
        )
        .unwrap();
        c.action = "status".into();
        assert_eq!(advance(&s, c.clone()).unwrap()["settled"], false);
        let current = s.instance_config();
        assert!(!current.enabled);
        config::private_write(&s.home.join("runtime.json"), &serde_json::to_vec(&json!({"state":"stopped","config_revision":current.revision,"updated_at":s.started_at})).unwrap()).unwrap();
        let r = advance(&s, c.clone()).unwrap();
        assert_eq!(r["settled"], true);
        assert_eq!(r["failed"], true);
        assert_eq!(r["state"], "disconnected");
        assert!(gated(&s.home));
        assert_eq!(advance(&s, c).unwrap(), r);
    }

    #[test]
    fn status_with_unknown_id_never_creates_an_operation() {
        let (d, s) = fixture();
        let c = cmd(&s, "status");
        assert_eq!(advance(&s, c).unwrap()["state"], "unknown");
        assert!(!gated(d.path()));
        assert!(s.list_records("connection_operations").unwrap().is_empty());
    }
}
