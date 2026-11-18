//! Authenticated loopback receiver for browser-owned ChatGPT snapshots.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
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
    save_dated_at(dir, snapshot, None)
}

fn save_dated_at(
    dir: &Path,
    snapshot: &Snapshot,
    candidates: Option<Vec<PathBuf>>,
) -> anyhow::Result<&'static str> {
    let mut latest = snapshot.clone();
    let mut obsolete = Vec::new();
    let filename = format!("{}.json", snapshot.id());
    let legacy = dir.join(snapshot.id());
    let paths = match candidates {
        Some(paths) => paths,
        None => {
            let mut paths = Vec::new();
            for entry in walkdir::WalkDir::new(dir).follow_links(false) {
                let entry = entry?;
                if entry.path().extension().is_some_and(|ext| ext == "json")
                    && (entry.file_name() == filename.as_str() || entry.path().starts_with(&legacy))
                {
                    paths.push(entry.into_path());
                }
            }
            paths
        }
    };
    for path in &paths {
        if !path.exists() {
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
    errors: Arc<Mutex<std::collections::HashMap<PathBuf, String>>>,
    stages: Arc<Mutex<std::collections::HashMap<PathBuf, (String, f64)>>>,
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
        let errors = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let worker_errors = errors.clone();
        let stages = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let worker_stages = stages.clone();
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
                    let started = std::time::Instant::now();
                    if let Ok(mut stages) = worker_stages.lock() {
                        stages.insert(path.clone(), ("generating".into(), 0.0));
                    }
                    if let Err(error) = export_markdown(&root, &path) {
                        if error.downcast_ref::<MarkdownSuperseded>().is_some() {
                            if let Ok(mut stages) = worker_stages.lock() {
                                stages.insert(path.clone(), ("queued".into(), 0.0));
                            }
                            failed.insert(path);
                            continue;
                        }
                        eprintln!(
                            "Markdown conversion failed for {}: {error:#}",
                            path.display()
                        );
                        if let Ok(mut errors) = worker_errors.lock() {
                            errors.insert(path.clone(), error.to_string());
                        }
                        if let Ok(mut stages) = worker_stages.lock() {
                            stages.insert(
                                path.clone(),
                                ("failed".into(), started.elapsed().as_secs_f64() * 1000.0),
                            );
                        }
                        failed.insert(path);
                    } else if let Ok(mut errors) = worker_errors.lock() {
                        errors.remove(&path);
                        if let Ok(mut stages) = worker_stages.lock() {
                            stages.insert(
                                path.clone(),
                                ("ready".into(), started.elapsed().as_secs_f64() * 1000.0),
                            );
                        }
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
            errors,
            stages,
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

#[derive(Debug, thiserror::Error)]
#[error("snapshot changed during Markdown rendering; queued for retry")]
struct MarkdownSuperseded;

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
    // Read a stable version, then release the writer lock while rendering.
    if !path.exists() {
        return Ok(());
    }
    let snapshot = read_snapshot(path)?;
    let source_meta = std::fs::metadata(path)?;
    let source_time = source_meta.modified()?;
    drop(_lock);
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
    let _lock = account_lock(&account)?;
    if !path.exists() {
        return Ok(());
    }
    let latest_meta = std::fs::metadata(path)?;
    if latest_meta.modified()? != source_time || latest_meta.len() != source_meta.len() {
        return Err(MarkdownSuperseded.into());
    }
    let destination = path.with_extension("md");
    if std::fs::symlink_metadata(&destination)
        .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
    {
        bail!("invalid Markdown destination");
    }
    if std::fs::read_to_string(&destination).ok().as_deref() == Some(&markdown) {
        // Mark an identical export as verified against the latest JSON.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&destination)?
            .set_modified(std::time::SystemTime::now())?;
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
    markdown_errors: Arc<Mutex<std::collections::HashMap<PathBuf, String>>>,
    catalog: Arc<Mutex<ArchiveCache>>,
    catalog_requests: std::sync::mpsc::Sender<Option<()>>,
    snapshot_index: Arc<Mutex<SnapshotIndex>>,
    markdown_stages: Arc<Mutex<std::collections::HashMap<PathBuf, (String, f64)>>>,
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
    let started = std::time::Instant::now();
    let status = receiver
        .snapshot_index
        .lock()
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot index lock failed".into(),
            )
        })?
        .save(&receiver.root(), &snapshot)
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
    let save_ms = started.elapsed().as_secs_f64() * 1000.0;
    receiver
        .update_catalog(&snapshot, status)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({"status": status, "save_ms": save_ms})))
}

