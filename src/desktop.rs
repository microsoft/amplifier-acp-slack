//! Local first launch for this connector only. No Slack login, engine choice,
//! session or work is created by opening the app.
use crate::{
    auth,
    config::{self, AuthMode, Config, WebTransport},
};
use amplifier_native_platform::{self as platform, Application};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const APPLICATION: Application = Application {
    display_name: "Slack Connector",
    startup_description: "Constellation Slack Connector",
    executable_name: "acp-slack-adapter",
    startup_prefix: "acp-slack-adapter",
    data_directory: "acp-slack-adapter",
    background_command: "serve",
};
pub fn default_home() -> Result<PathBuf> {
    platform::default_home(&APPLICATION, "default")
}

pub fn create(home: &Path) -> Result<()> {
    ensure!(
        !home.exists(),
        "This folder already exists; choose a new installation folder"
    );
    let parent = home.parent().context("Missing installation parent")?;
    std::fs::create_dir_all(parent)?;
    let stage = parent.join(format!(".slack-setup-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        platform::private_dir(&stage)?;
        for dir in ["bin", "secrets", "client"] {
            platform::private_dir(&stage.join(dir))?;
        }
        let owner = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "owner".into());
        let mut config = Config::new("Slack connection".into(), owner);
        config.server.auth_mode = AuthMode::LocalPassword;
        config.server.web_transport = WebTransport::LocalHttp;
        config.save(&stage)?;
        platform::private_create(
            &stage.join("native-installation.json"),
            &serde_json::to_vec(&json!({"schema":1,"id":config.id,"update_owner":"installation"}))?,
        )?;
        platform::copy_self(&APPLICATION, &stage)?;
        install_support(&stage)?;
        platform::private_create(
            &stage.join("secrets/first-account"),
            &serde_json::to_vec(&json!({
                "token":auth::random_token(), "expires_at":crate::model::now_ms()+24*3600*1000_i64
            }))?,
        )?;
        config::private_write(
            &stage.join("release.json"),
            &serde_json::to_vec_pretty(&json!({
                "version":env!("CARGO_PKG_VERSION"), "installed_at":crate::model::now_ms(),
                "os":std::env::consts::OS, "arch":std::env::consts::ARCH
            }))?,
        )?;
        drop(crate::state::AppState::open(stage.clone())?);
        platform::publish_directory_if_absent(&stage, home)
    })();
    if result.is_err() && stage.exists() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}

