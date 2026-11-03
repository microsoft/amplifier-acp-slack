use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub id: String,
    pub label: String,
    pub revision: u64,
    pub enabled: bool,
    pub setup_complete: bool,
    pub acp: AcpConfig,
    pub slack: SlackConfig,
    #[serde(default = "mentions")]
    pub participation: String,
    #[serde(default)]
    pub attention: Attention,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub media: MediaConfig,
    pub server: ServerConfig,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    pub incoming: bool,
    pub outgoing: bool,
    pub max_files: usize,
    pub max_file_bytes: usize,
}
impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            incoming: true,
            outgoing: true,
            max_files: 4,
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub enabled: bool,
    pub owner_slack_user_id: String,
    /// Limit non-owner grants originating in a thread to that thread.
    pub thread_only_grants: bool,
}
fn mentions() -> String {
    "mentions".into()
}
#[derive(Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AcpConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub secret_env: BTreeMap<String, String>,
    pub cwd: String,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    pub client: ClientCapabilities,
}
/// Capabilities hosted by this connector, independent of the agent implementation.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
    pub terminal: bool,
    pub workspace_roots: Vec<String>,
    /// Require a human decision for ACP tool permission requests.
    pub approvals_required: bool,
    /// Deprecated compatibility field; the initiating human can approve their own work.
    pub approval_users: Vec<String>,
    pub permission_timeout_seconds: u64,
    pub max_file_bytes: usize,
    pub terminal_output_bytes: usize,
    pub terminal_timeout_seconds: u64,
}
impl Default for ClientCapabilities {
    fn default() -> Self {
        Self {
            read_text_file: false,
            write_text_file: false,
            terminal: false,
            workspace_roots: vec![],
            approvals_required: true,
            approval_users: vec![],
            permission_timeout_seconds: 900,
            max_file_bytes: 1024 * 1024,
            terminal_output_bytes: 256 * 1024,
            terminal_timeout_seconds: 120,
        }
    }
}
fn timeout() -> u64 {
    300
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SlackConfig {
    pub team_id: String,
    pub app_id: String,
    pub bot_user_id: String,
    pub bot_id: String,
    pub channels: Vec<String>,
    pub allow_dms: bool,
    pub api_base: String,
    pub loopback_test: bool,
}
impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            team_id: String::new(),
            app_id: String::new(),
            bot_user_id: String::new(),
            bot_id: String::new(),
            channels: vec![],
            allow_dms: true,
            api_base: "https://slack.com/api/".into(),
            loopback_test: false,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Attention {
    pub parallel_contexts: usize,
    pub settle_ms: u64,
    pub minimum_reply_interval_seconds: u64,
    pub max_replies_per_context_per_hour: usize,
    pub max_replies_per_agent_per_hour: usize,
    pub max_decisions_per_agent_per_hour: usize,
    pub max_bot_replies_without_human: usize,
}
impl Default for Attention {
    fn default() -> Self {
        Self {
            parallel_contexts: 2,
            settle_ms: 1500,
            minimum_reply_interval_seconds: 3,
            max_replies_per_context_per_hour: 30,
            max_replies_per_agent_per_hour: 120,
            max_decisions_per_agent_per_hour: 240,
            max_bot_replies_without_human: 3,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    pub owner: String,
    pub pam_service: String,
    pub public_url: Option<String>,
    #[serde(default)]
    pub embed_origins: Vec<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    pub app_token: String,
    pub bot_token: String,
}

impl Config {
    pub fn new(label: String, owner: String) -> Self {
        Self {
            schema: 1,
            id: uuid::Uuid::new_v4().to_string(),
            label,
            revision: 1,
            enabled: false,
            setup_complete: false,
            acp: AcpConfig {
                timeout_seconds: 300,
                ..Default::default()
            },
            slack: SlackConfig::default(),
            participation: mentions(),
            attention: Attention::default(),
            context: ContextConfig::default(),
            media: MediaConfig::default(),
            server: ServerConfig {
                bind: "127.0.0.1".into(),
                port: 0,
                owner,
                pam_service: "acp-slack-adapter".into(),
                public_url: None,
                embed_origins: vec![],
            },
        }
    }
    pub fn load(home: &Path) -> Result<Self> {
        let c: Self = serde_json::from_slice(&std::fs::read(home.join("adapter.json"))?)?;
        c.validate()?;
        Ok(c)
    }
    pub fn save(&self, home: &Path) -> Result<()> {
        self.validate()?;
        private_write(
            &home.join("adapter.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "Unsupported configuration schema");
        ensure!(
            (1..=4).contains(&self.media.max_files)
                && (1..=2 * 1024 * 1024).contains(&self.media.max_file_bytes),
            "Media limits: 1–4 files, up to 2 MiB each"
        );
        ensure!(
            uuid::Uuid::parse_str(&self.id).is_ok(),
            "Invalid adapter id"
        );
        ensure!(
            !self.label.trim().is_empty() && self.label.len() <= 200,
            "Choose a connection name (1–200 characters)"
        );
        ensure!(
            ["mentions", "ambient"].contains(&self.participation.as_str()),
            "Participation must be mentions or ambient"
        );
        ensure!(
            self.context.owner_slack_user_id.is_empty()
                || (self.context.owner_slack_user_id.starts_with('U')
                    && self.context.owner_slack_user_id.len() <= 80
                    && self
                        .context
                        .owner_slack_user_id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric())),
            "Owner identity must be a Slack user ID"
        );
        if self.context.enabled {
            ensure!(
                self.server.public_url.is_some(),
                "Conversation context requires the connector's reachable HTTPS address"
            );
        }
        ensure!(
            (1..=8).contains(&self.attention.parallel_contexts),
            "Parallel conversations must be 1–8"
        );
        ensure!(
            self.attention.settle_ms <= 30000
                && self.attention.minimum_reply_interval_seconds <= 3600,
            "Invalid attention timing"
        );
        ensure!(
            (1..=10000).contains(&self.attention.max_decisions_per_agent_per_hour)
                && (1..=10000).contains(&self.attention.max_replies_per_agent_per_hour)
                && (1..=10000).contains(&self.attention.max_replies_per_context_per_hour),
            "Invalid attention budget"
        );
        ensure!(
            (1..=20).contains(&self.attention.max_bot_replies_without_human),
            "Bot exchange limit must be 1–20"
        );
        ensure!(
            (1..=3600).contains(&self.acp.timeout_seconds),
            "ACP timeout must be 1–3600 seconds"
        );
        let client = &self.acp.client;
        ensure!(
            client.workspace_roots.len() <= 16
                && client
                    .workspace_roots
                    .iter()
                    .all(|p| Path::new(p).is_absolute()
                        && p.len() <= 4096
                        && p != "/"
                        && !p.contains('\0')),
            "Client workspace roots must be explicit absolute folders, never the filesystem root"
        );
        ensure!(
            !(client.read_text_file || client.write_text_file || client.terminal)
                || !client.workspace_roots.is_empty(),
            "Choose local workspace roots before enabling client tools"
        );
        ensure!(
            client.approval_users.len() <= 100
                && client.approval_users.iter().all(|u| u.starts_with('U')
                    && u.len() <= 80
                    && u.chars().all(|c| c.is_ascii_alphanumeric())),
            "Permission approvers must be Slack member IDs"
        );
        ensure!(
            (1..=86400).contains(&client.permission_timeout_seconds)
                && (1..=3600).contains(&client.terminal_timeout_seconds)
                && (1..=4 * 1024 * 1024).contains(&client.max_file_bytes)
                && (1..=1024 * 1024).contains(&client.terminal_output_bytes),
            "Invalid client tool limits"
        );
        ensure!(
            self.acp.command.len() <= 4096
                && self.acp.args.len() <= 128
                && self.acp.args.iter().all(|s| s.len() <= 16384),
            "ACP command too large"
        );
        ensure!(
            self.acp.env.len() <= 128 && self.acp.secret_env.len() <= 128,
            "Too many environment settings"
        );
        for (key, value) in self.acp.env.iter().chain(self.acp.secret_env.iter()) {
            ensure!(
                !key.is_empty()
                    && key.len() <= 200
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !value.contains('\0'),
                "Invalid environment entry"
            );
        }
        for (key, value) in &self.acp.env {
            ensure!(
                !sensitive_name(key) && !looks_secret(value),
                "Use a secret reference for credentials; do not put them in environment settings"
            );
        }
        for arg in &self.acp.args {
            ensure!(
                !looks_secret(arg),
                "Credentials must not be embedded in command arguments"
            );
        }
        for name in self.acp.secret_env.values() {
            ensure!(valid_secret_name(name), "Invalid secret reference");
        }
        ensure!(
            self.slack.channels.len() <= 500
                && self
                    .slack
                    .channels
                    .iter()
                    .all(|s| !s.is_empty() && s.len() < 100),
            "Invalid channel list"
        );
        if self.slack.loopback_test {
            let u = reqwest::Url::parse(&self.slack.api_base)?;
            ensure!(
                u.scheme() == "http"
                    && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
                    && u.username().is_empty()
                    && u.password().is_none(),
                "Test transport must stay on loopback"
            );
        } else {
            ensure!(
                self.slack.api_base == "https://slack.com/api/",
                "Production transport must use Slack's API"
            );
        }
        self.server
            .bind
            .parse::<std::net::IpAddr>()
            .context("Bind must be an IP address")?;
        ensure!(
            !self.server.owner.is_empty() && self.server.owner.len() < 128,
            "An installation owner is required"
        );
        ensure!(
            valid_secret_name(&self.server.pam_service),
            "Invalid PAM service name"
        );
        for origin in self
            .server
            .embed_origins
            .iter()
            .chain(self.server.public_url.iter())
        {
            let u = reqwest::Url::parse(origin)?;
            ensure!(
                u.scheme() == "https"
                    && u.host_str().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none()
                    && u.path() == "/",
                "Use an HTTPS origin, without credentials or a path"
            );
        }
        Ok(())
    }
    pub fn validate_ready(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            !self.acp.command.trim().is_empty(),
            "Connect an ACP agent first"
        );
        ensure!(
            Path::new(&self.acp.cwd).is_absolute(),
            "The agent working directory must be absolute (on the agent host)"
        );
        ensure!(
            !self.slack.team_id.is_empty()
                && !self.slack.app_id.is_empty()
                && !self.slack.bot_user_id.is_empty()
                && !self.slack.bot_id.is_empty(),
            "Connect a Slack app first"
        );
        Ok(())
    }
    pub fn secrets(&self, home: &Path) -> Result<Secrets> {
        let path = home.join("secrets/slack.json");
        check_private(&path)?;
        let s: Secrets = serde_json::from_slice(&std::fs::read(path)?)?;
        if self.slack.loopback_test {
            ensure!(
                s.app_token.starts_with("test-") && s.bot_token.starts_with("test-"),
                "Real credentials cannot be used with the test transport"
            );
        } else {
            ensure!(
                s.app_token.starts_with("xapp-") && s.bot_token.starts_with("xoxb-"),
                "Supply an app token and a bot token"
            );
        }
        Ok(s)
    }
    pub fn allows(&self, channel: &str, channel_type: &str) -> bool {
        let dm = channel_type == "im" || channel.starts_with('D');
        (!dm || self.slack.allow_dms)
            && (self.slack.channels.is_empty() || self.slack.channels.iter().any(|s| s == channel))
    }
    pub fn should_wake(&self, channel: &str, text: &str) -> bool {
        self.participation == "ambient"
            || (channel.starts_with('D') && self.slack.allow_dms)
            || text.contains(&format!("<@{}>", self.slack.bot_user_id))
    }
}
pub fn valid_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
pub fn sensitive_name(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    [
        "token",
        "password",
        "secret",
        "api_key",
        "apikey",
        "authorization",
    ]
    .iter()
    .any(|k| s.contains(k))
}
pub fn looks_secret(s: &str) -> bool {
    ["xoxb-", "xapp-", "xoxp-", "sk-proj-", "sk-ant-"]
        .iter()
        .any(|p| s.contains(p))
}
pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing parent folder")?;
    private_dir(parent)?;
    let temp = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&temp)?;
    use std::io::Write;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&temp, path)?;
    Ok(())
}
pub fn check_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "Credential file permissions must be private (600)"
        );
    }
    Ok(())
}
pub fn local_path(home: &Path, s: &str) -> PathBuf {
    PathBuf::from(s.replace("${ADAPTER_HOME}", &home.to_string_lossy()))
}
