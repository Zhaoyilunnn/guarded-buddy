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

pub const DEFAULT_MAX_BODY_MIB: u64 = 100;

fn max_body_bytes(
    cli_mib: Option<u64>,
    config: &crate::config::WebConfig,
) -> anyhow::Result<usize> {
    let mib = cli_mib
        .or(config.max_body_mib)
        .unwrap_or(DEFAULT_MAX_BODY_MIB);
    let bytes = mib.checked_mul(1024 * 1024)
        .filter(|bytes| *bytes > 0)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .context("web.max_body_mib / --max-body-mib must be positive and fit in the platform's address space")?;
    Ok(bytes)
}

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
    pub fn newer_than(&self, other: &Self) -> bool {
        self.revision() > other.revision()
            || (self.revision() == other.revision() && self.captured_at > other.captured_at)
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
                if let Err(error) = builder.create(&path) {
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    let meta = std::fs::symlink_metadata(&path)?;
                    if !meta.is_dir() || meta.file_type().is_symlink() {
                        bail!("invalid storage directory: {}", path.display());
                    }
                }
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
    snapshot.validate()?;
    let root = private_dir(home, Path::new(".local/share/buddy/web/chatgpt"))?;
    let dir = private_dir(&root, Path::new(&snapshot.account))?;
    let _lock = account_lock(&dir)?;
    save_dated(&dir, snapshot)
}

/// Save one canonical conversation, merging and removing legacy hash snapshots first.
pub fn save_shared_snapshot(archive: &Path, snapshot: &Snapshot) -> anyhow::Result<&'static str> {
    snapshot.validate()?;
    std::fs::create_dir_all(archive)?;
    let root = private_dir(archive, Path::new("chatgpt"))?;
    let dir = private_dir(&root, Path::new(&snapshot.account))?;
    let _lock = account_lock(&dir)?;
    save_dated(&dir, snapshot)
}

fn snapshot_date(snapshot: &Snapshot) -> anyhow::Result<String> {
    chrono::DateTime::from_timestamp_millis((snapshot.revision() * 1000.0) as i64)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%Y/%m/%d")
                .to_string()
        })
        .context("invalid conversation update time")
}

fn save_dated(dir: &Path, snapshot: &Snapshot) -> anyhow::Result<&'static str> {
    let mut latest = snapshot.clone();
    let mut obsolete = Vec::new();
    let filename = format!("{}.json", snapshot.id());
    let legacy = dir.join(snapshot.id());
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json")
            || (entry.file_name() != filename.as_str() && !path.starts_with(&legacy))
        {
            continue;
        }
        let old = read_snapshot(path)?;
        if old.account != snapshot.account || old.id() != snapshot.id() {
            bail!(
                "snapshot identity does not match its path: {}",
                path.display()
            );
        }
        if old.newer_than(&latest) {
            latest = old;
        }
        obsolete.push(path.to_path_buf());
    }
    let dated = private_dir(dir, Path::new(&snapshot_date(&latest)?))?;
    let destination = dated.join(filename);
    let result = replace_latest(&dated, &latest)?;
    for path in obsolete {
        if path != destination {
            let markdown = path.with_extension("md");
            std::fs::remove_file(path)?;
            match std::fs::remove_file(markdown) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    if legacy.is_dir() {
        match std::fs::remove_dir(&legacy) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(if latest.newer_than(snapshot) {
        "stale"
    } else {
        result
    })
}

/// Consume a legacy copy only after its content or a newer revision is safely stored.
pub(crate) fn move_snapshot_to_shared(
    source: &Path,
    archive: &Path,
) -> anyhow::Result<&'static str> {
    let shared = archive.join("chatgpt");
    let _source_lock = if source.starts_with(&shared) {
        None
    } else {
        Some(account_lock(
            source.parent().context("snapshot has no parent")?,
        )?)
    };
    let snapshot = match read_snapshot(source) {
        Ok(snapshot) => snapshot,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok("skipped");
        }
        Err(error) => return Err(error),
    };
    let destination = shared
        .join(&snapshot.account)
        .join(snapshot_date(&snapshot)?)
        .join(format!("{}.json", snapshot.id()));
    if source == destination {
        return Ok("skipped");
    }
    let result = save_shared_snapshot(archive, &snapshot)?;
    match std::fs::remove_file(source) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(result)
}

