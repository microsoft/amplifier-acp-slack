use acp_slack_adapter::{
    config::{self, Config},
    driver, slack,
    store::Store,
};
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "A standalone Slack connection to any compatible ACP agent"
)]
struct Cli {
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Init {
        #[arg(long, default_value = "Slack connection")]
        label: String,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long)]
        agent_home: Option<PathBuf>,
    },
    Serve,
    /// Inspect, prepare or queue a verified update of this connector only.
    Update {
        #[arg(default_value = "status")]
        action: String,
        #[arg(long)]
        plan_id: Option<String>,
    },
    /// Fixed installation-owned connection protocol; bounded JSON from stdin.
    Connection {
        action: String,
    },
    /// Emit host enrollment for this existing saved Slack installation.
    ConnectionEnrollment,
    Status,
    Start,
    Stop,
    Check {
        #[arg(long, default_value = "all")]
        target: String,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    Manifest {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Calls the same authenticated management API used by the web interface.
    Api {
        method: String,
        path: String,
        #[arg(long)]
        body_file: Option<PathBuf>,
    },
    /// Read-only local ACP test: negotiates and creates a session, sends no prompt.
    Probe,
    /// Store an ACP environment credential by reference; never accepts it in argv.
    Secret {
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
}
#[derive(Subcommand)]
enum ConfigAction {
    Get,
    Set {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        offline: bool,
    },
    /// Relocate the same agent's workspace after an operator-verified host move.
    /// Requires the service to be stopped; does not change its Slack identity or ACP command.
    RelocateWorkspace {
        #[arg(long)]
        cwd: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long, required = true)]
        same_agent: bool,
        #[arg(long)]
        reason: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = cli
        .home
        .context("Supply --home with this adapter installation folder")?;
    match cli.command {
        Action::Init {
            label,
            owner,
            bind,
            port,
            agent_home,
        } => {
            ensure!(
                !home.join("adapter.json").exists(),
                "This installation already exists; nothing was overwritten"
            );
            config::private_dir(&home)?;
            for dir in ["bin", "secrets", "client"] {
                config::private_dir(&home.join(dir))?;
            }
            let mut c = Config::new(
                label,
                owner
                    .or_else(|| std::env::var("USER").ok())
                    .context("Supply the installation owner")?,
            );
            c.server.bind = bind;
            c.server.port = port;
            // Optional convenience import. The resulting command is ordinary
            // ACP stdio; no Agent Instance API is required during operation.
            if let Some(agent) = agent_home {
                let agent = std::fs::canonicalize(agent)?;
                let raw: Value =
                    serde_json::from_slice(&std::fs::read(agent.join("config.json"))?)?;
                let raw = raw.get("config").unwrap_or(&raw);
                let binary = agent.join("bin/agent-instance");
                ensure!(binary.is_file(), "Agent executable was not found");
                c.acp.command = binary.to_string_lossy().into_owned();
                c.acp.args = vec![
                    "acp".into(),
                    "--home".into(),
                    agent.to_string_lossy().into_owned(),
                ];
                c.acp.cwd = raw["backend"]["working_directory"]
                    .as_str()
                    .unwrap_or("${AGENT_HOME}/workspace")
                    .replace("${AGENT_HOME}", &agent.to_string_lossy());
            }
            c.save(&home)?;
            let name = if cfg!(windows) {
                "acp-slack-adapter.exe"
            } else {
                "acp-slack-adapter"
            };
            std::fs::copy(std::env::current_exe()?, home.join("bin").join(name))?;
            config::private_write(
                &home.join("release.json"),
                &serde_json::to_vec_pretty(
                    &json!({"version":env!("CARGO_PKG_VERSION"),"installed_at":acp_slack_adapter::model::now_ms(),"os":std::env::consts::OS,"arch":std::env::consts::ARCH}),
                )?,
            )?;
            print(
                json!({"id":c.id,"home":home,"configured_acp":!c.acp.command.is_empty(),"next":"Start the service, then open its setup URL"}),
            )?;
        }
        Action::Update { action, plan_id } => {
            if action == "status" {
                print(api(&home, "GET", "updates", None).await?)?;
            } else {
                print(
                    api(
                        &home,
                        "POST",
                        "updates",
                        Some(json!({"action":action,"plan_id":plan_id})),
                    )
                    .await?,
                )?;
            }
        }
        Action::Serve => acp_slack_adapter::server::serve(home).await?,
        Action::Connection { action } => {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::io::stdin().take(65537).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 65536, "Connection request too large");
            let request: acp_slack_adapter::connection::Command = serde_json::from_slice(&bytes)?;
            ensure!(request.action == action, "Fixed command action mismatch");
            print(
                api(
                    &home,
                    "POST",
                    "saved-connection",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
            )?;
        }
        Action::ConnectionEnrollment => {
            let c = Config::load(&home)?;
            let binary = std::env::current_exe()?;
            let commands = ["status", "connect", "disconnect"]
                .into_iter()
                .map(|action| {
                    (
                        action.to_owned(),
                        json!([binary, "--home", home, "connection", action]),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            print(
                json!({"id":acp_slack_adapter::connection::id(&c),"service":"slack","label":c.label,"revision":acp_slack_adapter::connection::revision(&c),"context_provider_ids":[format!("slack:{}",c.id)],"settings_url":c.server.public_url,"commands":commands}),
            )?;
        }
        Action::Status => print(api(&home, "GET", "status", None).await?)?,
        Action::Start => {
            print(api(&home, "POST", "lifecycle", Some(json!({"action":"start"}))).await?)?
        }
        Action::Stop => {
            print(api(&home, "POST", "lifecycle", Some(json!({"action":"stop"}))).await?)?
        }
        Action::Check { target } => {
            print(api(&home, "POST", "check", Some(json!({"target":target}))).await?)?
        }
        Action::Config {
            action: ConfigAction::Get,
        } => print(serde_json::to_value(Config::load(&home)?)?)?,
        Action::Config {
            action:
                ConfigAction::RelocateWorkspace {
                    cwd,
                    expected_revision,
                    same_agent,
                    reason,
                },
        } => {
            ensure!(
                same_agent,
                "Confirm this is the same individual after verifying the move"
            );
            print(acp_slack_adapter::relocation::workspace(
                &home,
                &cwd,
                expected_revision,
                &reason,
            )?)?;
        }
        Action::Config {
            action: ConfigAction::Set { file, offline },
        } => {
            let mut c: Config = serde_json::from_slice(&std::fs::read(file)?)?;
            let old = Config::load(&home)?;
            if offline {
                use fs2::FileExt;
                let lock = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(home.join("service.lock"))?;
                lock.try_lock_exclusive()
                    .context("Stop the service before an offline configuration change")?;
                ensure!(
                    c.id == old.id && c.server.owner == old.server.owner,
                    "Adapter identity and owner cannot change"
                );
                Store::open(&home)?.assert_endpoint_change_allowed(&old, &c)?;
                c.revision = old.revision + 1;
                if c.enabled {
                    c.validate_ready()?;
                    c.secrets(&home)?;
                }
                c.save(&home)?;
                print(serde_json::to_value(c)?)?;
            } else {
                print(
                    api(
                        &home,
                        "PUT",
                        "config",
                        Some(json!({"expected_revision":old.revision,"config":c})),
                    )
                    .await?,
                )?;
            }
        }
        Action::Manifest { output } => {
            let value = slack::manifest(&Config::load(&home)?.label);
            if let Some(path) = output {
                std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
            } else {
                print(value)?;
            }
        }
        Action::Api {
            method,
            path,
            body_file,
        } => {
            let body = body_file
                .map(std::fs::read)
                .transpose()?
                .map(|b| serde_json::from_slice(&b))
                .transpose()?;
            print(api(&home, &method, &path, body).await?)?;
        }
        Action::Probe => print(driver::check(&Config::load(&home)?, &home).await?)?,
        Action::Secret { name, file } => {
            ensure!(
                config::valid_secret_name(&name)
                    && !["control-token", "slack"].contains(&name.as_str()),
                "Choose a valid non-reserved secret name"
            );
            let bytes = std::fs::read(file)?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= 65536,
                "Secret must contain 1–65536 bytes"
            );
            let value = std::str::from_utf8(&bytes)?.trim_end();
            print(
                api(
                    &home,
                    "PUT",
                    &format!("secrets/{name}"),
                    Some(json!({"value":value})),
                )
                .await?,
            )?;
        }
    }
    Ok(())
}
fn print(value: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
async fn api(home: &Path, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    ensure!(
        !path.contains("..") && !path.contains("://") && !path.starts_with('/'),
        "Use a relative API path"
    );
    let endpoints: Value = serde_json::from_slice(
        &std::fs::read(home.join("endpoints.json")).context("Start the adapter service first")?,
    )?;
    let base = endpoints["base_url"]
        .as_str()
        .context("No local service URL")?;
    let certificate = endpoints["trust_certificate_file"]
        .as_str()
        .or_else(|| endpoints["certificate_file"].as_str())
        .context("No local service certificate")?;
    let token_path = home.join("secrets/control-token");
    config::check_private(&token_path)?;
    let token = std::fs::read_to_string(token_path)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(90))
        .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(
            certificate,
        )?)?)
        .build()?;
    let mut r = client
        .request(
            method.parse()?,
            format!("{}/api/v1/{path}", base.trim_end_matches('/')),
        )
        .bearer_auth(token.trim());
    if let Some(value) = body {
        r = r.json(&value);
    }
    let response = r
        .send()
        .await
        .context("Could not reach the adapter service")?;
    let status = response.status();
    let value: Value = response.json().await?;
    ensure!(
        status.is_success(),
        "{}",
        value["error"]
            .as_str()
            .unwrap_or("Management operation failed")
    );
    Ok(value)
}
