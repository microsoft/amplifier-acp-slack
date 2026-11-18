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

/// Finite native installation for a local owner such as Observatory. This uses
/// the same create-only bundle path as first launch, without serving or opening
/// a browser. Keep private first-account/control capabilities out of the result.
pub fn setup(home: &Path) -> Result<Value> {
    create_inner(home, true)?;
    let home = std::fs::canonicalize(home)?;
    let config = Config::load(&home)?;
    let executable = home.join("bin").join(if cfg!(windows) {
        "acp-slack-adapter.exe"
    } else {
        "acp-slack-adapter"
    });
    Ok(json!({
        "schema":"com.microsoft.amplifier.slack-native-setup/1",
        "created":true,
        "id":config.id,
        "home":home,
        "executable":executable,
        "version":env!("CARGO_PKG_VERSION"),
        "serving":false,
        "management":{
            "kind":"installation-local-cli",
            "api_version":"acp-slack-adapter.management/1",
            "relative_api_paths":true,
            "requires_running_connector":true
        }
    }))
}

pub fn create(home: &Path) -> Result<()> {
    create_inner(home, false)
}
fn create_inner(home: &Path, managed: bool) -> Result<()> {
    ensure!(
        !home.exists(),
        "This folder already exists; choose a new installation folder"
    );
    let parent = home.parent().context("Missing installation parent")?;
    std::fs::create_dir_all(parent)?;
    let stage = parent.join(format!(".slack-setup-{}", uuid::Uuid::new_v4()));
    let mut stage_created = false;
    let result = (|| {
        platform::private_create_dir(&stage)?;
        stage_created = true;
        for dir in ["bin", "secrets", "client"] {
            platform::private_create_dir(&stage.join(dir))?;
        }
        let owner = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "owner".into());
        let mut config = Config::new("Slack connection".into(), owner);
        config.server.auth_mode = AuthMode::LocalPassword;
        config.server.web_transport = WebTransport::LocalHttp;
        config.save(&stage)?;
        let mut marker = json!({"schema":1,"id":config.id,"update_owner":"installation"});
        if managed {
            marker["manager_owner"] = json!(crate::native_manager::OWNER);
        }
        platform::private_create(
            &stage.join("native-installation.json"),
            &serde_json::to_vec(&marker)?,
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
    if result.is_err() && stage_created && stage.exists() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}

pub async fn launch(home: PathBuf, open_browser: bool) -> Result<()> {
    if !home.join("adapter.json").exists() {
        create(&home)?;
    }
    let config = Config::load(&home)?;
    if crate::native_manager::is_managed(&home)? {
        let observed = crate::native_manager::status(&home).await?;
        let result = if observed["state"] == "ready" {
            observed
        } else {
            ensure!(
                matches!(
                    observed["state"].as_str(),
                    Some("inactive" | "not_registered")
                ),
                "Native connector startup is unresolved; inspect the original request instead of starting another one"
            );
            crate::native_manager::start(&home, uuid::Uuid::new_v4(), None).await?
        };
        ensure!(
            result["state"] == "ready"
                && result["request_current"] == true
                && result["launch_observed"] == true,
            "Native connector startup is not confirmed; inspect native-status and retain the request identity"
        );
        return ready(&home, &Config::load(&home)?, open_browser);
    }
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
        Err(error) if crate::process::lock_busy(&error) => {
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
    ensure!(
        !crate::native_manager::is_managed(home)?,
        "This connector uses its own native manager with startup at sign-in off. Changing that preference is not yet supported; its existing service was preserved."
    );
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
    if crate::native_manager::is_managed(&state.home)? {
        return Ok(Json(
            json!({"configured":false,"change_supported":false,"manager_owner":crate::native_manager::OWNER,"reason":"Startup at sign-in is off. This manager profile does not yet support changing that preference."}),
        ));
    }
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

// Slack's fixed setup POST owns a 180-second helper. Its reply envelope must
// allow that owner to finish and serialize its response; all other routes keep
// their existing 90-second transport bound. This never repeats a request.
fn local_api_timeout(method: &str, path: &str) -> Duration {
    Duration::from_secs(if method == "POST" && path == "setup/slack" {
        180 + 10
    } else {
        90
    })
}

pub async fn local_api(
    home: &Path,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    ensure!(
        !path.contains("..") && !path.contains("://") && !path.starts_with('/'),
        "Use an API path relative to /api/v1, such as \"status\""
    );
    let config = Config::load(home)?;
    let token_path = home.join("secrets/control-token");
    config::check_private(&token_path)?;
    let token = std::fs::read_to_string(token_path)?;
    if config.server.web_transport == WebTransport::LocalHttp {
        return tokio::time::timeout(
            local_api_timeout(method, path),
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
        .timeout(local_api_timeout(method, path))
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
    api_result(method, path, status, &response.bytes().await?)
}
/// The value a management API call returned, or an error naming what the
/// service answered. An unknown route answers with an empty body, and a proxy
/// may answer with HTML; decoding that as JSON first reported a syntax error
/// in place of the HTTP status.
fn api_result(
    method: &str,
    path: &str,
    status: axum::http::StatusCode,
    body: &[u8],
) -> Result<Value> {
    let value = serde_json::from_slice::<Value>(body).ok();
    if status.is_success() {
        return value.with_context(|| {
            format!("{method} {path}: the connector answered HTTP {status}, but not with JSON")
        });
    }
    if let Some(message) = value.as_ref().and_then(|v| v["error"].as_str()) {
        anyhow::bail!("{message}");
    }
    if value.is_some() {
        anyhow::bail!("Management operation failed (HTTP {status})");
    }
    if status == axum::http::StatusCode::NOT_FOUND {
        // The most likely slip is repeating the prefix this command already adds.
        let relative = path
            .strip_prefix("api/v1/")
            .or_else(|| path.strip_prefix("v1/"))
            .filter(|rest| !rest.is_empty());
        anyhow::bail!(
            "{method} {path}: the connector has no such management API path (HTTP {status}). \
             Paths are relative to /api/v1, for example \"status\" or \"tokens\"{}",
            relative.map_or(".".to_owned(), |rest| format!("; try \"{rest}\"."))
        );
    }
    anyhow::bail!("{method} {path}: the connector answered HTTP {status} without a JSON body")
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
    api_result(method, path, status, &bytes)
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

#[cfg(test)]
mod api_result_tests {
    use super::{api_result, local_api_timeout};
    use axum::http::StatusCode;
    use serde_json::json;
    use std::time::Duration;

    fn error(path: &str, status: StatusCode, body: &[u8]) -> String {
        api_result("GET", path, status, body)
            .unwrap_err()
            .to_string()
    }
    #[test]
    fn only_fixed_slack_setup_post_carries_the_helper_response_envelope() {
        assert_eq!(
            local_api_timeout("POST", "setup/slack"),
            Duration::from_secs(190)
        );
        for (method, path) in [
            ("GET", "setup/slack"),
            ("PUT", "setup/slack"),
            ("POST", "setup/slack/"),
            ("POST", "setup/slack?retry=1"),
            ("POST", "setup/complete"),
            ("POST", "native-update"),
            ("GET", "status"),
            ("post", "setup/slack"),
        ] {
            assert_eq!(
                local_api_timeout(method, path),
                Duration::from_secs(90),
                "unexpected extended route {method} {path}"
            );
        }
    }
    #[test]
    fn a_wrong_path_reports_its_status_and_how_paths_are_written() {
        // An unknown route answers 404 with an empty body.
        let repeated = error("api/v1/status", StatusCode::NOT_FOUND, b"");
        assert!(repeated.contains("HTTP 404 Not Found"), "{repeated}");
        assert!(repeated.contains("relative to /api/v1"), "{repeated}");
        assert!(repeated.contains("try \"status\""), "{repeated}");
        assert!(!repeated.contains("EOF"), "{repeated}");
        let unknown = error("nothing-here", StatusCode::NOT_FOUND, b"");
        assert!(unknown.contains("HTTP 404 Not Found"), "{unknown}");
        assert!(unknown.contains("\"status\" or \"tokens\"."), "{unknown}");
        assert!(!unknown.contains("try"), "{unknown}");
    }
    #[test]
    fn other_bodies_that_are_not_json_report_the_status() {
        let proxy = error(
            "status",
            StatusCode::BAD_GATEWAY,
            b"<html>Bad Gateway</html>",
        );
        assert_eq!(
            proxy,
            "GET status: the connector answered HTTP 502 Bad Gateway without a JSON body"
        );
        let page = error("status", StatusCode::OK, b"<html></html>");
        assert_eq!(
            page,
            "GET status: the connector answered HTTP 200 OK, but not with JSON"
        );
    }
    #[test]
    fn answers_from_the_service_itself_are_unchanged() {
        assert_eq!(
            api_result("GET", "status", StatusCode::OK, br#"{"enabled":true}"#).unwrap(),
            json!({"enabled":true})
        );
        // A real route's 404 keeps its own explanation; the path was right.
        assert_eq!(
            error(
                "views/absent",
                StatusCode::NOT_FOUND,
                br#"{"error":"Missing view"}"#
            ),
            "Missing view"
        );
        assert_eq!(
            error("status", StatusCode::INTERNAL_SERVER_ERROR, b"{}"),
            "Management operation failed (HTTP 500 Internal Server Error)"
        );
    }
}