// Refresh from disk before each explicit sync or continuation, never from browser acknowledgements.
async fn prepare_sync(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !authorized(&headers, &receiver) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid token, host or origin".into(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        receiver.reconcile_catalog()?;
        Ok::<_, anyhow::Error>(Json(json!({"ready": true})))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct VersionItem {
    id: String,
    revision: f64,
}

#[derive(Deserialize)]
struct VersionQuery {
    account: String,
    items: Vec<VersionItem>,
}

async fn check_versions(
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
    let query: VersionQuery = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid version query".into()))?;
    if query.account.len() != 64
        || !query.account.bytes().all(|b| b.is_ascii_hexdigit())
        || query.items.len() > 100
        || query.items.iter().any(|item| {
            item.id.is_empty()
                || item.id.len() > 128
                || !item
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return Err((StatusCode::BAD_REQUEST, "invalid version query".into()));
    }
    tokio::task::spawn_blocking(move || receiver.disk_versions(&query))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
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
        json!({"app": "buddy", "schema_version": 1, "storage_id": storage_id,
            "storage_path": root, "catalog_version": 1, "shared_directory": receiver.archive.is_some()}),
    ))
}

// Account directory stamps detect new/moved files without walking every file on each save.
#[derive(Default)]
struct AccountIndex {
    paths: std::collections::HashMap<String, Vec<PathBuf>>,
    directories: std::collections::HashMap<PathBuf, std::time::SystemTime>,
}
#[derive(Default)]
struct SnapshotIndex {
    accounts: std::collections::HashMap<String, AccountIndex>,
    scans: usize,
}
impl SnapshotIndex {
    fn save(&mut self, root: &Path, snapshot: &Snapshot) -> anyhow::Result<&'static str> {
        let dir = private_dir(root, Path::new(&snapshot.account))?;
        let _lock = account_lock(&dir)?;
        let valid = self.accounts.get(&snapshot.account).is_some_and(|index| {
            index.directories.iter().all(|(p, t)| {
                std::fs::symlink_metadata(p).is_ok_and(|m| {
                    m.is_dir()
                        && !m.file_type().is_symlink()
                        && m.modified().ok().as_ref() == Some(t)
                })
            })
        });
        if !valid {
            let mut index = AccountIndex::default();
            for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_dir() {
                    index
                        .directories
                        .insert(entry.path().into(), entry.metadata()?.modified()?);
                } else if entry.path().extension().is_some_and(|x| x == "json")
                    && let Some(id) = entry.path().file_stem().and_then(|x| x.to_str())
                {
                    index
                        .paths
                        .entry(id.to_owned())
                        .or_default()
                        .push(entry.path().into());
                }
            }
            self.accounts.insert(snapshot.account.clone(), index);
            self.scans += 1;
        }
        let index = self
            .accounts
            .get_mut(&snapshot.account)
            .expect("account index initialized");
        // Legacy per-conversation directories may contain hash-named snapshots.
        let result = if dir.join(snapshot.id()).exists() {
            save_dated(&dir, snapshot)?
        } else {
            save_dated_at(
                &dir,
                snapshot,
                Some(index.paths.get(snapshot.id()).cloned().unwrap_or_default()),
            )?
        };
        let candidate = dir
            .join(snapshot_date(snapshot)?)
            .join(format!("{}.json", snapshot.id()));
        let paths = index.paths.entry(snapshot.id().into()).or_default();
        paths.retain(|p| p.is_file());
        if candidate.is_file() && !paths.contains(&candidate) {
            paths.push(candidate.clone());
        }
        // Account for our own directory creation/removal before validating the next write.
        for ancestor in candidate
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&dir))
        {
            if ancestor.is_dir() {
                index
                    .directories
                    .insert(ancestor.into(), std::fs::metadata(ancestor)?.modified()?);
            }
        }
        index.directories.retain(|p, _| p.is_dir());
        for (path, time) in &mut index.directories {
            *time = std::fs::metadata(path)?.modified()?;
        }
        Ok(result)
    }
}

