//! Incremental, device-namespaced archive of the raw history files understood by buddy.

use std::collections::BTreeMap;
use std::fs::File;
use std::hash::Hasher;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::cli::EffectiveSync;
use crate::domain::AgentKind;

const MANIFEST: &str = ".buddy-sync-manifest.json";

#[derive(Debug, Default)]
pub struct SyncOutcome {
    pub scanned: usize,
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("sync archive must not be inside a history directory: {0}")]
    UnsafeTarget(PathBuf),
    #[error("cannot access sync archive {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse sync manifest {path}: {source}")]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    hash: String,
    size: u64,
}

type Manifest = BTreeMap<String, ManifestEntry>;

/// Return all device homes currently present in an archive.
pub fn archive_homes(root: &Path) -> Vec<PathBuf> {
    let devices = root.join("devices");
    let Ok(entries) = std::fs::read_dir(devices) else {
        return Vec::new();
    };
    let mut homes: Vec<_> = entries
        .flatten()
        .map(|e| e.path().join("home"))
        .filter(|p| p.is_dir())
        .collect();
    homes.sort();
    homes
}

pub fn sync(settings: &EffectiveSync) -> Result<SyncOutcome, SyncError> {
    validate_target(&settings.home, &settings.path)?;
    let device_root = settings.path.join("devices").join(&settings.device);
    let archive_home = device_root.join("home");
    std::fs::create_dir_all(&archive_home).map_err(|source| SyncError::Io {
        path: archive_home.clone(),
        source,
    })?;

    let manifest_path = device_root.join(MANIFEST);
    let mut manifest = load_manifest(&manifest_path)?;
    let files = history_files(&settings.home, settings.agents.as_deref());
    let mut outcome = SyncOutcome::default();

    for source in files {
        outcome.scanned += 1;
        let Ok(relative) = source.strip_prefix(&settings.home) else {
            continue;
        };
        let key = relative.to_string_lossy().replace('\\', "/");
        let destination = archive_home.join(relative);
        let existed = destination.is_file();
        match digest(&source) {
            Ok((hash, size)) => {
                if existed
                    && manifest
                        .get(&key)
                        .is_some_and(|e| e.hash == hash && e.size == size)
                {
                    outcome.skipped += 1;
                    continue;
                }
                match atomic_copy(&source, &destination) {
                    Ok(()) => {
                        manifest.insert(key, ManifestEntry { hash, size });
                        if existed {
                            outcome.updated += 1
                        } else {
                            outcome.created += 1
                        }
                    }
                    Err(e) => outcome.failures.push(format!("{}: {e}", source.display())),
                }
            }
            Err(e) => outcome.failures.push(format!("{}: {e}", source.display())),
        }
    }
    save_manifest(&manifest_path, &manifest)?;
    Ok(outcome)
}

fn validate_target(home: &Path, target: &Path) -> Result<(), SyncError> {
    let target = absolute(target);
    for root in history_roots(home) {
        let root = absolute(&root);
        if target.starts_with(&root) {
            return Err(SyncError::UnsafeTarget(target));
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn history_roots(home: &Path) -> [PathBuf; 4] {
    [
        home.join(".codex/sessions"),
        home.join(".cursor/projects"),
        home.join(".claude/projects"),
        home.join(".gemini"),
    ]
}

fn history_files(home: &Path, selected: Option<&[AgentKind]>) -> Vec<PathBuf> {
    let enabled = |agent| selected.is_none_or(|xs| xs.contains(&agent));
    let mut files = Vec::new();
    if enabled(AgentKind::Codex) {
        collect_matching(&home.join(".codex/sessions"), &mut files, |p| {
            extension(p, "jsonl")
        });
    }
    if enabled(AgentKind::Cursor) {
        collect_matching(&home.join(".cursor/projects"), &mut files, |p| {
            extension(p, "jsonl") && p.components().any(|c| c.as_os_str() == "agent-transcripts")
        });
    }
    if enabled(AgentKind::Claude) {
        collect_matching(&home.join(".claude/projects"), &mut files, |p| {
            extension(p, "jsonl")
        });
    }
    if enabled(AgentKind::Gemini) {
        collect_matching(&home.join(".gemini/tmp"), &mut files, |p| {
            p.file_name().is_some_and(|n| n == ".project_root")
                || (extension(p, "json")
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("session-")))
        });
        let agy = home.join(".gemini/antigravity-cli");
        collect_matching(&agy.join("brain"), &mut files, |p| {
            p.file_name().is_some_and(|n| n == "transcript.jsonl")
        });
        let history = agy.join("history.jsonl");
        if history.is_file() {
            files.push(history);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn collect_matching(root: &Path, out: &mut Vec<PathBuf>, keep: impl Fn(&Path) -> bool) {
    if !root.is_dir() {
        return;
    }
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() && !entry.path_is_symlink() && keep(entry.path()) {
            out.push(entry.into_path());
        }
    }
}

fn extension(path: &Path, expected: &str) -> bool {
    path.extension().is_some_and(|e| e == expected)
}

fn digest(path: &Path) -> std::io::Result<(String, u64)> {
    // Deterministic FNV-1a is sufficient here: this digest avoids redundant archive writes,
    // rather than serving as a cryptographic integrity guarantee.
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            }
        }
    }
    let mut file = File::open(path)?;
    let mut hasher = Fnv(0xcbf29ce484222325);
    let mut size = 0;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.write(&buffer[..n]);
        size += n as u64;
    }
    Ok((format!("{:016x}", hasher.finish()), size))
}