pub(crate) fn read_snapshot(path: &Path) -> anyhow::Result<Snapshot> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        bail!("invalid snapshot file: {}", path.display());
    }
    let snapshot: Snapshot =
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(path)?))?;
    snapshot.validate()?;
    Ok(snapshot)
}

// Keep the lock file in place: removing it would let concurrent writers lock different inodes.
fn account_lock(dir: &Path) -> anyhow::Result<std::fs::File> {
    let path = dir.join(".buddy-chatgpt.lock");
    let file = match private_file(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(&path)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("invalid snapshot lock");
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?
        }
        Err(e) => return Err(e.into()),
    };
    file.lock()?;
    Ok(file)
}

fn replace_latest(dir: &Path, snapshot: &Snapshot) -> anyhow::Result<&'static str> {
    use std::io::Write;
    let path = dir.join(format!("{}.json", snapshot.id()));
    let mut existed = false;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("invalid snapshot destination");
            }
            existed = true;
            let old = read_snapshot(&path)?;
            if old.newer_than(snapshot) {
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

// JSON is durable; the worker scans once for recovery, then sleeps until notified.
struct MarkdownWorker {
    jobs: std::sync::mpsc::Sender<Option<PathBuf>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn markdown_needed(path: &Path) -> bool {
    let Ok(source) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    let destination = path.with_extension("md");
    let Ok(meta) = std::fs::symlink_metadata(destination) else {
        return true;
    };
    !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.modified().map_or(true, |modified| modified < source)
}

impl MarkdownWorker {
    fn start(root: PathBuf) -> Self {
        let (jobs, receive) = std::sync::mpsc::channel::<Option<PathBuf>>();
        let thread = std::thread::spawn(move || {
            let mut pending = std::collections::HashSet::new();
            for entry in walkdir::WalkDir::new(&root).follow_links(false) {
                match entry {
                    Ok(entry)
                        if entry.file_type().is_file()
                            && entry.path().extension().is_some_and(|x| x == "json")
                            && markdown_needed(entry.path()) =>
                    {
                        pending.insert(entry.into_path());
                    }
                    Err(error) => eprintln!("Markdown scan failed: {error}"),
                    _ => (),
                }
            }
            loop {
                let mut failed = std::collections::HashSet::new();
                for path in pending.drain() {
                    if let Err(error) = export_markdown(&root, &path) {
                        eprintln!(
                            "Markdown conversion failed for {}: {error:#}",
                            path.display()
                        );
                        failed.insert(path);
                    }
                }
                pending = failed;
                let job = if pending.is_empty() {
                    receive
                        .recv()
                        .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
                } else {
                    receive.recv_timeout(std::time::Duration::from_secs(30))
                };
                match job {
                    Ok(Some(path)) => {
                        pending.insert(path);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    _ => break,
                }
                // Coalesce queued updates before reading the latest saved snapshots.
                for job in receive.try_iter() {
                    match job {
                        Some(path) => {
                            pending.insert(path);
                        }
                        None => return,
                    }
                }
            }
        });
        Self {
            jobs,
            thread: Some(thread),
        }
    }
}

impl Drop for MarkdownWorker {
    fn drop(&mut self) {
        let _ = self.jobs.send(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn export_markdown(root: &Path, path: &Path) -> anyhow::Result<()> {
    let snapshot = match read_snapshot(path) {
        Ok(snapshot) => snapshot,
        Err(_error) if !path.exists() => return Ok(()),
        Err(error) => return Err(error),
    };
    let account = private_dir(root, Path::new(&snapshot.account))?;
    if !path.starts_with(&account) {
        bail!("snapshot account does not match its path");
    }
    let _lock = account_lock(&account)?;
    // Re-read under the writer lock so a delayed conversion cannot publish an old revision.
    if !path.exists() {
        return Ok(());
    }
    let snapshot = read_snapshot(path)?;
    let session = crate::sources::chatgpt::parse(&snapshot).map_err(anyhow::Error::msg)?;
    let title = snapshot.conversation["title"]
        .as_str()
        .unwrap_or("Untitled")
        .replace(['\n', '\r'], " ");
    let mut markdown = format!(
        "# {title}\n\nhttps://chatgpt.com/c/{}\n\nCreated: {}\nUpdated: {}\n",
        snapshot.id(),
        snapshot.conversation["create_time"],
        snapshot.conversation["update_time"]
    );
    for message in session.messages {
        markdown.push_str(&format!(
            "\n## {:?} · {}\n\n{}\n",
            message.role,
            message.timestamp.to_rfc3339(),
            message.text().unwrap_or("")
        ));
    }
    let destination = path.with_extension("md");
    if std::fs::symlink_metadata(&destination)
        .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
    {
        bail!("invalid Markdown destination");
    }
    if std::fs::read_to_string(&destination).ok().as_deref() == Some(&markdown) {
        return Ok(());
    }
    let temp = path.with_extension(format!("{}.tmp", rand::random::<u64>()));
    let result = (|| -> anyhow::Result<()> {
        use std::io::Write;
        let mut file = private_file(&temp)?;
        file.write_all(markdown.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[derive(Clone)]
struct Receiver {
    home: PathBuf,
    archive: Option<PathBuf>,
    token: String,
    writes: Arc<Mutex<()>>,
    host: String,
    markdown: std::sync::mpsc::Sender<Option<PathBuf>>,
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
    let status = match &receiver.archive {
        Some(archive) => save_shared_snapshot(archive, &snapshot),
        None => save_snapshot(&receiver.home, &snapshot),
    }
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if status != "stale" {
        let root = receiver
            .archive
            .as_ref()
            .map(|p| p.join("chatgpt"))
            .unwrap_or_else(|| history_root(&receiver.home));
        let path = root
            .join(&snapshot.account)
            .join(
                snapshot_date(&snapshot)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
            )
            .join(format!("{}.json", snapshot.id()));
        if receiver.markdown.send(Some(path)).is_err() {
            eprintln!("Markdown worker unavailable; restart the receiver to backfill saved JSON");
        }
    }
    Ok(Json(json!({"status": status})))
}

async fn health(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &receiver) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    use sha2::{Digest, Sha256};
    let root = receiver
        .archive
        .as_ref()
        .map(|p| p.join("chatgpt"))
        .unwrap_or_else(|| history_root(&receiver.home));
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let storage_id = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
    Ok(Json(
        json!({"app": "buddy", "schema_version": 1, "storage_id": storage_id}),
    ))
}

pub fn run(
    command: WebCommand,
    default_home: &Path,
    config: &crate::config::Config,
) -> anyhow::Result<()> {
    match command {
        WebCommand::Pair { home } => println!(
            "{}",
            pairing_token(home.as_deref().unwrap_or(default_home))?
        ),
        WebCommand::Status { port, home } => {
            let home = home.as_deref().unwrap_or(default_home);
            let root = config
                .sync
                .path
                .as_ref()
                .map(|p| p.join("chatgpt"))
                .unwrap_or_else(|| history_root(home));
            println!("ChatGPT storage: {}", root.display());
            let count = walkdir::WalkDir::new(root)
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
        WebCommand::Serve {
            port,
            home,
            max_body_mib,
        } => {
            let max_body = max_body_bytes(max_body_mib, &config.web)?;
            let home = home.unwrap_or_else(|| default_home.to_path_buf());
            let token = pairing_token(&home)?;
            if let Some(archive) = &config.sync.path {
                crate::sync::validate_target(&home, archive)?;
                let outcome = crate::sync::migrate_chatgpt(&home, archive);
                if !outcome.failures.is_empty() {
                    bail!("ChatGPT migration failed: {}", outcome.failures.join("; "));
                }
            }
            let root = match &config.sync.path {
                Some(archive) => {
                    std::fs::create_dir_all(archive)?;
                    private_dir(archive, Path::new("chatgpt"))?
                }
                None => private_dir(&home, Path::new(".local/share/buddy/web/chatgpt"))?,
            };
            eprintln!("ChatGPT storage: {}", root.display());
            let _markdown_worker = MarkdownWorker::start(root.clone());
            let receiver = Receiver {
                home,
                archive: config.sync.path.clone(),
                token,
                writes: Arc::new(Mutex::new(())),
                host: format!("127.0.0.1:{port}"),
                markdown: _markdown_worker.jobs.clone(),
            };
            tokio::runtime::Runtime::new()?.block_on(async {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                let router = Router::new().route("/health", get(health)).route("/v1/chatgpt", post(ingest))
                    .layer(DefaultBodyLimit::max(max_body)).with_state(receiver);
                eprintln!("ChatGPT receiver: http://127.0.0.1:{port} (Ctrl-C to stop). Max request body: {} MiB. Pair with buddy web pair.", max_body / (1024 * 1024));
                axum::serve(listener, router).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebConfig;

    #[test]
    fn markdown_backfill_tracks_latest_revision_and_date() {
        let home = tempfile::tempdir().unwrap();
        let mut snapshot = Snapshot {
            schema_version: 1,
            account: "a".repeat(64),
            captured_at: chrono::Utc::now(),
            conversation: json!({"conversation_id": "test", "title": "Example", "create_time": 1700000000,
                "update_time": 1700000000, "current_node": "a", "mapping": {
                    "a": {"parent": null, "message": {"author": {"role": "assistant"},
                    "create_time": 1700000000, "content": {"parts": ["First answer"]}}}}}),
        };
        save_snapshot(home.path(), &snapshot).unwrap();
        let root = history_root(home.path());
        let old = root
            .join(&snapshot.account)
            .join(snapshot_date(&snapshot).unwrap())
            .join("test.json");
        export_markdown(&root, &old).unwrap();
        assert!(
            std::fs::read_to_string(old.with_extension("md"))
                .unwrap()
                .contains("First answer")
        );
        snapshot.conversation["update_time"] = json!(1700259200);
        snapshot.conversation["mapping"]["a"]["message"]["content"]["parts"] =
            json!(["Latest answer"]);
        save_snapshot(home.path(), &snapshot).unwrap();
        assert!(!old.with_extension("md").exists());
        export_markdown(&root, &old).unwrap();
        let new = root
            .join(&snapshot.account)
            .join(snapshot_date(&snapshot).unwrap())
            .join("test.json");
        export_markdown(&root, &new).unwrap();
        export_markdown(&root, &new).unwrap();
        let text = std::fs::read_to_string(new.with_extension("md")).unwrap();
        assert!(text.contains("Latest answer"));
        assert!(!text.contains("First answer"));
        assert!(text.contains("https://chatgpt.com/c/test"));
    }

    #[test]
    fn worker_skips_current_files_and_processes_notifications() {
        let root = tempfile::tempdir().unwrap();
        let account = root.path().join("a".repeat(64));
        std::fs::create_dir(&account).unwrap();
        let path = account.join("test.json");
        let snapshot = Snapshot {
            schema_version: 1,
            account: "a".repeat(64),
            captured_at: chrono::Utc::now(),
            conversation: json!({"conversation_id": "test", "title": "Queued", "update_time": 1700000000,
                "create_time": 1700000000, "current_node": "a", "mapping": {
                "a": {"parent": null, "message": {"author": {"role": "user"},
                "create_time": 1700000000, "content": {"parts": ["Hello"]}}}}}),
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(markdown_needed(&path));
        {
            let _worker = MarkdownWorker::start(root.path().to_path_buf());
        }
        assert!(!markdown_needed(&path));
        let worker = MarkdownWorker::start(root.path().to_path_buf());
        // A new file after startup must be converted through an explicit notification.
        let queued = account.join("queued.json");
        std::fs::write(&queued, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        worker.jobs.send(Some(queued.clone())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !queued.with_extension("md").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            std::fs::read_to_string(queued.with_extension("md"))
                .unwrap()
                .contains("Hello")
        );
    }

    #[test]
    fn body_limit_uses_cli_then_config_then_default() {
        assert_eq!(
            max_body_bytes(None, &WebConfig::default()).unwrap(),
            100 * 1024 * 1024
        );
        let config = WebConfig {
            max_body_mib: Some(48),
        };
        assert_eq!(max_body_bytes(None, &config).unwrap(), 48 * 1024 * 1024);
        assert_eq!(max_body_bytes(Some(64), &config).unwrap(), 64 * 1024 * 1024);
    }

    #[test]
    fn body_limit_rejects_zero_and_overflow() {
        for value in [0, u64::MAX] {
            let config = WebConfig {
                max_body_mib: Some(value),
            };
            assert!(max_body_bytes(None, &config).is_err());
            assert!(max_body_bytes(Some(value), &WebConfig::default()).is_err());
            assert!(max_body_bytes(Some(1), &config).is_ok());
        }
    }
}