pub async fn launch(home: PathBuf, open_browser: bool) -> Result<()> {
    if !home.join("adapter.json").exists() {
        create(&home)?;
    }
    let config = Config::load(&home)?;
    let lock_path = home.join("service.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    platform::protect_file(&lock_path)?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {
            drop(lock);
            // Only an inactive, still-unclaimed installation gets a fresh local setup link.
            if config.server.auth_mode == AuthMode::LocalPassword
                && !home.join("secrets/local-password").exists()
            {
                platform::private_write(
                    &home.join("secrets/first-account"),
                    &serde_json::to_vec(&json!({
                        "token":auth::random_token(), "expires_at":crate::model::now_ms()+24*3600*1000_i64
                    }))?,
                )?;
            }
            crate::server::serve_with_browser(home, open_browser).await
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let response = local_api(&home, "GET", "status", None).await.context("The running connector could not be verified; no credential was sent to an unverified local HTTP listener")?;
            ensure!(
                response["id"] == config.id,
                "A different connector answered at this address"
            );
            if config.server.auth_mode == AuthMode::LocalPassword
                && !home.join("secrets/local-password").exists()
            {
                let expired = std::fs::read(home.join("secrets/first-account"))
                    .ok()
                    .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
                    .and_then(|v| v["expires_at"].as_i64())
                    .is_none_or(|at| at <= crate::model::now_ms());
                if expired {
                    platform::private_write(
                        &home.join("secrets/first-account"),
                        &serde_json::to_vec(
                            &json!({"token":auth::random_token(),"expires_at":crate::model::now_ms()+24*3600*1000_i64}),
                        )?,
                    )?;
                }
            }

            ready(&home, &config, open_browser)
        }
        Err(error) => Err(error.into()),
    }
}
pub fn ready(home: &Path, config: &Config, open_browser: bool) -> Result<()> {
    let endpoint: Value = serde_json::from_slice(&std::fs::read(home.join("endpoints.json"))?)?;
    let base = config
        .server
        .public_url
        .as_deref()
        .or_else(|| endpoint["base_url"].as_str())
        .context("No connector address")?;
    let mut url = base.trim_end_matches('/').to_owned();
    if config.server.web_transport == WebTransport::LocalHttp
        && !home.join("secrets/local-password").exists()
    {
        let setup: Value =
            serde_json::from_slice(&std::fs::read(home.join("secrets/first-account"))?)?;
        ensure!(
            setup["expires_at"].as_i64().unwrap_or(0) > crate::model::now_ms(),
            "The setup link expired. Close this connector and open the app again."
        );
        let token = setup["token"].as_str().context("Invalid setup receipt")?;
        ensure!(
            token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid setup receipt"
        );
        url.push_str(&format!("/#setup={token}"));
    }
    if open_browser {
        if url.starts_with("https:") {
            platform::open_https_browser(&url)?;
        } else {
            platform::open_browser(&url)?;
        }
    } else {
        println!(
            "{}",
            json!({"ready":true,"home":home,"url":url,"id":config.id})
        );
    }
    Ok(())
}
pub fn set_startup(home: &Path, enabled: bool) -> Result<()> {
    let config = Config::load(home)?;
    ensure!(
        config.server.auth_mode == AuthMode::LocalPassword,
        "Startup for this existing installation remains owner-managed"
    );
    let id = uuid::Uuid::parse_str(&config.id)?;
    let result = if enabled {
        platform::install_startup(&APPLICATION, home, &id)
    } else {
        platform::remove_startup(&APPLICATION, home, &id)
    };
    let receipt = json!({"requested":enabled,"configured":result.is_ok().then_some(enabled),"error":result.as_ref().err().map(ToString::to_string)});
    platform::private_write(
        &home.join("startup.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    result
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Startup {
    enabled: bool,
}
pub async fn startup_get(
    State(state): State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
) -> crate::error::ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", false)?;
    let status = std::fs::read(state.home.join("startup.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(json!({"configured":false}));
    Ok(Json(status))
}
pub async fn startup_set(
    State(state): State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
    Json(input): Json<Startup>,
) -> crate::error::ApiResult<Json<Value>> {
    auth::check(&state, &headers, "admin", true)?;
    set_startup(&state.home, input.enabled)?;
    Ok(Json(json!({"configured":input.enabled})))
}

pub async fn local_api(
    home: &Path,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    ensure!(
        !path.contains("..") && !path.contains("://") && !path.starts_with('/'),
        "Use a relative API path"
    );
    let config = Config::load(home)?;
    let token_path = home.join("secrets/control-token");
    config::check_private(&token_path)?;
    let token = std::fs::read_to_string(token_path)?;
    if config.server.web_transport == WebTransport::LocalHttp {
        return tokio::time::timeout(
            Duration::from_secs(90),
            proved_local_api(&config, token.trim(), method, path, body),
        )
        .await
        .context("Local connector did not answer")?;
    }
    let endpoints: Value = serde_json::from_slice(&std::fs::read(home.join("endpoints.json"))?)?;
    let base = endpoints["base_url"]
        .as_str()
        .context("Start the adapter service first")?;
    ensure!(
        reqwest::Url::parse(base)?.scheme() == "https",
        "Existing installation requires HTTPS"
    );
    let certificate = endpoints["trust_certificate_file"]
        .as_str()
        .or_else(|| endpoints["certificate_file"].as_str())
        .context("Missing local certificate")?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(90))
        .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(
            certificate,
        )?)?)
        .build()?;
    let mut request = client
        .request(
            method.parse()?,
            format!("{}/api/v1/{path}", base.trim_end_matches('/')),
        )
        .bearer_auth(token.trim());
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
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
async fn proved_local_api(
    config: &Config,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    use axum::{body::Bytes, http::header};
    use http_body_util::{BodyExt, Full, Limited};
    use hyper_util::rt::TokioIo;
    struct Connection(tokio::task::JoinHandle<()>);
    impl Drop for Connection {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", config.server.port)).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream)).await?;
    let _connection = Connection(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let host = config
        .server
        .public_url
        .as_deref()
        .and_then(|u| u.strip_prefix("http://"))
        .context("Invalid local origin")?;
    let nonce = auth::random_token();
    let request = hyper::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/local-proof?nonce={nonce}"))
        .header(header::HOST, host)
        .body(Full::new(Bytes::new()))?;
    let response = sender.send_request(request).await?;
    ensure!(
        response.status().is_success(),
        "Local installation proof was refused"
    );
    let bytes = Limited::new(response.into_body(), 4096)
        .collect()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid local installation proof"))?
        .to_bytes();
    let proof: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        auth::verify_local_proof(config, &nonce, token, &proof),
        "A different installation answered at the saved address"
    );
    let mut request = hyper::Request::builder()
        .method(method)
        .uri(format!("/api/v1/{path}"))
        .header(header::HOST, host)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let bytes = if let Some(body) = body {
        request = request.header(header::CONTENT_TYPE, "application/json");
        serde_json::to_vec(&body)?
    } else {
        vec![]
    };
    let response = sender
        .send_request(request.body(Full::new(Bytes::from(bytes)))?)
        .await?;
    let status = response.status();
    let bytes = Limited::new(response.into_body(), 16 * 1024 * 1024)
        .collect()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid local API response"))?
        .to_bytes();
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        status.is_success(),
        "{}",
        value["error"]
            .as_str()
            .unwrap_or("Management operation failed")
    );
    Ok(value)
}

/// Bundled setup dependencies belong to this installation, never the system PATH.
pub fn python_path(home: &Path) -> PathBuf {
    home.join("runtime/python").join(if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    })
}
fn install_support(home: &Path) -> Result<()> {
    let executable = std::env::current_exe()?;
    let parent = executable.parent().context("No application folder")?;
    let bundle = if cfg!(target_os = "macos") && parent.ends_with("MacOS") {
        parent
            .parent()
            .context("No application resources")?
            .join("Resources")
    } else {
        parent.to_owned()
    };
    let bytes = std::fs::read(bundle.join("native-bundle.json"))
        .context("Open the complete Slack Connector download, with its private runtime folder")?;
    let manifest: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest["schema"] == 1 && manifest["component"] == "acp-slack-adapter",
        "Invalid native bundle manifest"
    );
    copy_runtime_contents(
        &bundle.join("runtime"),
        &home.join("runtime"),
        &manifest,
        &bytes,
    )
}
pub fn copy_runtime(source_resources: &Path, target_resources: &Path) -> Result<()> {
    let bytes = std::fs::read(source_resources.join("native-bundle.json"))?;
    let manifest: Value = serde_json::from_slice(&bytes)?;
    copy_runtime_contents(source_resources, target_resources, &manifest, &bytes)
}
fn copy_runtime_contents(
    source_resources: &Path,
    target_resources: &Path,
    manifest: &Value,
    bytes: &[u8],
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let files = manifest["python_files"]
        .as_object()
        .context("Missing private runtime inventory")?;
    ensure!(
        !files.is_empty() && files.len() < 20000,
        "Invalid private runtime inventory"
    );
    let target = target_resources.join("python");
    platform::private_dir(&target)?;
    for (name, expected) in files {
        let path = Path::new(name);
        ensure!(
            path.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
                && !name.contains('\\')
                && !name.contains(':'),
            "Invalid runtime member"
        );
        let source = source_resources.join("python").join(path);
        let metadata = std::fs::symlink_metadata(&source)?;
        ensure!(
            metadata.is_file() && metadata.len() <= 256 * 1024 * 1024,
            "Invalid private runtime member"
        );
        let bytes = std::fs::read(&source)?;
        ensure!(
            expected.as_str() == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
            "Private runtime file failed verification"
        );
        let destination = target.join(path);
        std::fs::create_dir_all(destination.parent().context("No runtime parent")?)?;
        std::fs::write(&destination, bytes)?;
        std::fs::set_permissions(&destination, metadata.permissions())?;
    }
    ensure!(
        target
            .join(if cfg!(windows) {
                "python.exe"
            } else {
                "bin/python3"
            })
            .is_file(),
        "Private Python executable missing"
    );
    platform::private_write(&target_resources.join("native-bundle.json"), bytes)?;
    Ok(())
}
