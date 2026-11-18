//! Fixed published-bundle updates for a native installation. External enrollment
//! retains update ownership; caller-provided commands, paths and URLs are refused.
use crate::{
    auth,
    config::{self, AuthMode, Config, WebTransport},
    error::{ApiError, ApiResult},
    state::AppState,
};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::Arc};
const SCRIPT: &[u8] = include_bytes!("../scripts/native_update.py");
fn read(path: &Path) -> Result<Value> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 4 * 1024 * 1024,
        "Native update record is not a bounded ordinary file"
    );
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
fn allowed(home: &Path) -> Result<()> {
    let c = Config::load(home)?;
    let marker = read(&home.join("native-installation.json"))?;
    ensure!(
        c.server.auth_mode == AuthMode::LocalPassword
            && c.server.web_transport == WebTransport::LocalHttp
            && marker["id"] == c.id
            && marker["update_owner"] == "installation",
        "This installation is not managed by the native app"
    );
    ensure!(
        !home.join("saved-connections.json").exists()
            && !home.join("updates/registered-service.json").exists(),
        "Updates belong to this installation's enrolled host controls"
    );
    Ok(())
}
pub fn active(home: &Path) -> Result<Option<Value>> {
    let root = home.join("native-updates");
    let marker = root.join("active.json");
    match std::fs::symlink_metadata(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => (),
    }
    let marker = read(&marker)?;
    let id = marker["id"]
        .as_str()
        .context("Native update identity is invalid")?;
    ensure!(
        uuid::Uuid::parse_str(id)?.to_string() == id,
        "Native update identity is invalid"
    );
    let job = read(&root.join(id).join("plan.json"))?;
    ensure!(
        job["schema"] == 1
            && job["id"] == id
            && matches!(job["action"].as_str(), Some("check" | "apply" | "rollback"))
            && matches!(
                job["state"].as_str(),
                Some(
                    "queued"
                        | "checking"
                        | "draining"
                        | "ready"
                        | "deferred"
                        | "stopping"
                        | "swapping"
                        | "verifying"
                        | "rolling_back"
                        | "recovery_required"
                        | "succeeded"
                        | "rolled_back"
                        | "failed"
                )
            ),
        "Native update state is invalid; saved evidence was preserved"
    );
    Ok(Some(job))
}
pub fn blocks(home: &Path) -> bool {
    active(home).map_or(true, |job| {
        job.is_some_and(|j| {
            [
                "stopping",
                "swapping",
                "verifying",
                "rolling_back",
                "recovery_required",
            ]
            .contains(&j["state"].as_str().unwrap_or(""))
        })
    })
}
/// Called while holding service.lock, before opening state or starting a listener.
/// A replacement may start only for the still-current durable launch intent.
pub fn validate_launch(home: &Path) -> Result<bool> {
    let plan = std::env::var("ACP_SLACK_UPDATE_PLAN").ok();
    let nonce = std::env::var("ACP_SLACK_UPDATE_LAUNCH").ok();
    validate_launch_intent(home, plan.as_deref(), nonce.as_deref())
}
pub(crate) fn validate_launch_intent(
    home: &Path,
    plan: Option<&str>,
    nonce: Option<&str>,
) -> Result<bool> {
    if plan.is_none() && nonce.is_none() {
        // Ordinary app opening must retain its existing management/recovery UI.
        // Unresolved state permits that UI but never starts the Slack runtime.
        return Ok(!blocks(home));
    }
    let job = active(home)?.context("No update owns this launch")?;
    let plan = plan.context("Missing update launch identity")?;
    let nonce = nonce.context("Missing update launch identity")?;
    ensure!(
        uuid::Uuid::parse_str(nonce)?.to_string() == nonce,
        "Invalid update launch identity"
    );
    let mode = if job["state"] == "verifying" {
        "candidate"
    } else if job["state"] == "rolling_back" {
        "rollback"
    } else {
        anyhow::bail!("Update launch was superseded")
    };
    ensure!(
        job["id"] == plan && job["launch"]["nonce"] == nonce && job["launch"]["mode"] == mode,
        "Update launch was superseded"
    );
    let expected = if mode == "candidate" {
        &job["binary_sha256"]
    } else {
        &job["previous_sha256"]
    };
    use sha2::{Digest, Sha256};
    let actual = format!(
        "{:x}",
        Sha256::digest(std::fs::read(std::env::current_exe()?)?)
    );
    ensure!(
        job["launch"]["binary_sha256"] == *expected && expected.as_str() == Some(actual.as_str()),
        "Update launch executable changed"
    );
    // Hashing a large executable is not atomic with recovery's journal update.
    // Recheck the same intent immediately before opening installation state.
    ensure!(
        active(home)?.as_ref() == Some(&job),
        "Update launch was superseded"
    );
    Ok(true)
}
/// Fixed stop-only authority for the installation-owned updater. This does not
/// authorize registration, start, retirement or replacement. The caller persists
/// its UUID before invoking the manager and retains any uncertain result.
pub(crate) fn validate_stop_intent(home: &Path, plan: &str, request: &str) -> Result<Value> {
    allowed(home)?;
    ensure!(
        uuid::Uuid::parse_str(plan)?.to_string() == plan
            && uuid::Uuid::parse_str(request)?.to_string() == request,
        "Invalid managed update stop identity"
    );
    let job = active(home)?.context("No update owns this stop")?;
    ensure!(
        job["id"] == plan
            && matches!(job["action"].as_str(), Some("apply" | "rollback"))
            && matches!(job["state"].as_str(), Some("stopping" | "rolling_back"))
            && job["manager_stop"]["request_id"] == request
            && job["manager_stop"]["dispatch"] == "intent",
        "Update does not own this exact stop intent"
    );
    use sha2::{Digest, Sha256};
    let digest = |path: &Path| -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
    };
    let current_config = digest(&home.join("adapter.json"))?;
    ensure!(
        job["manager_stop"]["config_sha256"].as_str() == Some(current_config.as_str()),
        "Update stop configuration changed since its explicit intent"
    );
    if job["state"] == "stopping" {
        ensure!(
            job["config_sha256"].as_str() == Some(current_config.as_str()),
            "Apply configuration changed since CHECK"
        );
    }
    let binary = amplifier_native_platform::installed_binary(&crate::desktop::APPLICATION, home);
    let actual = digest(&binary)?;
    let allowed = if job["state"] == "stopping" {
        job["installed_sha256"].as_str() == Some(actual.as_str())
    } else {
        ["binary_sha256", "previous_sha256"]
            .iter()
            .any(|k| job[*k].as_str() == Some(actual.as_str()))
    };
    ensure!(
        allowed,
        "Update stop executable is not the captured installed/candidate/previous binary"
    );
    ensure!(
        active(home)?.as_ref() == Some(&job),
        "Update stop intent changed during validation"
    );
    Ok(job)
}
fn worker_guard(home: &Path) -> Result<std::fs::File> {
    let path = home.join("native-updates/worker.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file).context("The update worker is still running")?;
    Ok(file)
}
pub fn status(home: &Path) -> Value {
    let support = allowed(home);
    let (mut current, journal_error) = match active(home) {
        Ok(current) => (current, None),
        Err(error) => (
            None,
            Some(format!("Update state needs inspection: {error}")),
        ),
    };
    if let Some(job) = current.as_mut() {
        let running = match std::fs::File::open(home.join("native-updates/worker.lock")) {
            Ok(file) => match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => Some(false),
                Err(e) if crate::process::lock_busy(&e) => Some(true),
                Err(_) => None,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        };
        let old = crate::model::now_ms()
            - job["updated_at"]
                .as_i64()
                .or_else(|| job["created_at"].as_i64())
                .unwrap_or(0)
            > 10000;
        job["worker_active"] = json!(running);
        if crate::native_manager::is_managed(home).unwrap_or(false) {
            let observation = (|| -> Result<Value> {
                let plan = job["id"].as_str().context("Missing plan")?;
                let request = job["request_id"]
                    .as_str()
                    .context("Missing worker request")?;
                ensure!(
                    uuid::Uuid::parse_str(plan)?.to_string() == plan
                        && uuid::Uuid::parse_str(request)?.to_string() == request,
                    "Invalid worker request"
                );
                let path = worker_dispatch_path(home, plan, request);
                if !path.exists() {
                    return Ok(json!({"state":"not_observed"}));
                }
                let record = read(&path)?;
                let retired = home
                    .join("native-updates")
                    .join(plan)
                    .join(format!("worker-retirement-{request}.json"));
                let cleanup = if retired.exists() {
                    let receipt = read(&retired)?;
                    if receipt["outcome"]["status"] == "retired" {
                        "retired"
                    } else {
                        "unknown"
                    }
                } else {
                    "pending"
                };
                let folder = home.join("native-updates").join(plan);
                let terminal_receipt_observed =
                    read(&folder.join(format!("worker-entered-{request}.json")))
                        .and_then(|entered| {
                            read(&folder.join(format!("worker-finished-{request}.json"))).and_then(
                                |finished| {
                                    validate_finished_worker(&entered, &finished, plan, request)
                                },
                            )
                        })
                        .is_ok();
                Ok(
                    json!({"request_id":request,"plan_id":plan,"dispatch_phase":record["phase"],"cleanup_last_observed":cleanup,"terminal_receipt_observed":terminal_receipt_observed,"read_only":true}),
                )
            })();
            job["worker_manager"] = observation
                .unwrap_or_else(|_| json!({"state":"observation_unavailable","read_only":true}));
        }

        job["recovery_available"] = json!(
            running == Some(false)
                && old
                && [
                    "queued",
                    "checking",
                    "draining",
                    "stopping",
                    "swapping",
                    "verifying",
                    "rolling_back",
                    "recovery_required"
                ]
                .contains(&job["state"].as_str().unwrap_or(""))
        );
    }
    json!({"supported":support.is_ok(),"reason":support.err().map(|e|e.to_string()).or(journal_error.clone()),"journal_error":journal_error,"repository":"microsoft/amplifier-acp-slack","source":"published releases","current":current})
}
pub async fn get(State(s): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", false)?;
    Ok(Json(status(&s.home)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    action: String,
    request_id: uuid::Uuid,
    plan_id: Option<uuid::Uuid>,
}
pub async fn command(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Command>,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", true)?;
    allowed(&s.home).map_err(|e| ApiError::bad(e.to_string()))?;
    let home = s.home.clone();
    let value = tokio::task::spawn_blocking(move || command_value(&home, input))
        .await
        .map_err(|_| ApiError::bad("Update preparation did not finish"))??;
    Ok(Json(value))
}
const CLEANUP_PROOF: &str =
    "matching_worker_receipts_exact_profile_inactive_empty_and_lifetime_lock_free";
fn cleanup_projection(
    plan: uuid::Uuid,
    request: uuid::Uuid,
    observation_only: bool,
    result: Result<Value>,
) -> Value {
    let complete = result.as_ref().is_ok_and(|value| value["retired"] == true);
    json!({"schema":"com.microsoft.amplifier.slack-native-update-cleanup/1","plan_id":plan,"request_id":request,"cleanup":if complete{"complete"}else{"unknown"},"observation_only":observation_only,"proof_basis":if complete{Some(CLEANUP_PROOF)}else{None},"full_generation_observed":false})
}
fn command_value(home: &Path, input: Command) -> Result<Value> {
    match input.action.as_str() {
        "retire_worker" | "inspect_worker_retirement" => {
            let plan = input.plan_id.context("Choose the original update plan")?;
            let inspect = input.action == "inspect_worker_retirement";
            let result = if inspect {
                inspect_worker_retirement(home, plan, input.request_id)
            } else {
                retire_managed_worker(home, plan, input.request_id)
            };
            Ok(cleanup_projection(plan, input.request_id, inspect, result))
        }
        _ => queue(home, input),
    }
}
fn queue(home: &Path, input: Command) -> Result<Value> {
    let root = home.join("native-updates");
    config::private_dir(&root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("queue.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("Another update operation is being prepared")?;
    let request_file = root.join(format!("request-{}.json", input.request_id));
    if request_file.exists() {
        let previous = read(&request_file)?;
        ensure!(
            previous["request_action"]
                .as_str()
                .or_else(|| previous["action"].as_str())
                == Some(input.action.as_str())
                && input
                    .plan_id
                    .is_none_or(|id| previous["id"] == id.to_string()),
            "This request ID already belongs to another update operation"
        );
        return Ok(previous);
    }
    if let Some(job) = active(home)? {
        let recovery = ["recover", "rollback"].contains(&input.action.as_str())
            && input.plan_id.is_some_and(|id| job["id"] == id.to_string());
        ensure!(
            recovery
                || ![
                    "queued",
                    "checking",
                    "draining",
                    "stopping",
                    "swapping",
                    "verifying",
                    "rolling_back",
                    "recovery_required"
                ]
                .contains(&job["state"].as_str().unwrap_or("")),
            "An update is already in progress or needs recovery"
        );
    }
    let _recovery_guard = if crate::native_manager::is_managed(home)?
        || ["recover", "rollback"].contains(&input.action.as_str())
    {
        Some(worker_guard(home)?)
    } else {
        None
    };
    let id = input.plan_id.unwrap_or(input.request_id).to_string();
    let folder = root.join(&id);
    config::private_dir(&folder)?;
    let mut effective_action = input.action.clone();
    let mut job = match input.action.as_str() {
        "recover" => {
            ensure!(input.plan_id.is_some(), "Choose the interrupted plan");
            let mut previous = read(&folder.join("plan.json"))?;
            ensure!(
                [
                    "queued",
                    "checking",
                    "draining",
                    "stopping",
                    "swapping",
                    "verifying",
                    "rolling_back",
                    "recovery_required"
                ]
                .contains(&previous["state"].as_str().unwrap_or("")),
                "This update is not interrupted"
            );
            if ["queued", "checking", "draining"]
                .contains(&previous["state"].as_str().unwrap_or(""))
            {
                previous["state"] = json!("failed");
                previous["reason"] = json!("interrupted_before_cutover");
                previous["request_action"] = json!("recover");
                previous["request_id"] = json!(input.request_id);
                config::private_write(
                    &folder.join("plan.json"),
                    &serde_json::to_vec_pretty(&previous)?,
                )?;
                config::private_write(&request_file, &serde_json::to_vec(&previous)?)?;
                let gate = home.join("updates/drain.json");
                if read(&gate).is_ok_and(|v| v["plan_id"] == id) {
                    std::fs::remove_file(gate)?;
                }
                return Ok(previous);
            }
            effective_action = "rollback".into();
            previous
        }
        "check" => {
            ensure!(input.plan_id.is_none(), "Check takes no plan ID");
            json!({"schema":1,"id":id,"created_at":crate::model::now_ms()})
        }
        "apply" | "rollback" => {
            ensure!(input.plan_id.is_some(), "Choose an existing update plan");
            let job = read(&folder.join("plan.json"))?;
            ensure!(
                if input.action == "apply" {
                    ["ready", "deferred"].contains(&job["state"].as_str().unwrap_or(""))
                        && job["action"] != "rollback"
                } else {
                    ["succeeded", "recovery_required"]
                        .contains(&job["state"].as_str().unwrap_or(""))
                        || (job["state"] == "deferred" && job["action"] == "rollback")
                },
                "This plan cannot perform that operation"
            );
            job
        }
        _ => anyhow::bail!("Choose check, apply, rollback or recover"),
    };
    let critical_recovery = [
        "stopping",
        "swapping",
        "verifying",
        "rolling_back",
        "recovery_required",
    ]
    .contains(&job["state"].as_str().unwrap_or(""));
    if effective_action == "rollback" {
        ensure!(
            folder.join("backup/binary").is_file(),
            "No retained prior version is available"
        );
    }
    job["critical_recovery"] = json!(critical_recovery);
    // Invalidate any process spawned by an earlier request before recovery can
    // dispatch. Its original PID may be unknown after a worker crash.
    job["launch"] = Value::Null;
    job["action"] = json!(effective_action);
    job["request_action"] = json!(input.action);
    job["updated_at"] = json!(crate::model::now_ms());
    job["state"] = json!(if critical_recovery {
        "rolling_back"
    } else {
        "queued"
    });
    job["request_id"] = json!(input.request_id);
    // The worker runs from its own private Python copy so Windows can replace the installed runtime.
    if !folder.join("worker-runtime").exists() {
        crate::desktop::copy_runtime(&home.join("runtime"), &folder.join("worker-runtime"))?;
    }
    config::private_write(&folder.join("worker.py"), SCRIPT)?;
    config::private_write(&folder.join("plan.json"), &serde_json::to_vec_pretty(&job)?)?;
    config::private_write(
        &root.join("active.json"),
        &serde_json::to_vec(&json!({"id":id}))?,
    )?;
    config::private_write(&request_file, &serde_json::to_vec(&job)?)?;
    if crate::native_manager::is_managed(home)? {
        let helper = folder.join(format!("worker-{}.py", input.request_id));
        amplifier_native_platform::private_create(&helper, SCRIPT)?;
        let dispatch = dispatch_managed_worker(home, &id, &input.request_id.to_string())?;
        let mut response = read(&folder.join("plan.json"))?;
        response["managed_worker_dispatch"] = json!({"request_id":input.request_id,"phase":dispatch["phase"],"outcome":dispatch["outcome"]});
        return Ok(response);
    }
    let python = folder.join("worker-runtime/python").join(if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    });
    let mut child = std::process::Command::new(python);
    child
        .args(["-I", "-B", "-X", "utf8"])
        .arg(folder.join("worker.py"))
        .arg(home)
        .arg(&id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        child.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.process_group(0);
    }
    if child.spawn().is_err() {
        job["state"] = json!(if critical_recovery {
            "recovery_required"
        } else {
            "failed"
        });
        job["reason"] = json!("update_worker_unavailable");
        config::private_write(&folder.join("plan.json"), &serde_json::to_vec_pretty(&job)?)?;
    }
    Ok(job)
}
#[derive(Clone, Debug, serde::Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManagedWorker {
    home: std::path::PathBuf,
    installation_id: String,
    plan_id: String,
    request_id: String,
    service: amplifier_native_platform::installation::Service,
    profile: amplifier_native_platform::registration::Registration,
    python_sha256: String,
    helper_sha256: String,
    runtime_manifest_sha256: String,
}
fn worker_hash(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink() && m.len() <= 256 * 1024 * 1024,
        "Invalid worker material"
    );
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}
fn managed_worker(home: &Path, plan: &str, request: &str) -> Result<ManagedWorker> {
    use amplifier_native_platform::{
        self as p,
        installation::{Service, ServiceManager},
        registration::Registration,
    };
    ensure!(
        uuid::Uuid::parse_str(plan)?.to_string() == plan
            && uuid::Uuid::parse_str(request)?.to_string() == request,
        "Invalid worker identity"
    );
    let home = home.canonicalize()?;
    p::validate_private_directory(&home)?;
    allowed(&home)?;
    ensure!(
        crate::native_manager::is_managed(&home)?,
        "This is not a native-manager installation"
    );
    let c = Config::load(&home)?;
    ensure!(
        uuid::Uuid::parse_str(&c.id)?.to_string() == c.id,
        "Invalid worker installation identity"
    );
    let folder = home.join("native-updates").join(plan);
    let runtime = folder.join("worker-runtime");
    let manifest_path = runtime.join("native-bundle.json");
    let manifest = read(&manifest_path)?;
    let files = manifest["python_files"]
        .as_object()
        .context("Missing immutable worker inventory")?;
    ensure!(
        !files.is_empty() && files.len() < 20000,
        "Invalid worker inventory"
    );
    for (name, sha) in files {
        let path = Path::new(name);
        ensure!(
            path.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
                && !name.contains(['\\', ':']),
            "Invalid worker member"
        );
        ensure!(
            sha.as_str() == Some(worker_hash(&runtime.join("python").join(path))?.as_str()),
            "Worker runtime inventory changed"
        );
    }
    let entry = if cfg!(windows) {
        "pythonw.exe"
    } else {
        "bin/python3"
    };
    let python = runtime.join("python").join(entry);
    let python_sha256 = worker_hash(&python)?;
    ensure!(
        files.get(entry).and_then(Value::as_str) == Some(python_sha256.as_str()),
        "Windowless/private worker executable is not in inventory"
    );
    let helper = folder.join(format!("worker-{request}.py"));
    let helper_sha256 = worker_hash(&helper)?;
    let name = format!("acp-slack-worker-{}-{request}", c.id);
    let user = p::user_home()?;
    #[cfg(target_os = "linux")]
    let service = Service {
        manager: ServiceManager::SystemdUser,
        name: format!("{name}.service"),
        definition_path: Some(
            user.join(".config/systemd/user")
                .join(format!("{name}.service")),
        ),
    };
    #[cfg(target_os = "macos")]
    let service = Service {
        manager: ServiceManager::LaunchAgent,
        name: name.clone(),
        definition_path: Some(
            user.join("Library/LaunchAgents")
                .join(format!("{name}.plist")),
        ),
    };
    #[cfg(windows)]
    let service = {
        let _ = user;
        Service {
            manager: ServiceManager::ScheduledTask,
            name,
            definition_path: Some(folder.join(format!("worker-{request}.xml"))),
        }
    };
    let text = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .context("Invalid fixed worker path")
    };
    let profile = Registration {
        executable: python,
        args: vec![
            "-I".into(),
            "-B".into(),
            "-X".into(),
            "utf8".into(),
            text(&helper)?,
            text(&home)?,
            plan.into(),
            request.into(),
        ],
        cwd: folder,
        description: "Constellation Slack one-shot updater".into(),
        autostart: false,
    };
    Ok(ManagedWorker {
        home,
        installation_id: c.id,
        plan_id: plan.into(),
        request_id: request.into(),
        service,
        profile,
        python_sha256,
        helper_sha256,
        runtime_manifest_sha256: worker_hash(&manifest_path)?,
    })
}
fn worker_dispatch_path(home: &Path, plan: &str, request: &str) -> std::path::PathBuf {
    home.join("native-updates")
        .join(plan)
        .join(format!("worker-dispatch-{request}.json"))
}
fn worker_record_write(path: &Path, value: &Value) -> Result<()> {
    amplifier_native_platform::private_write_data(path, &serde_json::to_vec_pretty(value)?)
}
fn dispatch_managed_worker(home: &Path, plan: &str, request: &str) -> Result<Value> {
    use amplifier_native_platform::{
        self as p,
        installation::{self, ActionOutcome, ServiceAction},
        registration::{self, RegistrationOutcome},
    };
    let timer = crate::native_manager::NativeBudget::new();
    let profile = managed_worker(home, plan, request)?;
    let path = worker_dispatch_path(&profile.home, plan, request);
    if path.exists() {
        let old = read(&path)?;
        ensure!(
            old["binding"] == serde_json::to_value(&profile)?,
            "Worker request changed"
        );
        return Ok(old);
    }
    timer.exists()?;
    ensure!(
        !installation::manager_registration_exists(&profile.service)?,
        "Existing worker profile was preserved"
    );
    let mut record = json!({"schema":1,"request_id":request,"plan_id":plan,"binding":profile,"phase":"register_intent","outcome":"unknown"});
    p::private_create(&path, &serde_json::to_vec_pretty(&record)?)?;
    let execute = (|| -> Result<()> {
        timer.register()?;
        match registration::register(&profile.service, &profile.profile)? {
            RegistrationOutcome::Registered { .. } => {}
            RegistrationOutcome::Refused { code, .. } => {
                record["phase"] = json!("refused");
                record["outcome"] = json!(code);
                return Ok(());
            }
            RegistrationOutcome::Unknown { .. } => return Ok(()),
        }
        record["phase"] = json!("registered");
        worker_record_write(&path, &record)?;
        timer.snapshot()?;
        let before = installation::snapshot(&profile.service)?;
        ensure!(
            registration::profile_matches(&profile.service, &profile.profile, &before)?
                && before["active"] == false
                && before["transitioning"] == false
                && before["processes"].as_array().is_some_and(Vec::is_empty),
            "Worker profile is not safely inactive"
        );
        record["phase"] = json!("start_intent");
        record["before"] = before.clone();
        worker_record_write(&path, &record)?;
        timer.action()?;
        let outcome = installation::request_state_checked(
            &profile.service,
            ServiceAction::Start,
            &before,
            |_| {
                ensure!(
                    managed_worker(home, plan, request)? == profile,
                    "Worker material changed before dispatch"
                );
                let job = active(home)?.context("Worker plan missing")?;
                ensure!(
                    job["id"] == plan && job["request_id"] == request,
                    "Worker intent was superseded"
                );
                Ok(None)
            },
        )?;
        match outcome {
            ActionOutcome::Accepted => {
                record["phase"] = json!("accepted");
                record["outcome"] = json!("accepted")
            }
            ActionOutcome::Refused { code, .. } => {
                record["phase"] = json!("refused");
                record["outcome"] = json!(code)
            }
            ActionOutcome::Unknown { .. } => {
                record["phase"] = json!("unknown");
            }
        }
        Ok(())
    })();
    if execute.is_err() {
        record["phase"] = json!("unknown");
    }
    worker_record_write(&path, &record)?;
    Ok(record)
}
fn validate_finished_worker(
    entered: &Value,
    finished: &Value,
    plan: &str,
    request: &str,
) -> Result<()> {
    ensure!(
        finished["schema"] == 1
            && finished["plan_id"] == plan
            && finished["request_id"] == request
            && matches!(
                finished["state"].as_str(),
                Some("ready" | "succeeded" | "rolled_back" | "failed" | "deferred")
            ),
        "No exact terminal worker receipt"
    );
    ensure!(
        entered["schema"] == 1
            && entered["plan_id"] == plan
            && entered["request_id"] == request
            && entered["pid"] == finished["pid"]
            && entered["pid"]
                .as_u64()
                .is_some_and(|p| p > 0 && p <= u32::MAX as u64),
        "Worker terminal receipt does not match its entered process"
    );
    Ok(())
}
/// Read-only reconciliation of an already recorded cleanup. Missing intent is
/// uncertainty, never authorization to dispatch a new retirement.
fn inspect_worker_retirement(home: &Path, plan: uuid::Uuid, request: uuid::Uuid) -> Result<Value> {
    use amplifier_native_platform::registration;
    let timer = crate::native_manager::NativeBudget::new();
    let plan = plan.to_string();
    let request = request.to_string();
    let binding = managed_worker(home, &plan, &request)?;
    let folder = binding.home.join("native-updates").join(&plan);
    let record = read(&folder.join(format!("worker-retirement-{request}.json")))?;
    ensure!(
        record["binding"] == serde_json::to_value(&binding)?,
        "Worker cleanup binding changed"
    );
    let entered = read(&folder.join(format!("worker-entered-{request}.json")))?;
    let finished = read(&folder.join(format!("worker-finished-{request}.json")))?;
    validate_finished_worker(&entered, &finished, &plan, &request)?;
    ensure!(
        record["before"]["active"] == false
            && record["before"]["transitioning"] == false
            && record["before"]["processes"]
                .as_array()
                .is_some_and(Vec::is_empty),
        "No recorded inactive profile proof"
    );
    // Open an existing lifetime lock only. This observation creates no file.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(binding.home.join("native-updates/worker.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Worker remains active")?;
    timer.exists()?;
    let retired = registration::is_retired(&binding.service)?;
    ensure!(
        managed_worker(home, &plan, &request)? == binding,
        "Worker material changed during observation"
    );
    Ok(json!({"retired":retired}))
}
/// Explicit cleanup of one terminal one-shot worker, never a status-side effect.
/// Registration absence does not rewrite its update outcome or remove its files.
pub fn retire_managed_worker(home: &Path, plan: uuid::Uuid, request: uuid::Uuid) -> Result<Value> {
    use amplifier_native_platform::{
        self as p, installation,
        registration::{self, RetirementOutcome},
    };
    let timer = crate::native_manager::NativeBudget::new();
    let plan = plan.to_string();
    let request = request.to_string();
    let binding = managed_worker(home, &plan, &request)?;
    let root = binding.home.join("native-updates");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("queue.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Another update owner is active")?;
    let _worker = worker_guard(&binding.home)?;
    let dispatch = read(&worker_dispatch_path(&binding.home, &plan, &request))?;
    ensure!(
        dispatch["binding"] == serde_json::to_value(&binding)?,
        "Worker binding changed"
    );
    let finished = read(
        &root
            .join(&plan)
            .join(format!("worker-finished-{request}.json")),
    )?;
    let entered = read(
        &root
            .join(&plan)
            .join(format!("worker-entered-{request}.json")),
    )?;
    validate_finished_worker(&entered, &finished, &plan, &request)?;
    let path = root
        .join(&plan)
        .join(format!("worker-retirement-{request}.json"));
    if path.exists() {
        let record = read(&path)?;
        ensure!(
            record["binding"] == serde_json::to_value(&binding)?,
            "Worker retirement binding changed"
        );
        timer.exists()?;
        return Ok(
            json!({"schema":1,"plan_id":plan,"request_id":request,"retired":registration::is_retired(&binding.service)?,"dispatch_repeated":false,"prior_outcome":record["outcome"],"service":binding.service,"worker_lock_available":true,"finished":finished}),
        );
    }
    // The process may have exited before the OS manager marks its profile
    // inactive. Observe within this single command's existing owner budget;
    // no retirement is dispatched until the exact profile is settled.
    let before = loop {
        timer.retire()?;
        timer.snapshot()?;
        let observed = installation::snapshot(&binding.service)?;
        ensure!(
            registration::profile_matches(&binding.service, &binding.profile, &observed)?,
            "Worker profile changed"
        );
        if observed["active"] == false
            && observed["transitioning"] == false
            && observed["processes"].as_array().is_some_and(Vec::is_empty)
        {
            break observed;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    timer.retire()?;
    let mut record = json!({"schema":1,"binding":binding,"before":before,"outcome":"unknown"});
    p::private_create(&path, &serde_json::to_vec_pretty(&record)?)?;
    ensure!(
        managed_worker(home, &plan, &request)? == binding,
        "Worker material changed before retirement"
    );
    let outcome = registration::retire(&binding.service, &binding.profile, &before);
    let retired = matches!(outcome, Ok(RetirementOutcome::Retired { .. }));
    record["outcome"] = match outcome {
        Ok(v) => serde_json::to_value(v)?,
        Err(_) => json!("unknown"),
    };
    worker_record_write(&path, &record)?;
    Ok(
        json!({"schema":1,"plan_id":plan,"request_id":request,"retired":retired,"dispatch_repeated":false,"prior_outcome":record["outcome"],"service":binding.service,"worker_lock_available":true,"finished":finished}),
    )
}
pub async fn readiness(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", false)?;
    let count:i64=s.db.lock().unwrap().query_row("SELECT (SELECT COUNT(*) FROM jobs WHERE status='running')+(SELECT COUNT(*) FROM outbox WHERE status IN ('ready','sending','uncertain'))+(SELECT COUNT(*) FROM public_speech WHERE status IN ('ready','sending','uncertain'))",[],|r|r.get(0))?;
    Ok(Json(
        json!({"idle":count==0 && s.check_gate.available_permits()>0 && s.setup_gate.available_permits()>0,"pending_or_unknown":count,"id":s.instance_config().id}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    plan_id: uuid::Uuid,
}
pub async fn stop(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Stop>,
) -> ApiResult<Json<Value>> {
    auth::check(&s, &headers, "admin", true)?;
    allowed(&s.home).map_err(|e| ApiError::bad(e.to_string()))?;
    let job = active(&s.home)
        .map_err(|e| ApiError::bad(e.to_string()))?
        .ok_or_else(ApiError::missing)?;
    if job["id"] != input.plan_id.to_string()
        || !["stopping", "rolling_back"].contains(&job["state"].as_str().unwrap_or(""))
    {
        return Err(ApiError::forbidden());
    }
    let handle = s
        .shutdown
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| ApiError::bad("Service shutdown is unavailable"))?;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });
    Ok(Json(json!({"stopping":true,"plan_id":input.plan_id})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(phase: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native-updates");
        let id = uuid::Uuid::new_v4().to_string();
        config::private_dir(&root.join(&id)).unwrap();
        config::private_write(
            &root.join("active.json"),
            json!({"id":id}).to_string().as_bytes(),
        )
        .unwrap();
        config::private_write(
            &root.join(&id).join("plan.json"),
            json!({"schema":1,"id":id,"state":phase,"action":"check","created_at":1})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        (dir, id)
    }
    #[test]
    fn invalid_existing_journal_preserves_closed_admission() {
        for invalid_marker in [false, true] {
            let (dir, id) = fixture("verifying");
            let root = dir.path().join("native-updates");
            let path = if invalid_marker {
                root.join("active.json")
            } else {
                root.join(id).join("plan.json")
            };
            std::fs::write(&path, b"{interrupted").unwrap();
            assert!(
                blocks(dir.path()),
                "Existing unreadable state is not no update"
            );
            assert!(
                queue(
                    dir.path(),
                    Command {
                        action: "check".into(),
                        request_id: uuid::Uuid::new_v4(),
                        plan_id: None
                    }
                )
                .is_err()
            );
            assert_eq!(std::fs::read(path).unwrap(), b"{interrupted");
        }
    }
    #[test]
    fn delayed_launch_cannot_cross_recovery_or_a_new_launch_nonce() {
        use sha2::{Digest, Sha256};
        let (dir, id) = fixture("verifying");
        let file = dir
            .path()
            .join("native-updates")
            .join(&id)
            .join("plan.json");
        let nonce = uuid::Uuid::new_v4().to_string();
        let digest = format!(
            "{:x}",
            Sha256::digest(std::fs::read(std::env::current_exe().unwrap()).unwrap())
        );
        let mut job = read(&file).unwrap();
        job["binary_sha256"] = json!(digest);
        job["previous_sha256"] = json!(digest);
        job["launch"] = json!({"nonce":nonce,"mode":"candidate","binary_sha256":digest});
        config::private_write(&file, &serde_json::to_vec(&job).unwrap()).unwrap();
        assert!(validate_launch_intent(dir.path(), Some(&id), Some(&nonce)).is_ok());
        assert!(!validate_launch_intent(dir.path(), None, None).unwrap());
        job["state"] = json!("rolling_back");
        job["launch"] = Value::Null;
        config::private_write(&file, &serde_json::to_vec(&job).unwrap()).unwrap();
        assert!(validate_launch_intent(dir.path(), Some(&id), Some(&nonce)).is_err());
        let next = uuid::Uuid::new_v4().to_string();
        job["launch"] = json!({"nonce":next,"mode":"rollback","binary_sha256":digest});
        config::private_write(&file, &serde_json::to_vec(&job).unwrap()).unwrap();
        assert!(validate_launch_intent(dir.path(), Some(&id), Some(&nonce)).is_err());
        assert!(validate_launch_intent(dir.path(), Some(&id), Some(&next)).is_ok());
        job["state"] = json!("rolled_back");
        config::private_write(&file, &serde_json::to_vec(&job).unwrap()).unwrap();
        assert!(validate_launch_intent(dir.path(), Some(&id), Some(&next)).is_err());
        assert!(validate_launch_intent(dir.path(), None, None).is_ok());
    }
    #[test]
    fn interrupted_pre_cutover_request_closes_without_redispatch() {
        for phase in ["queued", "checking", "draining"] {
            let (dir, id) = fixture(phase);
            let result = queue(
                dir.path(),
                Command {
                    action: "recover".into(),
                    request_id: uuid::Uuid::new_v4(),
                    plan_id: Some(uuid::Uuid::parse_str(&id).unwrap()),
                },
            )
            .unwrap();
            assert_eq!(result["state"], "failed");
            assert_eq!(result["reason"], "interrupted_before_cutover");
            assert!(
                !dir.path()
                    .join("native-updates")
                    .join(&id)
                    .join("worker.py")
                    .exists()
            );
        }
    }
    #[test]
    fn active_worker_cannot_be_recovered_and_critical_plan_cannot_be_replaced() {
        let (dir, id) = fixture("checking");
        let guard = worker_guard(dir.path()).unwrap();
        assert!(
            queue(
                dir.path(),
                Command {
                    action: "recover".into(),
                    request_id: uuid::Uuid::new_v4(),
                    plan_id: Some(uuid::Uuid::parse_str(&id).unwrap())
                }
            )
            .is_err()
        );
        drop(guard);
        let (dir, _) = fixture("recovery_required");
        assert!(
            queue(
                dir.path(),
                Command {
                    action: "check".into(),
                    request_id: uuid::Uuid::new_v4(),
                    plan_id: None
                }
            )
            .is_err()
        );
        assert!(blocks(dir.path()));
    }
    #[test]
    fn critical_recovery_worker_failure_never_opens_admission() {
        let (dir, id) = fixture("recovery_required");
        let folder = dir.path().join("native-updates").join(&id);
        std::fs::create_dir_all(folder.join("backup")).unwrap();
        std::fs::write(folder.join("backup/binary"), b"retained").unwrap();
        // A missing worker executable reproduces failed dispatch after durable intent.
        std::fs::create_dir_all(folder.join("worker-runtime")).unwrap();
        let result = queue(
            dir.path(),
            Command {
                action: "recover".into(),
                request_id: uuid::Uuid::new_v4(),
                plan_id: Some(uuid::Uuid::parse_str(&id).unwrap()),
            },
        )
        .unwrap();
        assert_eq!(result["state"], "recovery_required");
        assert_eq!(result["critical_recovery"], true);
        assert!(blocks(dir.path()));
    }
    #[test]
    fn recovery_receipt_is_idempotent_and_does_not_accept_other_action() {
        let (dir, id) = fixture("queued");
        let request_id = uuid::Uuid::new_v4();
        let command = || Command {
            action: "recover".into(),
            request_id,
            plan_id: Some(uuid::Uuid::parse_str(&id).unwrap()),
        };
        let first = queue(dir.path(), command()).unwrap();
        assert_eq!(first, queue(dir.path(), command()).unwrap());
        assert!(
            queue(
                dir.path(),
                Command {
                    action: "check".into(),
                    request_id,
                    plan_id: None
                }
            )
            .is_err()
        );
    }
    #[test]
    fn updater_stop_requires_exact_admitted_plan_uuid_config_and_binary() {
        use sha2::{Digest, Sha256};
        let (dir, id) = fixture("stopping");
        let request = uuid::Uuid::new_v4().to_string();
        let mut c = Config::new("Fixture".into(), "owner".into());
        c.server.auth_mode = AuthMode::LocalPassword;
        c.server.web_transport = WebTransport::LocalHttp;
        c.save(dir.path()).unwrap();
        config::private_write(
            &dir.path().join("native-installation.json"),
            serde_json::to_vec(&json!({"id":c.id,"update_owner":"installation"}))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let binary =
            amplifier_native_platform::installed_binary(&crate::desktop::APPLICATION, dir.path());
        config::private_dir(binary.parent().unwrap()).unwrap();
        config::private_write(&binary, b"fixture executable").unwrap();
        let config_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(dir.path().join("adapter.json")).unwrap())
        );
        let binary_hash = format!("{:x}", Sha256::digest(std::fs::read(&binary).unwrap()));
        let file = dir
            .path()
            .join("native-updates")
            .join(&id)
            .join("plan.json");
        let mut job = read(&file).unwrap();
        job["action"] = json!("apply");
        job["manager_stop"] =
            json!({"request_id":request,"dispatch":"intent","config_sha256":config_hash});
        job["config_sha256"] = json!(config_hash);
        job["installed_sha256"] = json!(binary_hash);
        let put =
            |v: &Value| config::private_write(&file, &serde_json::to_vec(v).unwrap()).unwrap();
        put(&job);
        assert_eq!(
            validate_stop_intent(dir.path(), &id, &request).unwrap(),
            job
        );
        assert!(
            validate_stop_intent(dir.path(), &uuid::Uuid::new_v4().to_string(), &request).is_err()
        );
        assert!(validate_stop_intent(dir.path(), &id, &uuid::Uuid::new_v4().to_string()).is_err());
        for (key, value) in [
            ("state", json!("recovery_required")),
            ("action", json!("check")),
            ("config_sha256", json!("changed")),
            ("installed_sha256", json!("changed")),
            (
                "manager_stop",
                json!({"request_id":request,"dispatch":"unknown"}),
            ),
        ] {
            let mut changed = job.clone();
            changed[key] = value;
            put(&changed);
            assert!(
                validate_stop_intent(dir.path(), &id, &request).is_err(),
                "accepted {key}"
            );
        }
        job["state"] = json!("rolling_back");
        job["previous_sha256"] = json!(binary_hash);
        put(&job);
        assert!(validate_stop_intent(dir.path(), &id, &request).is_ok());
        // Explicit Restore retains settings changed after the original CHECK;
        // its own persisted stop intent still binds the exact current bytes.
        c.label = "Changed current settings".into();
        c.save(dir.path()).unwrap();
        job["manager_stop"]["config_sha256"] = json!(format!(
            "{:x}",
            Sha256::digest(std::fs::read(dir.path().join("adapter.json")).unwrap())
        ));
        put(&job);
        assert!(validate_stop_intent(dir.path(), &id, &request).is_ok());
        c.label = "Changed after stop intent".into();
        c.save(dir.path()).unwrap();
        assert!(validate_stop_intent(dir.path(), &id, &request).is_err());
        c.label = "Changed current settings".into();
        c.save(dir.path()).unwrap();
        std::fs::write(&binary, b"changed executable").unwrap();
        assert!(validate_stop_intent(dir.path(), &id, &request).is_err());
    }
    #[test]
    fn cleanup_public_projection_is_exact_and_never_exposes_private_response() {
        let plan = uuid::Uuid::new_v4();
        let request = uuid::Uuid::new_v4();
        let success = cleanup_projection(
            plan,
            request,
            false,
            Ok(json!({"retired":true,"home":"private-path","finished":{"secret":"not public"}})),
        );
        assert_eq!(success["cleanup"], "complete");
        assert_eq!(success["plan_id"], plan.to_string());
        assert_eq!(success["request_id"], request.to_string());
        assert_eq!(success["proof_basis"], CLEANUP_PROOF);
        assert_eq!(success["full_generation_observed"], false);
        assert!(!success.to_string().contains("private-path"));
        assert!(!success.to_string().contains("secret"));
        let unknown = cleanup_projection(
            plan,
            request,
            true,
            Err(anyhow::anyhow!("private-path secret")),
        );
        assert_eq!(unknown["cleanup"], "unknown");
        assert_eq!(unknown["observation_only"], true);
        assert!(unknown["proof_basis"].is_null());
        assert!(!unknown.to_string().contains("private-path"));
    }
    #[test]
    fn cleanup_inspection_without_intent_never_creates_files_or_dispatches() {
        let dir = tempfile::tempdir().unwrap();
        let plan = uuid::Uuid::new_v4();
        let request = uuid::Uuid::new_v4();
        let value = command_value(
            dir.path(),
            Command {
                action: "inspect_worker_retirement".into(),
                request_id: request,
                plan_id: Some(plan),
            },
        )
        .unwrap();
        assert_eq!(value["cleanup"], "unknown");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[test]
    fn worker_finished_requires_same_entered_process_and_known_terminal_state() {
        let entered = json!({"schema":1,"plan_id":"plan","request_id":"request","pid":42});
        let mut finished = entered.clone();
        for state in ["ready", "deferred", "succeeded", "rolled_back", "failed"] {
            finished["state"] = json!(state);
            validate_finished_worker(&entered, &finished, "plan", "request").unwrap();
        }
        for (field, value) in [
            ("state", json!("recovery_required")),
            ("state", json!("unknown")),
            ("pid", json!(0)),
            ("pid", json!(43)),
            ("pid", Value::Null),
            ("schema", json!(2)),
            ("plan_id", json!("other")),
            ("request_id", json!("other")),
        ] {
            let mut invalid = finished.clone();
            invalid[field] = value;
            assert!(
                validate_finished_worker(&entered, &invalid, "plan", "request").is_err(),
                "accepted {field}"
            );
        }
        let mut bad_entry = entered.clone();
        bad_entry["pid"] = json!(43);
        assert!(validate_finished_worker(&bad_entry, &finished, "plan", "request").is_err());
    }
    #[test]
    fn one_shot_worker_profile_is_fixed_and_unknown_dispatch_cannot_repeat() {
        use sha2::{Digest, Sha256};
        let (dir, plan) = fixture("queued");
        let request = uuid::Uuid::new_v4().to_string();
        let mut c = Config::new("Fixture".into(), "owner".into());
        c.server.auth_mode = AuthMode::LocalPassword;
        c.server.web_transport = WebTransport::LocalHttp;
        c.save(dir.path()).unwrap();
        amplifier_native_platform::private_dir(dir.path()).unwrap();
        config::private_write(&dir.path().join("native-installation.json"),serde_json::to_vec(&json!({"id":c.id,"update_owner":"installation","manager_owner":crate::native_manager::OWNER})).unwrap().as_slice()).unwrap();
        let folder = dir.path().join("native-updates").join(&plan);
        let entry = if cfg!(windows) {
            "pythonw.exe"
        } else {
            "bin/python3"
        };
        let python = folder.join("worker-runtime/python").join(entry);
        config::private_dir(python.parent().unwrap()).unwrap();
        config::private_write(&python, b"fixed inert interpreter").unwrap();
        config::private_write(&folder.join("worker-runtime/native-bundle.json"),&serde_json::to_vec(&json!({"python_files":{entry:format!("{:x}",Sha256::digest(b"fixed inert interpreter"))}})).unwrap()).unwrap();
        let helper = folder.join(format!("worker-{request}.py"));
        config::private_write(&helper, SCRIPT).unwrap();
        let binding = managed_worker(dir.path(), &plan, &request).unwrap();
        assert!(!binding.profile.autostart);
        assert_eq!(binding.profile.executable, python.canonicalize().unwrap());
        assert_eq!(binding.profile.args[6], plan);
        assert_eq!(binding.profile.args[7], request);
        assert_eq!(binding.profile.cwd, folder.canonicalize().unwrap());
        let record = json!({"schema":1,"plan_id":plan,"request_id":request,"binding":binding,"phase":"unknown","outcome":"unknown"});
        worker_record_write(&worker_dispatch_path(dir.path(), &plan, &request), &record).unwrap();
        assert_eq!(
            dispatch_managed_worker(dir.path(), &plan, &request).unwrap(),
            record
        );
        // The retained unknown request is not reinterpreted as fresh admission.
        config::private_write(&helper, b"changed helper").unwrap();
        assert!(dispatch_managed_worker(dir.path(), &plan, &request).is_err());
    }
}
