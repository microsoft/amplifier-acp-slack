//! Fixed, installation-owned native service. No caller-selected service, command,
//! or URL is accepted. A lost response is reconciled from the same request; it is
//! never permission to repeat registration or start.
use crate::{
    config::{AuthMode, Config, WebTransport},
    desktop, native_update,
};
use amplifier_native_platform::{
    self as platform,
    installation::{self, ActionOutcome, Service, ServiceAction, ServiceManager},
    locking::InstallationLock,
    registration::{self, Registration, RegistrationOutcome, RetirementOutcome},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

pub const OWNER: &str = "slack-native-manager/1";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateLaunch {
    pub plan: String,
    pub nonce: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Binding {
    id: String,
    home: PathBuf,
    service: Service,
    profile: Registration,
    binary_sha256: String,
    update: Option<UpdateLaunch>,
}
fn read(path: &Path) -> Result<Value> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink() && m.len() <= 4 * 1024 * 1024,
        "Invalid native manager record"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn optional(path: &Path) -> Result<Option<Value>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(_) => read(path).map(Some),
    }
}
fn write(path: &Path, value: &Value) -> Result<()> {
    platform::private_write_data(path, &serde_json::to_vec_pretty(value)?)
}
pub fn is_managed(home: &Path) -> Result<bool> {
    Ok(optional(&home.join("native-installation.json"))?
        .is_some_and(|v| v["manager_owner"] == OWNER))
}
fn binding(home: &Path, update: Option<UpdateLaunch>) -> Result<Binding> {
    let home = home.canonicalize()?;
    platform::validate_private_directory(&home)?;
    let c = Config::load(&home)?;
    let marker = read(&home.join("native-installation.json"))?;
    ensure!(
        marker["id"] == c.id
            && marker["update_owner"] == "installation"
            && marker["manager_owner"] == OWNER
            && c.server.auth_mode == AuthMode::LocalPassword
            && c.server.web_transport == WebTransport::LocalHttp,
        "This home was not created for the native manager; existing installation preserved"
    );
    ensure!(
        !home.join("saved-connections.json").exists()
            && !home.join("updates/registered-service.json").exists(),
        "Existing host enrollment owns this installation"
    );
    let id = Uuid::parse_str(&c.id)?;
    let label = format!("acp-slack-adapter-{id}");
    let user = platform::user_home()?;
    #[cfg(target_os = "linux")]
    let service = Service {
        manager: ServiceManager::SystemdUser,
        name: format!("{label}.service"),
        definition_path: Some(
            user.join(".config/systemd/user")
                .join(format!("{label}.service")),
        ),
    };
    #[cfg(target_os = "macos")]
    let service = Service {
        manager: ServiceManager::LaunchAgent,
        name: label.clone(),
        definition_path: Some(
            user.join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
        ),
    };
    #[cfg(windows)]
    let service = {
        let _ = user;
        Service {
            manager: ServiceManager::ScheduledTask,
            name: label,
            definition_path: Some(home.join("native-manager/service.xml")),
        }
    };
    let executable = platform::installed_binary(&desktop::APPLICATION, &home);
    let m = fs::symlink_metadata(&executable)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "Installed executable changed"
    );
    let binary_sha256 = format!("{:x}", Sha256::digest(fs::read(&executable)?));
    let profile = Registration {
        executable,
        args: vec![
            "--home".into(),
            home.to_str().context("Invalid native home")?.into(),
            "native-serve".into(),
        ],
        cwd: home.clone(),
        description: "Constellation Slack Connector".into(),
        autostart: false,
    };
    Ok(Binding {
        id: c.id,
        home,
        service,
        profile,
        binary_sha256,
        update,
    })
}
trait Manager {
    fn exists(&self, b: &Binding) -> Result<bool>;
    fn register(&self, b: &Binding) -> Result<RegistrationOutcome>;
    fn snapshot(&self, b: &Binding) -> Result<Value>;
    fn matches(&self, b: &Binding, s: &Value) -> Result<bool>;
    fn start(&self, b: &Binding, s: &Value) -> Result<ActionOutcome>;
    fn stop(&self, b: &Binding, s: &Value, update_stop: Option<&Value>) -> Result<ActionOutcome>;
    fn process_matches(&self, process: &Value) -> Result<bool>;
    fn process_relationship(
        &self,
        b: &Binding,
        s: &Value,
        process: &Value,
    ) -> Result<Option<Value>>;
    fn retire(&self, b: &Binding, s: &Value) -> Result<RetirementOutcome>;
    fn retired(&self, b: &Binding) -> Result<bool>;
    async fn api_status(&self, b: &Binding) -> Result<Value>;
}
struct Native;
const LIMIT: Duration = Duration::from_secs(60);
// The accepted shared implementation bounds a Windows COM call at 8 seconds;
// Unix command calls take 3 seconds plus at most 1 second of child cleanup.
// Reserve each whole shared boundary (including its internal observations), not
// an optimistic budget for a multi-call status loop. Filesystem/OS stalls are
// still reported as unknown by the supervising CLI client, never safe retries.
#[derive(Clone, Copy)]
enum Boundary {
    Exists,
    Register,
    Snapshot,
    Action,
    Retire,
    Api,
}
fn reserve(boundary: Boundary) -> Duration {
    let secs = match boundary {
        Boundary::Exists => 9,
        Boundary::Register | Boundary::Snapshot => {
            if cfg!(target_os = "macos") {
                25
            } else {
                17
            }
        }
        Boundary::Action => {
            if cfg!(target_os = "macos") {
                33
            } else {
                25
            }
        }
        Boundary::Retire => {
            if cfg!(target_os = "macos") {
                45
            } else {
                37
            }
        }
        Boundary::Api => 6,
    };
    Duration::from_secs(secs)
}
struct TimedNative {
    end: std::time::Instant,
}
impl TimedNative {
    fn check(&self, boundary: Boundary) -> Result<()> {
        ensure!(
            self.end
                .saturating_duration_since(std::time::Instant::now())
                >= reserve(boundary),
            "Native operation budget exhausted before next boundary; inspect the original request"
        );
        Ok(())
    }
}
/// Same fixed owner budget for one-shot worker manager boundaries.
pub(crate) struct NativeBudget(TimedNative);
impl NativeBudget {
    pub(crate) fn new() -> Self {
        Self(TimedNative {
            end: std::time::Instant::now() + LIMIT,
        })
    }
    pub(crate) fn exists(&self) -> Result<()> {
        self.0.check(Boundary::Exists)
    }
    pub(crate) fn register(&self) -> Result<()> {
        self.0.check(Boundary::Register)
    }
    pub(crate) fn snapshot(&self) -> Result<()> {
        self.0.check(Boundary::Snapshot)
    }
    pub(crate) fn action(&self) -> Result<()> {
        self.0.check(Boundary::Action)
    }
    pub(crate) fn retire(&self) -> Result<()> {
        self.0.check(Boundary::Retire)
    }
}
impl Manager for TimedNative {
    fn exists(&self, b: &Binding) -> Result<bool> {
        self.check(Boundary::Exists)?;
        Native.exists(b)
    }
    fn register(&self, b: &Binding) -> Result<RegistrationOutcome> {
        self.check(Boundary::Register)?;
        Native.register(b)
    }
    fn snapshot(&self, b: &Binding) -> Result<Value> {
        self.check(Boundary::Snapshot)?;
        Native.snapshot(b)
    }
    fn matches(&self, b: &Binding, s: &Value) -> Result<bool> {
        Native.matches(b, s)
    }
    fn start(&self, b: &Binding, s: &Value) -> Result<ActionOutcome> {
        self.check(Boundary::Action)?;
        Native.start(b, s)
    }
    fn stop(&self, b: &Binding, s: &Value, update_stop: Option<&Value>) -> Result<ActionOutcome> {
        self.check(Boundary::Action)?;
        Native.stop(b, s, update_stop)
    }
    fn process_matches(&self, process: &Value) -> Result<bool> {
        Native.process_matches(process)
    }
    fn process_relationship(
        &self,
        b: &Binding,
        s: &Value,
        process: &Value,
    ) -> Result<Option<Value>> {
        Native.process_relationship(b, s, process)
    }
    fn retire(&self, b: &Binding, s: &Value) -> Result<RetirementOutcome> {
        self.check(Boundary::Retire)?;
        Native.retire(b, s)
    }
    fn retired(&self, b: &Binding) -> Result<bool> {
        self.check(Boundary::Exists)?;
        Native.retired(b)
    }
    async fn api_status(&self, b: &Binding) -> Result<Value> {
        self.check(Boundary::Api)?;
        Native.api_status(b).await
    }
}

