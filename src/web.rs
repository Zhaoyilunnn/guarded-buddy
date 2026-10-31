//! Authenticated loopback receiver for browser-owned ChatGPT snapshots.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::WebCommand;

pub const MAX_BODY: usize = 20 * 1024 * 1024;

pub fn history_root(home: &Path) -> PathBuf {
    home.join(".local/share/buddy/web/chatgpt")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    /// A SHA-256 namespace derived in the browser; no login credentials are accepted.
    pub account: String,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub conversation: Value,
}

impl Snapshot {
    pub fn id(&self) -> &str {
        self.conversation["conversation_id"].as_str().unwrap_or("")
    }
    pub fn revision(&self) -> f64 {
        self.conversation["update_time"].as_f64().unwrap_or(0.0)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != 1
            || self.account.len() != 64
            || !self.account.bytes().all(|c| c.is_ascii_hexdigit())
            || self.id().is_empty()
            || self.id().len() > 128
            || !self
                .id()
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || !self.conversation["mapping"].is_object()
            || !self.conversation["update_time"].is_number()
            || self.revision() <= 0.0
            || self.conversation["current_node"].as_str().is_none()
        {
            bail!("invalid ChatGPT snapshot");
        }
        // Reject credential-shaped fields rather than persisting authentication data.
        for key in ["accessToken", "access_token", "cookies", "authorization"] {
            if self.conversation.get(key).is_some() {
                bail!("unexpected credential field");
            }
        }
        Ok(())
    }
}

/// Create private directories while refusing symlink components below HOME.
fn private_dir(home: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    let mut path = home.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            bail!("invalid storage path");
        }
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => (),
            Ok(_) => bail!("storage path is not a real directory: {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&path)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn pairing_token(home: &Path) -> anyhow::Result<String> {
    use std::io::Write;
    let dir = private_dir(home, Path::new(".local/share/buddy/web"))?;
    let path = dir.join("token");
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            bail!("invalid token file");
        }
        let token = std::fs::read_to_string(path)?;
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid pairing token");
        }
        return Ok(token);
    }
    let bytes: [u8; 32] = rand::random();
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    match private_file(&path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return pairing_token(home),
        Err(e) => return Err(e.into()),
    }
    Ok(token)
}

pub fn save_snapshot(home: &Path, snapshot: &Snapshot) -> anyhow::Result<&'static str> {
    use std::io::Write;
    snapshot.validate()?;
    let dir = private_dir(
        home,
        &Path::new(".local/share/buddy/web/chatgpt").join(&snapshot.account),
    )?;
    let path = dir.join(format!("{}.json", snapshot.id()));
    let mut existed = false;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("invalid snapshot destination");
            }
            existed = true;
            let old: Snapshot = serde_json::from_slice(&std::fs::read(&path)?)?;
            if old.revision() > snapshot.revision()
                || (old.revision() == snapshot.revision() && old.captured_at > snapshot.captured_at)
            {
                return Ok("stale");
            }
            if old.conversation == snapshot.conversation {
                return Ok("skipped");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let temp = dir.join(format!(".{}.{}.tmp", snapshot.id(), rand::random::<u64>()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = private_file(&temp)?;
        file.write_all(&serde_json::to_vec(snapshot)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(if existed { "updated" } else { "created" })
}

#[derive(Clone)]
struct Receiver {
    home: PathBuf,
    token: String,
    writes: Arc<Mutex<()>>,
    host: String,
}

fn authorized(headers: &HeaderMap, receiver: &Receiver) -> bool {
    // Extension service workers send no web Origin, or their extension origin.
    let origin_ok = headers.get("origin").is_none_or(|o| {
        o.to_str()
            .is_ok_and(|s| s.starts_with("chrome-extension://"))
    });
    origin_ok
        && headers
            .get("host")
            .is_some_and(|h| h == receiver.host.as_str())
        && headers
            .get("authorization")
            .is_some_and(|h| h == format!("Bearer {}", receiver.token).as_str())
}

async fn ingest(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !authorized(&headers, &receiver) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid token, host or origin".into(),
        ));
    }
    let snapshot: Snapshot = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid JSON".into()))?;
    snapshot
        .validate()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let _lock = receiver.writes.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "receiver lock failed".into(),
        )
    })?;
    let status = save_snapshot(&receiver.home, &snapshot)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({"status": status})))
}

async fn health(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &receiver) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(json!({"app": "buddy", "schema_version": 1})))
}

pub fn run(command: WebCommand, default_home: &Path) -> anyhow::Result<()> {
    match command {
        WebCommand::Pair { home } => println!(
            "{}",
            pairing_token(home.as_deref().unwrap_or(default_home))?
        ),
        WebCommand::Status { port, home } => {
            let home = home.as_deref().unwrap_or(default_home);
            let count = walkdir::WalkDir::new(history_root(home))
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "json")
                })
                .count();
            println!("Saved ChatGPT conversations: {count}");
            let token = std::fs::read_to_string(home.join(".local/share/buddy/web/token"))
                .context("not paired; run buddy web pair")?;
            let response = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(3))
                .build()?
                .get(format!("http://127.0.0.1:{port}/health"))
                .bearer_auth(token)
                .send()?
                .error_for_status()?;
            let body: Value = response.json()?;
            if body["app"] != "buddy" {
                bail!("unexpected receiver");
            }
            println!("Receiver: online (127.0.0.1:{port})");
        }
        WebCommand::Serve { port, home } => {
            let home = home.unwrap_or_else(|| default_home.to_path_buf());
            let token = pairing_token(&home)?;
            let receiver = Receiver {
                home,
                token,
                writes: Arc::new(Mutex::new(())),
                host: format!("127.0.0.1:{port}"),
            };
            tokio::runtime::Runtime::new()?.block_on(async {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                let router = Router::new().route("/health", get(health)).route("/v1/chatgpt", post(ingest))
                    .layer(DefaultBodyLimit::max(MAX_BODY)).with_state(receiver);
                eprintln!("ChatGPT receiver: http://127.0.0.1:{port} (Ctrl-C to stop). Pair with buddy web pair.");
                axum::serve(listener, router).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await
            })?;
        }
    }
    Ok(())
}
