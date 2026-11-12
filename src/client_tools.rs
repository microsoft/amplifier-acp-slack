//! Explicit local ACP client tools. No mapping of arbitrary remote paths.
use crate::{
    config::{ClientCapabilities, Config},
    process,
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
#[cfg(unix)]
use std::path::Component;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Notify,
};

pub struct ClientTools {
    policy: ClientCapabilities,
    roots: Vec<PathBuf>,
    cwd: PathBuf,
    home: PathBuf,
    session: String,
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
}
struct Terminal {
    state: Arc<Mutex<TerminalState>>,
    notify: Arc<Notify>,
    tree: Arc<Mutex<Option<process::ProcessTree>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.tree.lock().unwrap().take();
        self.task.abort();
    }
}
#[derive(Default)]
struct TerminalState {
    output: Vec<u8>,
    truncated: bool,
    limit: usize,
    exit: Option<Value>,
}
impl TerminalState {
    fn append(&mut self, bytes: &[u8], limit: usize) {
        self.limit = limit;
        self.output.extend_from_slice(bytes);
        if self.output.len() > limit {
            self.truncated = true;
            let mut start = self.output.len() - limit;
            while start < self.output.len() && self.output[start] & 0xc0 == 0x80 {
                start += 1;
            }
            self.output.drain(..start);
        }
    }
}
pub fn advertised(c: &Config) -> Value {
    let mut caps = json!({});
    if c.acp.client.read_text_file || c.acp.client.write_text_file || c.acp.client.terminal {
        caps = json!({"fs":{"readTextFile":c.acp.client.read_text_file && cfg!(unix),"writeTextFile":c.acp.client.write_text_file && cfg!(unix)},"terminal":c.acp.client.terminal && cfg!(target_os="linux")});
    }
    // Questions belong to the initiating human, independent of tool approval policy.
    caps["elicitation"] = json!({"form":{},"url":{}});
    caps
}
pub fn validate_local(c: &Config) -> Result<Vec<PathBuf>> {
    let p = &c.acp.client;
    if !(p.read_text_file || p.write_text_file || p.terminal) {
        return Ok(vec![]);
    }
    ensure!(
        cfg!(unix),
        "Local filesystem client tools currently require Unix"
    );
    if p.terminal {
        ensure!(
            cfg!(target_os = "linux") && Path::new("/usr/bin/bwrap").is_file(),
            "Client terminals require Linux and /usr/bin/bwrap; no unrestricted fallback is used"
        );
    }
    let mut roots = Vec::new();
    for root in &p.workspace_roots {
        let resolved =
            std::fs::canonicalize(root).context("Client workspace root does not exist locally")?;
        ensure!(
            resolved.is_dir() && resolved != Path::new("/"),
            "Client root must be a local workspace folder"
        );
        // Keep sensitive connector state outside all client mounts.
        roots.push(resolved);
    }
    let cwd = std::fs::canonicalize(&c.acp.cwd).context(
        "Client tools need a local agent working folder; remote paths are not translated",
    )?;
    ensure!(
        roots.iter().any(|p| cwd.starts_with(p)),
        "Agent working folder must be inside an explicit client root"
    );
    roots.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    Ok(roots)
}
impl ClientTools {
    pub fn new(c: &Config, home: &Path, session: &str) -> Result<Self> {
        let roots = validate_local(c)?;
        let adapter = std::fs::canonicalize(home)?;
        for root in &roots {
            ensure!(
                !adapter.starts_with(root),
                "Client roots must not expose the connector installation or credentials"
            );
        }
        Ok(Self {
            policy: c.acp.client.clone(),
            roots,
            cwd: PathBuf::from(&c.acp.cwd),
            home: home.to_owned(),
            session: session.to_owned(),
            terminals: Mutex::new(HashMap::new()),
        })
    }
    pub async fn call(&self, method: &str, p: &Value) -> Result<Value> {
        ensure!(
            p["sessionId"] == self.session,
            "Client operation belongs to a different session"
        );
        let result = match method {
            "fs/read_text_file" if self.policy.read_text_file => self.read(p),
            "fs/write_text_file" if self.policy.write_text_file => self.write(p),
            "terminal/create" if self.policy.terminal => self.create(p).await,
            "terminal/output" | "terminal/wait_for_exit" | "terminal/kill" | "terminal/release"
                if self.policy.terminal =>
            {
                self.terminal(method, p).await
            }
            _ => bail!("Client capability not offered"),
        };
        Store::open(&self.home)?.audit(
            "client_operation",
            json!({"method":method,"session_id":self.session,"ok":result.is_ok()}),
        )?;
        result
    }
    #[cfg(unix)]
    fn local(&self, path: &str) -> Result<(PathBuf, PathBuf)> {
        let path = PathBuf::from(path);
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)),
            "Use an absolute path without traversal"
        );
        let root = self
            .roots
            .iter()
            .find(|r| path.starts_with(r))
            .context("Path is outside the configured client roots")?;
        let relative = path.strip_prefix(root)?.to_owned();
        ensure!(!relative.as_os_str().is_empty(), "A file path is required");
        Ok((root.clone(), relative))
    }
    #[cfg(unix)]
    fn open(&self, path: &str, write: bool) -> Result<std::fs::File> {
        use std::{
            ffi::CString,
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            },
        };
        let (root, relative) = self.local(path)?;
        let mut directory = std::fs::File::open(root)?;
        let components: Vec<_> = relative.components().collect();
        for (i, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                bail!("Invalid path component")
            };
            let name = CString::new(name.as_bytes())?;
            let last = i + 1 == components.len();
            let flags = if last {
                libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK
                    | if write {
                        libc::O_WRONLY | libc::O_CREAT
                    } else {
                        libc::O_RDONLY
                    }
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
            };
            // SAFETY: the owned directory descriptor and NUL-terminated single
            // path component remain valid. No symlink is ever followed.
            let mut fd =
                unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0
                && write
                && !last
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            }
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            if last {
                ensure!(
                    file.metadata()?.is_file(),
                    "Client tools only read or write regular files"
                );
                return Ok(file);
            }
            directory = file;
        }
        bail!("File path required")
    }
    #[cfg(not(unix))]
    fn open(&self, _: &str, _: bool) -> Result<std::fs::File> {
        bail!("Local filesystem callbacks are not available on this platform")
    }
    fn read(&self, p: &Value) -> Result<Value> {
        use std::io::Read;
        let file = self.open(p["path"].as_str().context("Missing path")?, false)?;
        ensure!(
            file.metadata()?.len() <= self.policy.max_file_bytes as u64,
            "File exceeds configured byte limit"
        );
        let mut text = String::new();
        file.take(self.policy.max_file_bytes as u64 + 1)
            .read_to_string(&mut text)?;
        ensure!(
            text.len() <= self.policy.max_file_bytes,
            "File exceeds configured byte limit"
        );
        let line = optional_positive(p, "line")?.unwrap_or(1);
        let limit = optional_positive(p, "limit")?.unwrap_or(usize::MAX);
        let content = if p.get("line").is_some_and(|v| !v.is_null())
            || p.get("limit").is_some_and(|v| !v.is_null())
        {
            text.split_inclusive('\n')
                .skip(line - 1)
                .take(limit)
                .collect::<String>()
        } else {
            text
        };
        Ok(json!({"content":content}))
    }
    fn write(&self, p: &Value) -> Result<Value> {
        use std::io::Write;
        let content = p["content"].as_str().context("Missing text content")?;
        ensure!(
            content.len() <= self.policy.max_file_bytes,
            "Content exceeds configured byte limit"
        );
        let mut file = self.open(p["path"].as_str().context("Missing path")?, true)?;
        file.set_len(0)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        Ok(json!({}))
    }
    async fn create(&self, p: &Value) -> Result<Value> {
        ensure!(
            cfg!(target_os = "linux"),
            "Isolated client terminals currently require Linux"
        );
        ensure!(
            self.terminals.lock().unwrap().len() < 8,
            "Too many active client terminals"
        );
        let command = p["command"].as_str().context("Missing command")?;
        ensure!(
            !command.is_empty() && command.len() <= 4096 && !command.contains('\0'),
            "Invalid terminal command"
        );
        let args: Vec<String> = serde_json::from_value(
            p.get("args")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or(json!([])),
        )?;
        ensure!(
            args.len() <= 128 && args.iter().all(|v| v.len() <= 16384 && !v.contains('\0')),
            "Terminal arguments too large"
        );
        let cwd = std::fs::canonicalize(
            p["cwd"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or(self.cwd.clone()),
        )?;
        ensure!(
            cwd.is_dir() && self.roots.iter().any(|r| cwd.starts_with(r)),
            "Terminal working directory is outside the configured roots"
        );
        let limit = optional_positive(p, "outputByteLimit")?
            .unwrap_or(self.policy.terminal_output_bytes)
            .min(self.policy.terminal_output_bytes);
        let mut cmd = Command::new("/usr/bin/bwrap");
        process::prepare(&mut cmd);
        cmd.env_clear().args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/usr",
            "/usr",
        ]);
        for name in ["bin", "sbin", "lib", "lib64"] {
            let path = PathBuf::from(format!("/{name}"));
            if path.exists() {
                if path.is_symlink() {
                    cmd.args([
                        "--symlink",
                        &std::fs::read_link(&path)?.to_string_lossy(),
                        &format!("/{name}"),
                    ]);
                } else {
                    cmd.arg("--ro-bind").arg(&path).arg(&path);
                }
            }
        }
        cmd.args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "LANG",
            "C.UTF-8",
        ]);
        // Only operator-selected workspace folders are writable. Host /home,
        // /etc, provider credentials, sockets and network are not mounted.
        for root in self.roots.iter().rev() {
            cmd.arg(if self.policy.write_text_file {
                "--bind"
            } else {
                "--ro-bind"
            })
            .arg(root)
            .arg(root);
        }
        if let Some(env) = p.get("env").filter(|v| !v.is_null()) {
            let env = env
                .as_array()
                .context("Terminal environment must be an array")?;
            ensure!(env.len() <= 64, "Too many terminal environment entries");
            for item in env {
                let name = item["name"].as_str().context("Missing environment name")?;
                let value = item["value"]
                    .as_str()
                    .context("Missing environment value")?;
                ensure!(
                    !name.is_empty()
                        && name.len() <= 128
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && value.len() <= 16384
                        && !value.contains('\0'),
                    "Invalid terminal environment"
                );
                cmd.args(["--setenv", name, value]);
            }
        }
        cmd.arg("--chdir")
            .arg(cwd)
            .arg("--")
            .arg(command)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .context("Could not start isolated client terminal")?;
        let tree = Arc::new(Mutex::new(Some(process::ProcessTree::attach(&child)?)));
        let state = Arc::new(Mutex::new(TerminalState::default()));
        let notify = Arc::new(Notify::new());
        let stdout = child.stdout.take().context("Missing terminal output")?;
        let stderr = child.stderr.take().context("Missing terminal errors")?;
        let out = state.clone();
        let changed = notify.clone();
        let cleanup = tree.clone();
        let timeout = self.policy.terminal_timeout_seconds;
        let task = tokio::spawn(async move {
            let a = capture(stdout, out.clone(), limit);
            let b = capture(stderr, out.clone(), limit);
            let wait = async {
                let status =
                    match tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await {
                        Ok(status) => status,
                        Err(_) => {
                            cleanup.lock().unwrap().take();
                            child.wait().await
                        }
                    };
                // Reap every descendant even when the primary command exits.
                cleanup.lock().unwrap().take();
                status
            };
            let (status, _, _) = tokio::join!(wait, a, b);
            let value = match status {
                Ok(status) => {
                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal().map(|s| s.to_string())
                    };
                    #[cfg(not(unix))]
                    let signal: Option<String> = None;
                    json!({"exitCode":status.code(),"signal":signal})
                }
                Err(_) => json!({"exitCode":null,"signal":"error"}),
            };
            out.lock().unwrap().exit = Some(value);
            changed.notify_waiters();
        });
        let id = uuid::Uuid::new_v4().to_string();
        self.terminals.lock().unwrap().insert(
            id.clone(),
            Arc::new(Terminal {
                state,
                notify,
                tree,
                task,
            }),
        );
        Ok(json!({"terminalId":id}))
    }
    async fn terminal(&self, method: &str, p: &Value) -> Result<Value> {
        let id = p["terminalId"].as_str().context("Missing terminal id")?;
        let terminal = self
            .terminals
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("Unknown terminal for this session")?;
        match method {
            "terminal/output" => {
                let state = terminal.state.lock().unwrap();
                let output = String::from_utf8_lossy(&state.output);
                let mut start = output.len().saturating_sub(state.limit);
                while !output.is_char_boundary(start) {
                    start += 1;
                }
                let mut result =
                    json!({"output":&output[start..],"truncated":state.truncated || start>0});
                if let Some(exit) = &state.exit {
                    result["exitStatus"] = exit.clone();
                }
                Ok(result)
            }
            "terminal/kill" => {
                terminal.tree.lock().unwrap().take();
                Ok(json!({}))
            }
            "terminal/release" => {
                terminal.tree.lock().unwrap().take();
                self.terminals.lock().unwrap().remove(id);
                Ok(json!({}))
            }
            "terminal/wait_for_exit" => loop {
                let notified = terminal.notify.notified();
                if let Some(exit) = terminal.state.lock().unwrap().exit.clone() {
                    break Ok(exit);
                }
                notified.await;
            },
            _ => bail!("Unknown terminal operation"),
        }
    }
}
fn optional_positive(p: &Value, key: &str) -> Result<Option<usize>> {
    let Some(v) = p.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let n = v
        .as_u64()
        .filter(|n| *n > 0 && *n <= usize::MAX as u64)
        .context("Expected a positive integer")?;
    Ok(Some(n as usize))
}
async fn capture<R: AsyncRead + Unpin>(
    mut stream: R,
    state: Arc<Mutex<TerminalState>>,
    limit: usize,
) {
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => state.lock().unwrap().append(&buffer[..n], limit),
        }
    }
}