impl Manager for Native {
    fn exists(&self, b: &Binding) -> Result<bool> {
        installation::manager_registration_exists(&b.service)
    }
    fn register(&self, b: &Binding) -> Result<RegistrationOutcome> {
        registration::register(&b.service, &b.profile)
    }
    fn snapshot(&self, b: &Binding) -> Result<Value> {
        installation::snapshot(&b.service)
    }
    fn matches(&self, b: &Binding, s: &Value) -> Result<bool> {
        registration::profile_matches(&b.service, &b.profile, s)
    }
    fn start(&self, b: &Binding, s: &Value) -> Result<ActionOutcome> {
        installation::request_state_checked(&b.service, ServiceAction::Start, s, |_| {
            if binding(&b.home, b.update.clone())? != *b {
                return Ok(Some(ActionOutcome::Refused {
                    code: "installation_changed".into(),
                    message: "Installation changed before start".into(),
                }));
            }
            validate_update(&b.home, b.update.as_ref())?;
            Ok(None)
        })
    }
    fn stop(&self, b: &Binding, s: &Value, update_stop: Option<&Value>) -> Result<ActionOutcome> {
        installation::request_state_checked(&b.service, ServiceAction::Stop, s, |current| {
            if binding(&b.home, None)? != *b || !self.matches(b, current)? {
                return Ok(Some(ActionOutcome::Refused {
                    code: "installation_changed".into(),
                    message: "Installation changed before stop".into(),
                }));
            }
            if let Some(job) = update_stop {
                let current = native_update::validate_stop_intent(
                    &b.home,
                    job["id"].as_str().context("Missing update plan")?,
                    job["manager_stop"]["request_id"]
                        .as_str()
                        .context("Missing stop UUID")?,
                )?;
                ensure!(
                    current == *job
                        && active_request(&b.home)?.is_some_and(
                            |r| r["request_id"] == job["manager_stop"]["start_request_id"]
                        ),
                    "Update stop intent changed before dispatch"
                );
            }
            Ok(None)
        })
    }
    fn process_matches(&self, process: &Value) -> Result<bool> {
        platform::process::matches(process)
    }
    fn process_relationship(
        &self,
        b: &Binding,
        s: &Value,
        process: &Value,
    ) -> Result<Option<Value>> {
        process_relationship(
            &b.service.manager,
            s,
            process,
            platform::process::parent_identity,
        )
    }
    fn retire(&self, b: &Binding, s: &Value) -> Result<RetirementOutcome> {
        registration::retire(&b.service, &b.profile, s)
    }
    fn retired(&self, b: &Binding) -> Result<bool> {
        registration::is_retired(&b.service)
    }
    async fn api_status(&self, b: &Binding) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(5),
            desktop::local_api(&b.home, "GET", "status", None),
        )
        .await?
    }
}
fn validate_update(home: &Path, update: Option<&UpdateLaunch>) -> Result<()> {
    if let Some(u) = update {
        ensure!(
            Uuid::parse_str(&u.plan)?.to_string() == u.plan
                && Uuid::parse_str(&u.nonce)?.to_string() == u.nonce,
            "Invalid update launch identity"
        );
        native_update::validate_launch_intent(home, Some(&u.plan), Some(&u.nonce))?;
    }
    Ok(())
}
fn request_path(home: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        Uuid::parse_str(id)?.to_string() == id,
        "Invalid native start request"
    );
    Ok(home
        .join("native-manager/requests")
        .join(format!("{id}.json")))
}
fn active_request(home: &Path) -> Result<Option<Value>> {
    let Some(p) = optional(&home.join("native-manager/active.json"))? else {
        return Ok(None);
    };
    let id = p["request_id"]
        .as_str()
        .context("Invalid native manager pointer")?;
    let r = read(&request_path(home, id)?)?;
    ensure!(r["request_id"] == id, "Native manager request changed");
    Ok(Some(r))
}
fn save_request(home: &Path, r: &Value) -> Result<()> {
    write(
        &request_path(home, r["request_id"].as_str().context("Missing request")?)?,
        r,
    )
}
fn finished(r: &Value) -> bool {
    matches!(
        r["phase"].as_str(),
        Some("ready" | "refused" | "registration_refused" | "registered")
    )
}
fn same_authority(old: &Value, b: &Binding) -> bool {
    old["binding"]["id"] == b.id
        && old["binding"]["home"] == json!(b.home)
        && old["binding"]["service"] == json!(b.service)
        && old["binding"]["profile"] == json!(b.profile)
}
/// The only mutating state machine. Intent is durable before each native call.
/// Re-entry returns existing evidence without calling register/start again.
fn admit(m: &impl Manager, b: &Binding, id: &str) -> Result<Value> {
    let path = request_path(&b.home, id)?;
    if let Some(r) = optional(&path)? {
        ensure!(
            r["binding"] == json!(b),
            "Request identity cannot be reused for another installation or launch"
        );
        let mut r = r;
        if (r["phase"] == "registering"
            || (r["phase"] == "unknown" && r["outcome"]["boundary"] == "registration"))
            && m.exists(b)?
        {
            let observed = m.snapshot(b)?;
            if m.matches(b, &observed)? && inactive(&observed) {
                r["phase"] = json!("registered");
                save_request(&b.home, &r)?;
            }
        }
        return Ok(r);
    }
    if let Some(control) = active_control(&b.home)? {
        ensure!(
            control["phase"] == "inactive" || control["phase"] == "refused",
            "Previous lifecycle control is unresolved or retired; inspect its original request"
        );
    }
    if let Some(old) = active_request(&b.home)? {
        ensure!(
            same_authority(&old, b),
            "Previous native start belongs to another profile"
        );
        let stopped = active_control(&b.home)?.is_some_and(|c| {
            c["phase"] == "inactive" && c["start_request_id"] == old["request_id"]
        });
        ensure!(
            finished(&old)
                || stopped
                || (b.update.is_some() && old["binding"]["update"] != json!(b.update)),
            "Previous native start is unresolved; reconcile its original request"
        );
        validate_update(&b.home, b.update.as_ref())?;
        let prior = m.snapshot(b)?;
        ensure!(
            m.matches(b, &prior)? && inactive(&prior),
            "Previous service is not confirmed inactive; no new start"
        );
        let _idle = InstallationLock::try_acquire(&b.home.join("service.lock"))?
            .context("Existing connector still owns the home")?;
    }
    let enrollment = b.home.join("native-manager/enrollment.json");
    let expected =
        json!({"schema":1,"id":b.id,"home":b.home,"service":b.service,"profile":b.profile});
    let registered = if let Some(existing) = optional(&enrollment)? {
        ensure!(existing == expected, "Native manager enrollment changed");
        let snapshot = m.snapshot(b)?;
        ensure!(
            m.matches(b, &snapshot)? && inactive(&snapshot),
            "Existing owned service is not inactive or its profile changed"
        );
        true
    } else {
        ensure!(
            !m.exists(b)?,
            "An existing service owns this name; it was preserved"
        );
        // Refuse adoption of a directly running connector, not just a manager unit.
        let _idle = InstallationLock::try_acquire(&b.home.join("service.lock"))?
            .context("An existing connector is already running")?;
        platform::private_create(&enrollment, &serde_json::to_vec_pretty(&expected)?)?;
        false
    };
    let mut r = json!({"schema":1,"request_id":id,"binding":b,"phase":if registered {"registered"}else{"registering"}});
    platform::private_create(&path, &serde_json::to_vec_pretty(&r)?)?;
    write(
        &b.home.join("native-manager/active.json"),
        &json!({"request_id":id}),
    )?;
    ensure!(
        binding(&b.home, b.update.clone())? == *b,
        "Installation changed before registration"
    );
    if !registered {
        match m.register(b) {
            Ok(RegistrationOutcome::Registered { .. }) => (),
            Ok(RegistrationOutcome::Refused { code, message }) => {
                r["phase"] = json!("registration_refused");
                r["outcome"] = json!({"code":code,"message":message});
                save_request(&b.home, &r)?;
                return Ok(r);
            }
            result => {
                r["phase"] = json!("unknown");
                r["outcome"] = json!({"boundary":"registration","detail":format!("{result:?}")});
                save_request(&b.home, &r)?;
                return Ok(r);
            }
        }
    }
    r["phase"] = json!("registered");
    save_request(&b.home, &r)?;
    let snapshot = m.snapshot(b)?;
    ensure!(
        m.matches(b, &snapshot)?,
        "Effective service profile changed"
    );
    if snapshot["active"] == true {
        r["phase"] = json!("observing_existing");
        save_request(&b.home, &r)?;
        return Ok(r);
    }
    ensure!(
        snapshot["active"] == false && snapshot["transitioning"] == false,
        "Service is not confirmed inactive"
    );
    // Confirm no unmanaged connector owns the home. Release before manager
    // dispatch: the server itself takes service.lock and refuses a raced peer.
    let _idle = InstallationLock::try_acquire(&b.home.join("service.lock"))?
        .context("Another connector already owns the home")?;
    r["phase"] = json!("start_intent");
    r["before"] = snapshot.clone();
    save_request(&b.home, &r)?;
    ensure!(
        binding(&b.home, b.update.clone())? == *b,
        "Installation changed before dispatch"
    );
    validate_update(&b.home, b.update.as_ref())?;
    drop(_idle);
    match m.start(b, &snapshot) {
        Ok(ActionOutcome::Accepted) => r["phase"] = json!("accepted"),
        Ok(ActionOutcome::Refused { code, message }) => {
            r["phase"] = json!("refused");
            r["outcome"] = json!({"code":code,"message":message});
        }
        result => {
            r["phase"] = json!("unknown");
            r["outcome"] = json!({"boundary":"start","detail":format!("{result:?}")});
        }
    }
    save_request(&b.home, &r)?;
    Ok(r)
}
fn inactive(snapshot: &Value) -> bool {
    snapshot["active"] == false
        && snapshot["transitioning"] == false
        && snapshot["processes"].as_array().is_some_and(Vec::is_empty)
}
fn enrollment_matches(b: &Binding) -> Result<bool> {
    Ok(
        optional(&b.home.join("native-manager/enrollment.json"))?.is_some_and(|v| {
            v == json!({"schema":1,"id":b.id,"home":b.home,"service":b.service,"profile":b.profile})
        }),
    )
}
fn witness_process(home: &Path, r: &Value) -> Result<Option<Value>> {
    let id = r["request_id"].as_str().context("Missing native request")?;
    let _ = request_path(home, id)?;
    let Some(witness) = optional(
        &home
            .join("native-manager/requests")
            .join(format!("{id}.launched.json")),
    )?
    else {
        return Ok(None);
    };
    ensure!(
        witness["request_id"] == id && witness["binding"] == r["binding"],
        "Native launch witness changed"
    );
    ensure!(
        witness["process"]["pid"].as_u64().is_some_and(|p| p > 0),
        "Invalid native launch generation"
    );
    Ok(Some(witness["process"].clone()))
}
// Task Scheduler exposes an engine PID, which need not be its Exec action.
// Only a complete live-generation ancestry chain to that exact engine can bind
// the service witness. Other platforms retain direct native process matching.
fn process_relationship(
    manager: &ServiceManager,
    snapshot: &Value,
    process: &Value,
    mut parent: impl FnMut(&Value) -> Result<Option<Value>>,
) -> Result<Option<Value>> {
    let rows = snapshot["processes"]
        .as_array()
        .context("Missing service processes")?;
    let mut current = process.clone();
    let mut chain = vec![current.clone()];
    for depth in 0..=16 {
        if rows.iter().any(|row| row["identity"] == current) {
            return Ok(Some(
                json!({"kind":if depth==0{"direct"}else{"task_engine_ancestor"},"manager_process":current,"application_process":process,"chain":chain}),
            ));
        }
        if *manager != ServiceManager::ScheduledTask || depth == 16 {
            return Ok(None);
        }
        let Some(next) = parent(&current)? else {
            return Ok(None);
        };
        ensure!(!chain.contains(&next), "Process ancestry contains a cycle");
        chain.push(next.clone());
        current = next;
    }
    Ok(None)
}
fn ready_generation(
    m: &impl Manager,
    b: &Binding,
    request: Option<&Value>,
    snapshot: &Value,
    api: &Value,
) -> Result<Option<Value>> {
    let Some(r) = request else { return Ok(None) };
    if !same_authority(r, b)
        || r["binding"]["binary_sha256"] != b.binary_sha256
        || api["id"] != b.id
        || snapshot["active"] != true
    {
        return Ok(None);
    }
    let Some(witness) = witness_process(&b.home, r)? else {
        return Ok(None);
    };
    if api["addresses"]["pid"] != witness["pid"] {
        return Ok(None);
    }
    if m.process_relationship(b, snapshot, &witness)?.is_some() && m.process_matches(&witness)? {
        Ok(Some(witness))
    } else {
        Ok(None)
    }
}
fn status_base(b: &Binding, request: Option<&Value>) -> Value {
    json!({"schema":"com.microsoft.amplifier.slack-native-status/1","id":b.id,"home":b.home,"state":"not_registered","launch_observed":false,"request_current":false,"executable":b.profile.executable,"profile_matches":false,"autostart":false,"startup_change_supported":false,"request":request.map(|r|json!({"request_id":r["request_id"],"phase":r["phase"]})),"service":b.service})
}
async fn status_with(m: &impl Manager, b: &Binding) -> Result<Value> {
    let request = active_request(&b.home)?;
    let control = active_control(&b.home)?;
    let mut result = status_base(b, request.as_ref());
    if optional(&b.home.join("native-manager/enrollment.json"))?.is_some() {
        ensure!(enrollment_matches(b)?, "Native manager enrollment changed");
        if m.retired(b)? {
            result["state"] = json!("retired");
        } else {
            let snapshot = m.snapshot(b)?;
            ensure!(
                m.matches(b, &snapshot)?,
                "Native service definition changed"
            );
            result["profile_matches"] = json!(true);
            result["state"] = json!(if inactive(&snapshot) {
                "inactive"
            } else {
                "starting_or_unknown"
            });
            if snapshot["active"] == true
                && control.as_ref().is_none_or(|c| {
                    c["start_request_id"]
                        != request
                            .as_ref()
                            .map(|r| r["request_id"].clone())
                            .unwrap_or(Value::Null)
                        || c["phase"] == "refused"
                })
                && let Ok(api) = m.api_status(b).await
                && let Some(process) = ready_generation(m, b, request.as_ref(), &snapshot, &api)?
                && m.snapshot(b)? == snapshot
            {
                result["state"] = json!("ready");
                result["pid"] = process["pid"].clone();
                result["process_relationship"] = m
                    .process_relationship(b, &snapshot, &process)?
                    .context("Managed process relationship changed")?;
                result["process_identity"] = process;
                result["version"] = api["version"].clone();
                result["launch_observed"] = json!(true);
            }
        }
    }
    // Request, control and installation bracket all OS/API observations. Generic
    // readiness from a later start cannot settle the captured request.
    if active_request(&b.home)? != request
        || active_control(&b.home)? != control
        || binding(&b.home, b.update.clone())? != *b
    {
        result["state"] = json!("unknown");
        result["launch_observed"] = json!(false);
        result["profile_matches"] = json!(false);
        result.as_object_mut().unwrap().remove("process_identity");
        result
            .as_object_mut()
            .unwrap()
            .remove("process_relationship");
        result.as_object_mut().unwrap().remove("pid");
    } else {
        result["request_current"] = json!(true);
    }
    result["control"] = control
        .as_ref()
        .map(control_projection)
        .unwrap_or(Value::Null);
    Ok(result)
}
/// Read-only: no journal write, lock creation, registration or dispatch.
pub async fn status(home: &Path) -> Result<Value> {
    let timer = TimedNative {
        end: std::time::Instant::now() + LIMIT,
    };
    let b = binding(home, None)?;
    status_with(&timer, &b).await
}
pub async fn start(home: &Path, id: Uuid, update: Option<UpdateLaunch>) -> Result<Value> {
    let timer = TimedNative {
        end: std::time::Instant::now() + LIMIT,
    };
    let b = binding(home, update)?;
    ensure!(
        format!("{:x}", Sha256::digest(fs::read(std::env::current_exe()?)?)) == b.binary_sha256,
        "Start using this installation's recorded executable version"
    );
    platform::private_dir(&b.home.join("native-manager"))?;
    platform::private_dir(&b.home.join("native-manager/requests"))?;
    let _owner = InstallationLock::try_acquire(&b.home.join("native-manager/owner.lock"))?
        .context("Native lifecycle is already being reconciled")?;
    validate_update(&b.home, b.update.as_ref())?;
    let _update_owner = update_guard(&b)?;
    let mut r = admit(&timer, &b, &id.to_string())?;
    let mut last = status_base(&b, Some(&r));
    last["state"] = json!("unknown");
    loop {
        // Each observation boundary reserves its own full composed native cost.
        // A failed observation does not acknowledge or repeat the start.
        match status_with(&timer, &b).await {
            Ok(mut observed) => {
                if observed["state"] == "ready"
                    && observed["request_current"] == true
                    && observed["launch_observed"] == true
                    && observed["request"]["request_id"] == id.to_string()
                {
                    ensure!(
                        binding(&b.home, b.update.clone())? == b
                            && active_request(&b.home)?.as_ref() == Some(&r),
                        "Native start was superseded"
                    );
                    r["phase"] = json!("ready");
                    r["ready_process"] = observed["process_identity"].clone();
                    save_request(&b.home, &r)?;
                    observed["request"] = json!({"request_id":id,"phase":"ready"});
                    observed["dispatch_repeated"] = json!(false);
                    return Ok(observed);
                }
                last = observed;
            }
            Err(_) => break,
        }
        if finished(&r)
            || timer
                .end
                .saturating_duration_since(std::time::Instant::now())
                < reserve(Boundary::Snapshot) + Duration::from_secs(1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    last["request"] = json!({"request_id":id,"phase":r["phase"]});
    last["dispatch_repeated"] = json!(false);
    Ok(last)
}
fn update_guard(b: &Binding) -> Result<Option<InstallationLock>> {
    if b.update.is_none() && b.home.join("native-updates").is_dir() {
        Ok(Some(
            InstallationLock::try_acquire(&b.home.join("native-updates/worker.lock"))?
                .context("Native update worker is active; no competing lifecycle action")?,
        ))
    } else {
        Ok(None)
    }
}
/// Called after the server acquires service.lock, before opening mutable state.
pub fn validate_launch(home: &Path) -> Result<bool> {
    let r = active_request(home)?.context("No native manager launch owns this service")?;
    ensure!(
        matches!(
            r["phase"].as_str(),
            Some("start_intent" | "accepted" | "unknown" | "ready")
        ),
        "Native manager launch was not dispatched"
    );
    let control_before = active_control(home)?;
    ensure!(
        !control_before
            .as_ref()
            .is_some_and(|c| c["start_request_id"] == r["request_id"] && c["phase"] != "refused"),
        "Native launch was fenced by an explicit lifecycle control"
    );
    let expected: Binding = serde_json::from_value(r["binding"].clone())?;
    ensure!(
        binding(home, expected.update.clone())? == expected,
        "Native launch installation changed"
    );
    ensure!(
        std::env::current_exe()?.canonicalize()? == expected.profile.executable.canonicalize()?,
        "Native launch executable path changed"
    );
    let allowed = if let Some(u) = &expected.update {
        native_update::validate_launch_intent(home, Some(&u.plan), Some(&u.nonce))?
    } else {
        native_update::validate_launch_intent(home, None, None)?
    };
    ensure!(
        active_request(home)?.as_ref() == Some(&r) && active_control(home)? == control_before,
        "Native manager launch was superseded"
    );
    let process =
        platform::process::identity(std::process::id())?.context("Service identity unavailable")?;
    let receipt = json!({"request_id":r["request_id"],"binding":expected,"process":process});
    let path = home.join("native-manager/requests").join(format!(
        "{}.launched.json",
        r["request_id"].as_str().context("Missing request")?
    ));
    if let Some(prior) = optional(&path)? {
        ensure!(
            prior == receipt,
            "This native start already launched another process"
        );
    } else {
        platform::private_create(&path, &serde_json::to_vec_pretty(&receipt)?)?;
    }
    Ok(allowed)
}

fn control_path(home: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        Uuid::parse_str(id)?.to_string() == id,
        "Invalid native control request"
    );
    Ok(home
        .join("native-manager/controls")
        .join(format!("{id}.json")))
}
fn active_control(home: &Path) -> Result<Option<Value>> {
    let Some(pointer) = optional(&home.join("native-manager/control.json"))? else {
        return Ok(None);
    };
    let id = pointer["request_id"]
        .as_str()
        .context("Invalid native control pointer")?;
    let r = read(&control_path(home, id)?)?;
    ensure!(r["request_id"] == id, "Native control changed");
    Ok(Some(r))
}
fn control_projection(r: &Value) -> Value {
    json!({"request_id":r["request_id"],"action":r["action"],"phase":r["phase"],"start_request_id":r["start_request_id"],"dispatch_outcome":r["dispatch_outcome"],"observation":r["observation"],"process_identity":r["process_identity"],"process_absent":r["process_absent"],"update_plan":r["update_plan"]})
}
fn save_control(b: &Binding, r: &Value) -> Result<()> {
    write(
        &control_path(
            &b.home,
            r["request_id"].as_str().context("Missing control ID")?,
        )?,
        r,
    )
}
fn control_result(b: &Binding, r: &Value) -> Value {
    let mut v = status_base(b, None);
    let current = active_control(&b.home).ok().flatten().as_ref() == Some(r)
        && active_request(&b.home)
            .ok()
            .flatten()
            .is_some_and(|start| start["request_id"] == r["start_request_id"]);
    v["state"] = if current {
        r["phase"].clone()
    } else {
        json!("superseded")
    };
    v["control_current"] = json!(current);
    v["control"] = control_projection(r);
    v["dispatch_repeated"] = json!(false);
    v
}
fn reconcile_control(m: &impl Manager, b: &Binding, r: &mut Value) -> Result<()> {
    if r["phase"] == "refused" || r["phase"] == "retired" || r["phase"] == "inactive" {
        return Ok(());
    }
    if r["action"] == "retire" && m.retired(b)? {
        let _idle = InstallationLock::try_acquire(&b.home.join("service.lock"))?
            .context("Connector still owns service lock")?;
        if !r["process_identity"].is_null() {
            ensure!(
                !m.process_matches(&r["process_identity"])?,
                "Owned service generation remains alive"
            );
        }
        r["process_absent"] = json!(true);
        r["phase"] = json!("retired");
        r["observation"] = json!("exact_registration_absent");
        save_control(b, r)?;
    } else if r["action"] == "stop" {
        let snapshot = m.snapshot(b)?;
        if m.matches(b, &snapshot)? && inactive(&snapshot) {
            if !r["process_identity"].is_null() {
                ensure!(
                    !m.process_matches(&r["process_identity"])?,
                    "Owned service generation remains alive"
                );
            }
            let _idle = InstallationLock::try_acquire(&b.home.join("service.lock"))?
                .context("Connector still owns service lock")?;
            // Re-read our durable fence: inactive observation never releases it.
            ensure!(
                active_control(&b.home)?.as_ref() == Some(r),
                "Control was superseded"
            );
            r["phase"] = json!("inactive");
            r["process_absent"] = json!(true);
            r["observation"] = json!("exact_profile_inactive_and_service_lock_available");
            save_control(b, r)?;
        }
    }
    Ok(())
}
#[cfg(test)]
fn control_with(m: &impl Manager, b: &Binding, id: Uuid, action: &str) -> Result<Value> {
    control_with_authority(m, b, id, action, None)
}
fn control_with_authority(
    m: &impl Manager,
    b: &Binding,
    id: Uuid,
    action: &str,
    update_stop: Option<&Value>,
) -> Result<Value> {
    ensure!(
        enrollment_matches(b)?,
        "This installation has no exact owned manager profile"
    );
    let path = control_path(&b.home, &id.to_string())?;
    if let Some(mut prior) = optional(&path)? {
        ensure!(
            prior["binding"] == json!(b)
                && prior["action"] == action
                && prior["update_plan"]
                    == update_stop.map(|j| j["id"].clone()).unwrap_or(Value::Null),
            "Control UUID belongs to another action/profile"
        );
        // Re-entry only reads exact postconditions; it never repeats a dispatch.
        if active_control(&b.home)?.as_ref() == Some(&prior) {
            let _ = reconcile_control(m, b, &mut prior);
        }
        return Ok(control_result(b, &prior));
    }
    if let Some(old) = active_control(&b.home)? {
        ensure!(
            old["phase"] == "inactive" || old["phase"] == "refused",
            "Previous control is unresolved or retired; retain its request"
        );
    }
    let request =
        active_request(&b.home)?.context("No original managed start owns this profile")?;
    ensure!(same_authority(&request, b), "Managed start profile changed");
    if let Some(job) = update_stop {
        ensure!(
            action == "stop"
                && job["manager_stop"]["request_id"] == id.to_string()
                && job["manager_stop"]["start_request_id"] == request["request_id"],
            "Update stop start identity changed"
        );
    } else {
        ensure!(
            request["binding"]["binary_sha256"] == b.binary_sha256,
            "Managed executable changed"
        );
    }
    let snapshot = m.snapshot(b)?;
    ensure!(
        request["binding"]["binary_sha256"] == b.binary_sha256 || inactive(&snapshot),
        "Active managed executable changed"
    );
    ensure!(
        m.matches(b, &snapshot)?,
        "Service profile changed; it was preserved"
    );
    let process = witness_process(&b.home, &request)?;
    if action == "retire" {
        if let Some(process) = &process {
            ensure!(
                !m.process_matches(process)?,
                "Owned service generation remains alive"
            );
        }
        ensure!(
            inactive(&snapshot),
            "Retirement requires a confirmed inactive profile"
        );
    }
    if !inactive(&snapshot) {
        let witness = witness_process(&b.home, &request)?
            .context("No exact owned launch generation; no stop dispatched")?;
        ensure!(
            m.process_matches(&witness)?
                && m.process_relationship(b, &snapshot, &witness)?.is_some(),
            "Current service generation is not the owned launch"
        );
    }
    platform::private_dir(&b.home.join("native-manager/controls"))?;
    let mut r = json!({"schema":1,"request_id":id,"action":action,"binding":b,"start_request_id":request["request_id"],"phase":"intent","dispatch_outcome":"unconfirmed","before":snapshot,"process_identity":process,"process_absent":false,"update_plan":update_stop.map(|j|j["id"].clone())});
    platform::private_create(&path, &serde_json::to_vec_pretty(&r)?)?;
    write(
        &b.home.join("native-manager/control.json"),
        &json!({"request_id":id}),
    )?;
    ensure!(
        binding(&b.home, None)? == *b && active_request(&b.home)?.as_ref() == Some(&request),
        "Installation changed before control dispatch"
    );
    if action == "stop" {
        if inactive(&snapshot) {
            r["dispatch_outcome"] = json!("not_needed");
        } else {
            match m.stop(b, &snapshot, update_stop) {
                Ok(ActionOutcome::Accepted) => r["dispatch_outcome"] = json!("accepted"),
                Ok(ActionOutcome::Refused { code, .. }) => {
                    r["phase"] = json!("refused");
                    r["dispatch_outcome"] = json!("refused");
                    r["code"] = json!(code);
                }
                _ => r["dispatch_outcome"] = json!("unknown"),
            }
        }
    } else {
        let _idle = InstallationLock::try_acquire(&b.home.join("service.lock"))?
            .context("Connector still owns service lock")?;
        match m.retire(b, &snapshot) {
            Ok(RetirementOutcome::Retired { .. }) => {
                r["phase"] = json!("retired");
                r["process_absent"] = json!(true);
                r["dispatch_outcome"] = json!("accepted");
                r["observation"] = json!("exact_registration_absent");
            }
            Ok(RetirementOutcome::Refused { code, .. }) => {
                r["phase"] = json!("refused");
                r["dispatch_outcome"] = json!("refused");
                r["code"] = json!(code);
            }
            _ => r["dispatch_outcome"] = json!("unknown"),
        }
    }
    if r["phase"] == "intent" {
        r["phase"] = json!("unknown");
    }
    save_control(b, &r)?;
    let _ = reconcile_control(m, b, &mut r);
    Ok(control_result(b, &r))
}
async fn control(home: &Path, id: Uuid, action: &str) -> Result<Value> {
    control_authority(home, id, action, None).await
}
async fn control_authority(
    home: &Path,
    id: Uuid,
    action: &str,
    update_plan: Option<Uuid>,
) -> Result<Value> {
    let timer = TimedNative {
        end: std::time::Instant::now() + LIMIT,
    };
    let b = binding(home, None)?;
    ensure!(
        format!("{:x}", Sha256::digest(fs::read(std::env::current_exe()?)?)) == b.binary_sha256,
        "Use this installation's recorded executable version"
    );
    let _owner = InstallationLock::try_acquire(&b.home.join("native-manager/owner.lock"))?
        .context("Native lifecycle is already being reconciled")?;
    let update_stop = update_plan
        .map(|plan| {
            native_update::validate_stop_intent(&b.home, &plan.to_string(), &id.to_string())
        })
        .transpose()?;
    let _update = if update_stop.is_some() {
        ensure!(action == "stop", "Update authority is stop-only");
        let lock = b.home.join("native-updates/worker.lock");
        ensure!(lock.is_file(), "No update worker owns this stop");
        ensure!(
            InstallationLock::try_acquire(&lock)?.is_none(),
            "No update worker owns this stop"
        );
        None
    } else {
        update_guard(&b)?
    };
    let mut result = control_with_authority(&timer, &b, id, action, update_stop.as_ref())?;
    while result["state"] == "unknown"
        && timer
            .end
            .saturating_duration_since(std::time::Instant::now())
            >= reserve(Boundary::Snapshot) + Duration::from_secs(1)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
        result = control_with_authority(&timer, &b, id, action, update_stop.as_ref())?;
    }
    Ok(result)
}
pub async fn stop(home: &Path, id: Uuid) -> Result<Value> {
    control(home, id, "stop").await
}
pub async fn stop_for_update(home: &Path, id: Uuid, plan: Uuid) -> Result<Value> {
    control_authority(home, id, "stop", Some(plan)).await
}
pub async fn retire(home: &Path, id: Uuid) -> Result<Value> {
    control(home, id, "retire").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        present: Cell<bool>,
        snapshot: RefCell<Value>,
        registers: Cell<usize>,
        starts: Cell<usize>,
        stops: Cell<usize>,
        retires: Cell<usize>,
        drift: Cell<bool>,
        api: RefCell<Value>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                present: Cell::new(false),
                snapshot: RefCell::new(
                    json!({"active":false,"transitioning":false,"processes":[],"matches":true}),
                ),
                registers: Cell::new(0),
                starts: Cell::new(0),
                stops: Cell::new(0),
                retires: Cell::new(0),
                drift: Cell::new(false),
                api: RefCell::new(Value::Null),
            }
        }
    }
    impl Manager for Fake {
        fn exists(&self, _: &Binding) -> Result<bool> {
            Ok(self.present.get())
        }
        fn register(&self, _: &Binding) -> Result<RegistrationOutcome> {
            self.registers.set(self.registers.get() + 1);
            self.present.set(true);
            Ok(RegistrationOutcome::Registered { created: true })
        }
        fn snapshot(&self, _: &Binding) -> Result<Value> {
            Ok(self.snapshot.borrow().clone())
        }
        fn matches(&self, _: &Binding, s: &Value) -> Result<bool> {
            Ok(s["matches"] == true)
        }
        fn start(&self, _: &Binding, _: &Value) -> Result<ActionOutcome> {
            self.starts.set(self.starts.get() + 1);
            Ok(ActionOutcome::Accepted)
        }
        fn stop(&self, _: &Binding, _: &Value, _: Option<&Value>) -> Result<ActionOutcome> {
            self.stops.set(self.stops.get() + 1);
            *self.snapshot.borrow_mut() =
                json!({"active":false,"transitioning":false,"processes":[],"matches":true});
            Ok(ActionOutcome::Accepted)
        }
        fn process_matches(&self, process: &Value) -> Result<bool> {
            Ok(self.snapshot.borrow()["processes"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|r| r["identity"] == *process)))
        }
        fn process_relationship(
            &self,
            b: &Binding,
            s: &Value,
            process: &Value,
        ) -> Result<Option<Value>> {
            process_relationship(&b.service.manager, s, process, |_| Ok(None))
        }
        fn retire(&self, _: &Binding, _: &Value) -> Result<RetirementOutcome> {
            self.retires.set(self.retires.get() + 1);
            self.present.set(false);
            Ok(RetirementOutcome::Retired { removed: true })
        }
        fn retired(&self, _: &Binding) -> Result<bool> {
            Ok(!self.present.get())
        }
        async fn api_status(&self, b: &Binding) -> Result<Value> {
            if self.drift.get() {
                let mut r = active_request(&b.home)?.unwrap();
                r["phase"] = json!("changed-during-observation");
                save_request(&b.home, &r)?;
            }
            Ok(self.api.borrow().clone())
        }
    }
    fn fixture() -> (tempfile::TempDir, Binding, Fake) {
        let t = tempfile::tempdir().unwrap();
        platform::private_dir(t.path()).unwrap();
        platform::private_dir(&t.path().join("bin")).unwrap();
        platform::private_dir(&t.path().join("native-manager/requests")).unwrap();
        let mut c = Config::new("Fixture".into(), "fixture".into());
        c.server.auth_mode = AuthMode::LocalPassword;
        c.server.web_transport = WebTransport::LocalHttp;
        c.save(t.path()).unwrap();
        write(
            &t.path().join("native-installation.json"),
            &json!({"schema":1,"id":c.id,"update_owner":"installation","manager_owner":OWNER}),
        )
        .unwrap();
        let exe = platform::installed_binary(&desktop::APPLICATION, t.path());
        platform::private_create(&exe, b"exact fixture executable").unwrap();
        let b = binding(t.path(), None).unwrap();
        (t, b, Fake::new())
    }
    fn started(b: &Binding, m: &Fake) -> Value {
        admit(m, b, &Uuid::new_v4().to_string()).unwrap()
    }
    fn ready(b: &Binding, m: &Fake, r: &mut Value) -> Value {
        let process = json!({"pid":123,"creation_time":456});
        r["phase"] = json!("ready");
        save_request(&b.home, r).unwrap();
        write(
            &b.home.join("native-manager/requests").join(format!(
                "{}.launched.json",
                r["request_id"].as_str().unwrap()
            )),
            &json!({"request_id":r["request_id"],"binding":r["binding"],"process":process}),
        )
        .unwrap();
        *m.snapshot.borrow_mut() = json!({"active":true,"transitioning":false,"matches":true,"processes":[{"identity":process}]});
        *m.api.borrow_mut() = json!({"id":b.id,"addresses":{"pid":123},"version":"fixture"});
        process
    }
    #[test]
    fn one_start_request_never_repeats_registration_or_dispatch() {
        let (_t, b, m) = fixture();
        let r = started(&b, &m);
        let same = admit(&m, &b, r["request_id"].as_str().unwrap()).unwrap();
        assert_eq!(r, same);
        assert_eq!(m.registers.get(), 1);
        assert_eq!(m.starts.get(), 1);
        assert!(admit(&m, &b, &Uuid::new_v4().to_string()).is_err());
        assert_eq!(m.starts.get(), 1);
    }
    #[tokio::test]
    async fn ready_binds_full_witness_generation_and_brackets_request() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        ready(&b, &m, &mut r);
        let yes = status_with(&m, &b).await.unwrap();
        assert_eq!(yes["state"], "ready");
        assert_eq!(yes["request_current"], true);
        assert_eq!(yes["launch_observed"], true);
        m.snapshot.borrow_mut()["processes"][0]["identity"]["creation_time"] = json!(789);
        assert_ne!(status_with(&m, &b).await.unwrap()["state"], "ready");
        m.snapshot.borrow_mut()["processes"][0]["identity"]["creation_time"] = json!(456);
        m.drift.set(true);
        let drift = status_with(&m, &b).await.unwrap();
        assert_eq!(drift["state"], "unknown");
        assert_eq!(drift["request_current"], false);
        assert_eq!(drift["launch_observed"], false);
        assert!(drift.get("process_identity").is_none());
    }
    #[test]
    fn explicit_new_start_after_terminal_inactive_reuses_profile_without_registration() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        r["phase"] = json!("ready");
        save_request(&b.home, &r).unwrap();
        let second = admit(&m, &b, &Uuid::new_v4().to_string()).unwrap();
        assert_ne!(second["request_id"], r["request_id"]);
        assert_eq!(m.registers.get(), 1);
        assert_eq!(m.starts.get(), 2);
    }
    #[test]
    fn active_or_changed_profile_cannot_gain_new_start() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        ready(&b, &m, &mut r);
        assert!(admit(&m, &b, &Uuid::new_v4().to_string()).is_err());
        m.snapshot.borrow_mut()["active"] = json!(false);
        m.snapshot.borrow_mut()["processes"] = json!([]);
        m.snapshot.borrow_mut()["matches"] = json!(false);
        assert!(admit(&m, &b, &Uuid::new_v4().to_string()).is_err());
        assert_eq!(m.starts.get(), 1);
    }
    #[test]
    fn checked_stop_and_retire_once_preserve_home_and_old_requests() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        ready(&b, &m, &mut r);
        write(
            &b.home.join("continuity.json"),
            &json!({"saved":"unchanged"}),
        )
        .unwrap();
        let start_before =
            fs::read(request_path(&b.home, r["request_id"].as_str().unwrap()).unwrap()).unwrap();
        let stop_id = Uuid::new_v4();
        let stop = control_with(&m, &b, stop_id, "stop").unwrap();
        assert_eq!(stop["state"], "inactive");
        assert_eq!(stop["control_current"], true);
        assert_eq!(m.stops.get(), 1);
        assert_eq!(
            control_with(&m, &b, stop_id, "stop").unwrap()["state"],
            "inactive"
        );
        assert_eq!(m.stops.get(), 1);
        let retire_id = Uuid::new_v4();
        assert_eq!(
            control_with(&m, &b, retire_id, "retire").unwrap()["state"],
            "retired"
        );
        assert_eq!(
            control_with(&m, &b, retire_id, "retire").unwrap()["state"],
            "retired"
        );
        assert_eq!(m.retires.get(), 1);
        assert_eq!(
            read(&b.home.join("continuity.json")).unwrap()["saved"],
            "unchanged"
        );
        assert_eq!(
            fs::read(request_path(&b.home, r["request_id"].as_str().unwrap()).unwrap()).unwrap(),
            start_before
        );
        assert!(admit(&m, &b, &Uuid::new_v4().to_string()).is_err());
    }
    #[test]
    fn stop_refuses_foreign_generation_and_active_retirement() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        ready(&b, &m, &mut r);
        assert!(control_with(&m, &b, Uuid::new_v4(), "retire").is_err());
        m.snapshot.borrow_mut()["processes"][0]["identity"]["creation_time"] = json!(999);
        assert!(control_with(&m, &b, Uuid::new_v4(), "stop").is_err());
        assert_eq!(m.stops.get(), 0);
        assert!(active_control(&b.home).unwrap().is_none());
    }
    #[test]
    fn same_profile_resume_does_not_replay_old_control_or_start() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        ready(&b, &m, &mut r);
        let stop_id = Uuid::new_v4();
        control_with(&m, &b, stop_id, "stop").unwrap();
        let next = admit(&m, &b, &Uuid::new_v4().to_string()).unwrap();
        assert_ne!(next["request_id"], r["request_id"]);
        assert_eq!(m.registers.get(), 1);
        assert_eq!(m.starts.get(), 2);
        assert_eq!(
            control_with(&m, &b, stop_id, "stop").unwrap()["state"],
            "superseded"
        );
        assert_eq!(m.stops.get(), 1);
    }
    #[test]
    fn budgets_cover_individual_composed_boundaries_and_refuse_before_call() {
        let (_t, b, _) = fixture();
        let timed = TimedNative {
            end: std::time::Instant::now() + Duration::from_millis(5),
        };
        assert!(timed.exists(&b).is_err());
        assert!(timed.register(&b).is_err());
        assert!(timed.snapshot(&b).is_err());
        assert!(timed.start(&b, &Value::Null).is_err());
        assert!(timed.stop(&b, &Value::Null, None).is_err());
        assert!(timed.retire(&b, &Value::Null).is_err());
        assert!(reserve(Boundary::Action) >= Duration::from_secs(25));
        assert!(reserve(Boundary::Retire) >= Duration::from_secs(37));
    }
    #[tokio::test]
    async fn unresolved_stop_is_fenced_and_never_redispatched() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        ready(&b, &m, &mut r);
        let id = Uuid::new_v4();
        platform::private_dir(&b.home.join("native-manager/controls")).unwrap();
        let control = json!({"schema":1,"request_id":id,"action":"stop","binding":b,"start_request_id":r["request_id"],"phase":"unknown","dispatch_outcome":"unknown"});
        save_control(&b, &control).unwrap();
        write(
            &b.home.join("native-manager/control.json"),
            &json!({"request_id":id}),
        )
        .unwrap();
        assert_ne!(status_with(&m, &b).await.unwrap()["state"], "ready");
        assert_eq!(
            control_with(&m, &b, id, "stop").unwrap()["state"],
            "unknown"
        );
        assert_eq!(m.stops.get(), 0);
        assert!(control_with(&m, &b, Uuid::new_v4(), "stop").is_err());
        assert!(admit(&m, &b, &Uuid::new_v4().to_string()).is_err());
    }
    #[test]
    fn updater_stop_binds_original_start_and_proves_generation_exit_without_health() {
        let (_t, b, m) = fixture();
        let mut r = started(&b, &m);
        let process = ready(&b, &m, &mut r);
        *m.api.borrow_mut() = Value::Null;
        let id = Uuid::new_v4();
        let mut job = json!({"id":Uuid::new_v4(),"manager_stop":{"request_id":id,"start_request_id":Uuid::new_v4(),"dispatch":"intent"}});
        assert!(control_with_authority(&m, &b, id, "stop", Some(&job)).is_err());
        assert_eq!(m.stops.get(), 0);
        job["manager_stop"]["start_request_id"] = r["request_id"].clone();
        let result = control_with_authority(&m, &b, id, "stop", Some(&job)).unwrap();
        assert_eq!(result["state"], "inactive");
        assert_eq!(result["control"]["process_identity"], process);
        assert_eq!(result["control"]["process_absent"], true);
        assert_eq!(result["control"]["update_plan"], job["id"]);
        assert_eq!(m.stops.get(), 1);
        control_with_authority(&m, &b, id, "stop", Some(&job)).unwrap();
        assert_eq!(m.stops.get(), 1);
        let mut wrong = job;
        wrong["id"] = json!(Uuid::new_v4());
        assert!(control_with_authority(&m, &b, id, "stop", Some(&wrong)).is_err());
    }
    #[test]
    fn windows_task_engine_ancestry_requires_exact_generations_and_bounded_chain() {
        let app = json!({"pid":1,"birth":11});
        let middle = json!({"pid":2,"birth":22});
        let engine = json!({"pid":3,"birth":33});
        let snapshot = json!({"processes":[{"identity":engine}]});
        let relation = process_relationship(&ServiceManager::ScheduledTask, &snapshot, &app, |p| {
            Ok(if *p == app {
                Some(middle.clone())
            } else if *p == middle {
                Some(engine.clone())
            } else {
                None
            })
        })
        .unwrap()
        .unwrap();
        assert_eq!(relation["kind"], "task_engine_ancestor");
        assert_eq!(relation["chain"], json!([app, middle, engine]));
        assert!(
            process_relationship(&ServiceManager::SystemdUser, &snapshot, &app, |_| panic!(
                "Non-Windows ancestor traversal"
            ))
            .unwrap()
            .is_none()
        );
        assert!(
            process_relationship(&ServiceManager::ScheduledTask, &snapshot, &app, |_| Ok(
                Some(json!({"pid":3,"birth":99}))
            ))
            .is_err()
        );
        let mut count = 0;
        assert!(
            process_relationship(&ServiceManager::ScheduledTask, &snapshot, &app, |_| {
                count += 1;
                Ok(Some(json!({"pid":100+count,"birth":count})))
            })
            .unwrap()
            .is_none()
        );
        assert_eq!(count, 16);
    }
}