struct CatalogWorker {
    stop: std::sync::mpsc::Sender<Option<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl CatalogWorker {
    fn start(receiver: Receiver, receive: std::sync::mpsc::Receiver<Option<()>>) -> Self {
        let stop = receiver.catalog_requests.clone();
        let thread = std::thread::spawn(move || {
            loop {
                if let Err(error) = receiver.reconcile_catalog()
                    && let Ok(mut cache) = receiver.catalog.lock()
                {
                    cache.warnings = vec![error.to_string()];
                    cache.refreshing = false;
                }
                match receive.recv_timeout(std::time::Duration::from_secs(60)) {
                    Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    _ => (),
                }
                let mut stopping = false;
                for signal in receive.try_iter() {
                    if signal.is_none() {
                        stopping = true;
                    }
                }
                if stopping {
                    break;
                }
                if let Ok(mut cache) = receiver.catalog.lock() {
                    cache.refreshing = true;
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for CatalogWorker {
    fn drop(&mut self) {
        let _ = self.stop.send(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct ArchiveCache {
    entries: std::collections::HashMap<PathBuf, (std::time::SystemTime, u64, Value)>,
    by_identity: std::collections::HashMap<(String, String), Vec<PathBuf>>,
    warnings: Vec<String>,
    refreshing: bool,
    checked_at: Option<String>,
}

impl Receiver {
    fn root(&self) -> PathBuf {
        self.archive
            .as_ref()
            .map(|p| p.join("chatgpt"))
            .unwrap_or_else(|| history_root(&self.home))
    }

    fn reconcile_catalog(&self) -> anyhow::Result<()> {
        let original = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock failed"))?
            .entries
            .clone();
        let mut cache = ArchiveCache {
            entries: original.clone(),
            ..ArchiveCache::default()
        };
        let root = self.root();
        let mut seen = std::collections::HashSet::new();
        let mut warnings = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(error.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|x| x != "json")
            {
                continue;
            }
            let path = entry.path().to_path_buf();
            let result = (|| -> anyhow::Result<()> {
                let meta = entry.metadata()?;
                let modified = meta.modified()?;
                if cache
                    .entries
                    .get(&path)
                    .is_none_or(|(time, size, _)| *time != modified || *size != meta.len())
                {
                    let snapshot = read_snapshot(&path)?;
                    if !path.starts_with(root.join(&snapshot.account)) {
                        bail!("snapshot account does not match its path");
                    }
                    let row = json!({
                        "id": snapshot.id(), "account": snapshot.account,
                        "title": snapshot.conversation["title"].as_str().unwrap_or("Untitled"),
                        "url": format!("https://chatgpt.com/c/{}", snapshot.id()),
                        "updated_at": snapshot.revision(), "captured_at": snapshot.captured_at,
                        "saved_at": chrono::DateTime::<chrono::Utc>::from(modified),
                        "path": path, "markdown_path": path.with_extension("md")
                    });
                    cache
                        .entries
                        .insert(path.clone(), (modified, meta.len(), row));
                }
                Ok(())
            })();
            seen.insert(path);
            match result {
                Ok(()) => (),
                Err(error) => {
                    cache.entries.remove(entry.path());
                    warnings.push(format!("{}: {error}", entry.path().display()));
                }
            }
        }
        cache.entries.retain(|path, _| seen.contains(path));
        let mut live = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock failed"))?;
        let mut external_accounts = std::collections::HashSet::new();
        for path in original.keys().chain(cache.entries.keys()) {
            if original.get(path) != cache.entries.get(path)
                && live.entries.get(path) == original.get(path)
                && let Some(account) = cache
                    .entries
                    .get(path)
                    .or_else(|| original.get(path))
                    .and_then(|(_, _, r)| r["account"].as_str())
            {
                external_accounts.insert(account.to_owned());
            }
        }
        // Preserve writes published while the disk scan was in progress.
        for path in original.keys() {
            if !live.entries.contains_key(path) {
                cache.entries.remove(path);
            }
        }
        for (path, entry) in &live.entries {
            if original.get(path) != Some(entry) {
                cache.entries.insert(path.clone(), entry.clone());
            }
        }
        live.entries = cache.entries;
        live.by_identity = std::collections::HashMap::new();
        let identities: Vec<_> = live.entries.iter().filter_map(|(path, (_, _, row))| {
            let identity = (row["account"].as_str()?.to_owned(), row["id"].as_str()?.to_owned());
            Some((identity, path.clone()))
        }).collect();
        for (identity, path) in identities {
            live.by_identity.entry(identity).or_default().push(path);
        }
        live.warnings = warnings;
        live.refreshing = false;
        live.checked_at = Some(chrono::Utc::now().to_rfc3339());
        let live_paths: std::collections::HashSet<_> = live.entries.keys().cloned().collect();
        drop(live);
        if !external_accounts.is_empty() {
            let mut index = self
                .snapshot_index
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot index lock failed"))?;
            for account in external_accounts {
                index.accounts.remove(&account);
            }
        }
        self.markdown_errors
            .lock()
            .map_err(|_| anyhow::anyhow!("Markdown lock failed"))?
            .retain(|p, _| live_paths.contains(p));
        self.markdown_stages
            .lock()
            .map_err(|_| anyhow::anyhow!("Markdown lock failed"))?
            .retain(|p, _| live_paths.contains(p));
        Ok(())
    }

    fn update_catalog(&self, snapshot: &Snapshot, status: &str) -> anyhow::Result<()> {
        if status == "stale" {
            return Ok(());
        }
        let root = self.root();
        let path = root
            .join(&snapshot.account)
            .join(snapshot_date(snapshot)?)
            .join(format!("{}.json", snapshot.id()));
        if !path.is_file() {
            return Ok(());
        }
        if status == "skipped"
            && self
                .catalog
                .lock()
                .map_err(|_| anyhow::anyhow!("catalog lock failed"))?
                .entries
                .contains_key(&path)
        {
            return Ok(());
        }
        let stored_snapshot;
        let stored = if status == "skipped" {
            stored_snapshot = read_snapshot(&path)?;
            &stored_snapshot
        } else {
            snapshot
        };
        let meta = std::fs::metadata(&path)?;
        let row = json!({"id": stored.id(), "account": stored.account,
            "title": stored.conversation["title"].as_str().unwrap_or("Untitled"),
            "url": format!("https://chatgpt.com/c/{}", stored.id()),
            "updated_at": stored.revision(), "captured_at": stored.captured_at,
            "saved_at": chrono::DateTime::<chrono::Utc>::from(meta.modified()?),
            "path": path, "markdown_path": path.with_extension("md")});
        let mut cache = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock failed"))?;
        cache
            .entries
            .retain(|_, (_, _, r)| r["account"] != stored.account || r["id"] != stored.id());
        cache.by_identity.insert(
            (stored.account.clone(), stored.id().to_owned()), vec![path.clone()],
        );
        cache
            .entries
            .insert(path, (meta.modified()?, meta.len(), row));
        Ok(())
    }

    fn disk_versions(&self, query: &VersionQuery) -> anyhow::Result<Value> {
        let cache = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock failed"))?;
        let items: Vec<_> = query
            .items
            .iter()
            .map(|item| {
                let local = cache
                    .by_identity
                    .get(&(query.account.clone(), item.id.clone()))
                    .into_iter()
                    .flatten()
                    .filter_map(|path| {
                        let (modified, size, row) = cache.entries.get(path)?;
                        // A missing or externally changed file cannot justify skipping a download.
                        let meta = std::fs::symlink_metadata(path).ok()?;
                        if !meta.is_file()
                            || meta.file_type().is_symlink()
                            || meta.len() != *size
                            || meta.modified().ok().as_ref() != Some(modified)
                        {
                            return None;
                        }
                        row["updated_at"].as_f64()
                    })
                    .reduce(f64::max);
                let status = match local {
                    Some(local) if item.revision > 0.0 && local == item.revision => "unchanged",
                    Some(local) if item.revision > 0.0 && local > item.revision => "stale",
                    _ => "needed",
                };
                json!({"id": item.id, "status": status})
            })
            .collect();
        Ok(json!({"items": items}))
    }

    fn archive_rows(&self) -> anyhow::Result<(Vec<Value>, Vec<String>)> {
        let cache = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock failed"))?;
        let mut rows: Vec<_> = cache
            .entries
            .values()
            .map(|(_, _, row)| row.clone())
            .collect();
        let warnings = cache.warnings.clone();
        drop(cache);
        rows.sort_by(|a, b| {
            b["saved_at"]
                .as_str()
                .cmp(&a["saved_at"].as_str())
                .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
        });
        Ok((rows, warnings))
    }

    fn decorate_markdown(&self, row: &mut Value) {
        let Some(path) = row["path"].as_str().map(PathBuf::from) else {
            return;
        };
        let needed = markdown_needed(&path);
        let errors = self
            .markdown_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stages = self
            .markdown_stages
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        row["markdown_status"] = json!(if !needed {
            "ready"
        } else if errors.contains_key(&path) {
            "failed"
        } else if stages.get(&path).is_some_and(|s| s.0 == "generating") {
            "generating"
        } else {
            "queued"
        });
        row["markdown_error"] = if needed {
            json!(errors.get(&path))
        } else {
            Value::Null
        };
        row["markdown_ms"] = json!(stages.get(&path).map(|s| s.1));
    }
}

#[derive(Default, Deserialize)]
struct ArchiveQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    account: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
    #[serde(default)]
    refresh: bool,
}

async fn archives(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
    Query(query): Query<ArchiveQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !authorized(&headers, &receiver) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid token, host or origin".into(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        if query.refresh {
            receiver.catalog.lock().map_err(|_| anyhow::anyhow!("catalog lock failed"))?.refreshing = true;
            let _ = receiver.catalog_requests.send(Some(()));
        }
        let (rows, warnings) = receiver.archive_rows()?;
        let accounts: std::collections::BTreeSet<_> = rows
            .iter()
            .filter_map(|r| r["account"].as_str().map(str::to_owned))
            .collect();
        let search = query.q.to_lowercase();
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| {
                (query.account.is_empty() || r["account"] == query.account)
                    && r["title"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&search)
            })
            .collect();
        let total = rows.len();
        let mut items: Vec<_> = rows
            .into_iter()
            .skip(query.offset)
            .take(query.limit.unwrap_or(50).clamp(1, 100))
            .collect();
        for row in &mut items { receiver.decorate_markdown(row); }
        let cache = receiver.catalog.lock().map_err(|_| anyhow::anyhow!("catalog lock failed"))?;
        Ok::<_, anyhow::Error>(Json(
            json!({"items": items, "total": total, "accounts": accounts, "warnings": warnings, "refreshing": cache.refreshing, "checked_at": cache.checked_at}),
        ))
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog worker failed".into(),
        )
    })?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct ArchiveItem {
    account: String,
    id: String,
}

async fn retry_markdown(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
    Json(item): Json<ArchiveItem>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !authorized(&headers, &receiver) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid token, host or origin".into(),
        ));
    }
    // Resolve only existing catalog entries; callers cannot supply filesystem paths.
    tokio::task::spawn_blocking(move || {
        let (rows, _) = receiver
            .archive_rows()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let row = rows
            .iter()
            .find(|r| r["id"] == item.id && r["account"] == item.account)
            .ok_or((StatusCode::NOT_FOUND, "archive not found".into()))?;
        let path = PathBuf::from(row["path"].as_str().unwrap_or(""));
        receiver.markdown.send(Some(path)).map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Markdown worker unavailable".into(),
            )
        })?;
        Ok(Json(json!({"status": "queued"})))
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog worker failed".into(),
        )
    })?
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
            let (catalog_requests, catalog_receive) = std::sync::mpsc::channel();
            let receiver = Receiver {
                home,
                archive: config.sync.path.clone(),
                token,
                writes: Arc::new(Mutex::new(())),
                host: format!("127.0.0.1:{port}"),
                markdown: _markdown_worker.jobs.clone(),
                markdown_errors: _markdown_worker.errors.clone(),
                catalog: Arc::new(Mutex::new(ArchiveCache {
                    refreshing: true,
                    ..ArchiveCache::default()
                })),
                catalog_requests,
                snapshot_index: Arc::new(Mutex::new(SnapshotIndex::default())),
                markdown_stages: _markdown_worker.stages.clone(),
            };
            let _catalog_worker = CatalogWorker::start(receiver.clone(), catalog_receive);
            tokio::runtime::Runtime::new()?.block_on(async {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                let router = Router::new().route("/health", get(health)).route("/v1/chatgpt", post(ingest))
                    .route("/v1/sync/prepare", post(prepare_sync))
                    .route("/v1/sync/check", post(check_versions))
                    .route("/v1/archives", get(archives))
                    .route("/v1/archives/retry", post(retry_markdown))
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
    fn catalog_reconciles_existing_updates_deletions_and_markdown_errors() {
        let home = tempfile::tempdir().unwrap();
        let root = history_root(home.path());
        let mut snapshot = Snapshot {
            schema_version: 1,
            account: "a".repeat(64),
            captured_at: chrono::Utc::now(),
            conversation: json!({"conversation_id": "test", "title": "Before upgrade", "update_time": 1700000000,
                "current_node": "a", "mapping": {"a": {"parent": null, "message": {"author": {"role": "user"},
                "create_time": 1700000000, "content": {"parts": ["Hello"]}}}}}),
        };
        save_snapshot(home.path(), &snapshot).unwrap();
        let path = root
            .join(&snapshot.account)
            .join(snapshot_date(&snapshot).unwrap())
            .join("test.json");
        let (jobs, _receive) = std::sync::mpsc::channel();
        let receiver = Receiver {
            home: home.path().into(),
            archive: None,
            token: "b".repeat(64),
            host: "127.0.0.1:1".into(),
            writes: Arc::new(Mutex::new(())),
            markdown: jobs,
            markdown_errors: Arc::new(Mutex::new(std::collections::HashMap::new())),
            catalog: Arc::new(Mutex::new(ArchiveCache::default())),
            catalog_requests: std::sync::mpsc::channel().0,
            snapshot_index: Arc::new(Mutex::new(SnapshotIndex::default())),
            markdown_stages: Arc::new(Mutex::new(std::collections::HashMap::new())),
        };
        assert_eq!(
            {
                receiver.reconcile_catalog().unwrap();
                receiver.archive_rows().unwrap().0[0]["title"].clone()
            },
            "Before upgrade"
        );
        receiver
            .markdown_errors
            .lock()
            .unwrap()
            .insert(path.clone(), "Conversion failed".into());
        assert_eq!(
            {
                receiver.reconcile_catalog().unwrap();
                let mut row = receiver.archive_rows().unwrap().0.remove(0);
                receiver.decorate_markdown(&mut row);
                row["markdown_status"].clone()
            },
            "failed"
        );
        export_markdown(&root, &path).unwrap();
        assert_eq!(
            {
                receiver.reconcile_catalog().unwrap();
                let mut row = receiver.archive_rows().unwrap().0.remove(0);
                receiver.decorate_markdown(&mut row);
                row["markdown_status"].clone()
            },
            "ready"
        );
        snapshot.conversation["title"] = json!("Renamed conversation");
        snapshot.captured_at = chrono::Utc::now();
        save_snapshot(home.path(), &snapshot).unwrap();
        assert_eq!(
            {
                receiver.reconcile_catalog().unwrap();
                receiver.archive_rows().unwrap().0[0]["title"].clone()
            },
            "Renamed conversation"
        );
        export_markdown(&root, &path).unwrap();
        // JSON metadata can change while rendered Markdown remains identical.
        snapshot.conversation["extra_metadata"] = json!("changed");
        snapshot.captured_at = chrono::Utc::now();
        save_snapshot(home.path(), &snapshot).unwrap();
        export_markdown(&root, &path).unwrap();
        assert!(!markdown_needed(&path));
        std::fs::write(root.join("invalid.json"), "broken").unwrap();
        receiver.reconcile_catalog().unwrap();
        assert_eq!(receiver.archive_rows().unwrap().1.len(), 1);
        std::fs::remove_file(&path).unwrap();
        receiver.reconcile_catalog().unwrap();
        assert!(receiver.archive_rows().unwrap().0.is_empty());
        assert!(receiver.catalog.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn indexed_saves_reuse_paths_and_detect_external_moves() {
        let root = tempfile::tempdir().unwrap();
        let mut index = SnapshotIndex::default();
        let mut snapshot = Snapshot {
            schema_version: 1,
            account: "a".repeat(64),
            captured_at: chrono::Utc::now(),
            conversation: json!({"conversation_id": "one", "title": "Example", "update_time": 1700000000,
                "current_node": "a", "mapping": {"a": {"parent": null, "message": {"author": {"role": "user"},
                    "create_time": 1700000000, "content": {"parts": ["Hello"]}}}}}),
        };
        assert_eq!(index.save(root.path(), &snapshot).unwrap(), "created");
        let scans = index.scans;
        for _ in 0..20 {
            assert_eq!(index.save(root.path(), &snapshot).unwrap(), "skipped");
        }
        assert_eq!(
            index.scans, scans,
            "unchanged directory stamps avoid repeated full walks"
        );
        let old = snapshot.clone();
        snapshot.conversation["update_time"] = json!(1700259200);
        snapshot.conversation["title"] = json!("External newer version");
        let dir = root.path().join(&snapshot.account);
        let _lock = account_lock(&dir).unwrap();
        save_dated(&dir, &snapshot).unwrap();
        drop(_lock);
        assert_eq!(index.save(root.path(), &old).unwrap(), "stale");
        assert!(
            index.scans > scans,
            "an external move invalidates directory stamps"
        );
        let path = dir.join(snapshot_date(&snapshot).unwrap()).join("one.json");
        assert_eq!(
            read_snapshot(&path).unwrap().conversation["title"],
            "External newer version"
        );
        snapshot.conversation["conversation_id"] = json!("two");
        assert_eq!(index.save(root.path(), &snapshot).unwrap(), "created");
        assert!(
            path.is_file(),
            "saving another conversation cannot replace the previous one"
        );
    }

    #[test]
    fn archive_queries_and_reconciliation_do_not_take_the_writer_lock() {
        let home = tempfile::tempdir().unwrap();
        let receiver = Receiver {
            home: home.path().into(),
            archive: None,
            token: "b".repeat(64),
            host: "127.0.0.1:1".into(),
            writes: Arc::new(Mutex::new(())),
            markdown: std::sync::mpsc::channel().0,
            markdown_errors: Arc::new(Mutex::new(std::collections::HashMap::new())),
            markdown_stages: Arc::new(Mutex::new(std::collections::HashMap::new())),
            catalog: Arc::new(Mutex::new(ArchiveCache::default())),
            snapshot_index: Arc::new(Mutex::new(SnapshotIndex::default())),
            catalog_requests: std::sync::mpsc::channel().0,
        };
        std::fs::create_dir_all(receiver.root()).unwrap();
        let guard = receiver.writes.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let copy = receiver.clone();
        let thread = std::thread::spawn(move || {
            copy.reconcile_catalog().unwrap();
            send.send(copy.archive_rows().unwrap()).unwrap();
        });
        let result = receive.recv_timeout(std::time::Duration::from_secs(2));
        drop(guard);
        thread.join().unwrap();
        assert!(
            result.is_ok(),
            "catalog IO must proceed independently of conversation writes"
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