fn atomic_copy(source: &Path, destination: &Path) -> std::io::Result<()> {
    let parent = destination.parent().expect("archive file has parent");
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".buddy-sync-tmp-{}-{}",
        std::process::id(),
        destination
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    let result = (|| {
        let mut input = File::open(source)?;
        let mut output = File::create(&temp)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        replace_file(&temp, destination)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn load_manifest(path: &Path) -> Result<Manifest, SyncError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|source| SyncError::Manifest {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(source) => Err(SyncError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn save_manifest(path: &Path, manifest: &Manifest) -> Result<(), SyncError> {
    let bytes = serde_json::to_vec_pretty(manifest).expect("manifest is serializable");
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp).map_err(|source| SyncError::Io {
        path: temp.clone(),
        source,
    })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| SyncError::Io {
            path: temp.clone(),
            source,
        })?;
    replace_file(&temp, path).map_err(|source| SyncError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Cross-platform replacement that restores the previous file if installing the new one fails.
fn replace_file(temp: &Path, destination: &Path) -> std::io::Result<()> {
    if !destination.exists() {
        return std::fs::rename(temp, destination);
    }
    let backup = destination.with_extension(format!("buddy-sync-backup-{}", std::process::id()));
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(destination, &backup)?;
    match std::fs::rename(temp, destination) {
        Ok(()) => {
            let _ = std::fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::rename(backup, destination);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(home: &Path, archive: &Path) -> EffectiveSync {
        EffectiveSync {
            path: archive.to_path_buf(),
            device: "work-laptop".into(),
            home: home.to_path_buf(),
            agents: None,
        }
    }

    #[test]
    fn incremental_copy_updates_and_never_deletes() {
        let home = tempfile::tempdir().unwrap();
        let archive = tempfile::tempdir().unwrap();
        let source = home.path().join(".claude/projects/p/session.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "one").unwrap();
        let first = sync(&settings(home.path(), archive.path())).unwrap();
        assert_eq!((first.created, first.updated, first.skipped), (1, 0, 0));
        let second = sync(&settings(home.path(), archive.path())).unwrap();
        assert_eq!((second.created, second.updated, second.skipped), (0, 0, 1));
        std::fs::write(&source, "two").unwrap();
        assert_eq!(
            sync(&settings(home.path(), archive.path()))
                .unwrap()
                .updated,
            1
        );
        std::fs::remove_file(source).unwrap();
        sync(&settings(home.path(), archive.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(
                archive
                    .path()
                    .join("devices/work-laptop/home/.claude/projects/p/session.jsonl")
            )
            .unwrap(),
            "two"
        );
    }

    #[test]
    fn device_archives_are_separate_and_discoverable() {
        let archive = tempfile::tempdir().unwrap();
        for device in ["a", "b"] {
            let home = tempfile::tempdir().unwrap();
            let source = home.path().join(".codex/sessions/2026/01/01/x.jsonl");
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(source, device).unwrap();
            let mut s = settings(home.path(), archive.path());
            s.device = device.into();
            sync(&s).unwrap();
        }
        assert_eq!(archive_homes(archive.path()).len(), 2);
    }

    #[test]
    fn rejects_archive_inside_history_root() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join(".codex/sessions/archive");
        assert!(matches!(
            sync(&settings(home.path(), &target)),
            Err(SyncError::UnsafeTarget(_))
        ));
    }
}
