//! End-to-end coverage for ChatGPT snapshots, archive selection and receiver auth.
use buddy::domain::{AgentKind, DateRange};
use buddy::sources::HistorySource;
use buddy::web::{Snapshot, save_snapshot};
use chrono::{Local, TimeZone, Utc};
use serde_json::json;

fn snapshot(revision: f64, answer: &str) -> Snapshot {
    let now = Utc::now().timestamp() as f64;
    Snapshot {
        schema_version: 1,
        account: "a".repeat(64),
        captured_at: Utc::now(),
        conversation: json!({
            "conversation_id": "conversation-1", "title": "Architecture discussion",
            "create_time": now, "update_time": revision, "current_node": "new",
            "mapping": {
                "root": {"parent": null, "message": null},
                "user": {"parent": "root", "message": {"id": "user", "author": {"role": "user"}, "create_time": now, "content": {"parts": ["Question"]}}},
                "old": {"parent": "user", "message": {"author": {"role": "assistant"}, "create_time": now + 1.0, "content": {"parts": ["Discarded branch"]}}},
                "new": {"parent": "user", "message": {"id": "new", "author": {"role": "assistant"}, "create_time": now + 2.0, "content": {"parts": [answer, {"content_type": "image_asset_pointer"}]}}}
            }
        }),
    }
}

fn range() -> DateRange {
    DateRange::last_n_days(2, Local::now().date_naive().succ_opt().unwrap())
}

#[test]
fn snapshot_updates_are_atomic_idempotent_and_reject_older_versions() {
    let home = tempfile::tempdir().unwrap();
    let old = snapshot(100.0, "Original");
    assert_eq!(save_snapshot(home.path(), &old).unwrap(), "created");
    assert_eq!(save_snapshot(home.path(), &old).unwrap(), "skipped");
    let new = snapshot(200.0, "Edited");
    assert_eq!(save_snapshot(home.path(), &new).unwrap(), "updated");
    assert_eq!(save_snapshot(home.path(), &old).unwrap(), "stale");
    let mut delayed = new.clone();
    delayed.captured_at -= chrono::Duration::seconds(1);
    delayed.conversation["title"] = json!("Delayed snapshot");
    assert_eq!(save_snapshot(home.path(), &delayed).unwrap(), "stale");
    let mut warnings = vec![];
    let sessions =
        buddy::sources::chatgpt::ChatgptSource::new(home.path()).collect(&range(), &mut warnings);
    assert!(warnings.is_empty());
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].messages.len(), 2);
    assert_eq!(sessions[0].messages[1].text(), Some("Edited"));
}

#[test]
fn archive_collection_selects_newest_snapshot_before_date_filtering() {
    let live = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let archived_home = archive.path().join("devices/laptop/home");
    std::fs::create_dir_all(&archived_home).unwrap();
    save_snapshot(&archived_home, &snapshot(100.0, "Old reply")).unwrap();
    save_snapshot(live.path(), &snapshot(200.0, "New reply")).unwrap();
    let sources = buddy::app::build_aggregate_sources(
        live.path(),
        Some(archive.path()),
        &Some(vec![AgentKind::Chatgpt]),
        false,
    );
    assert_eq!(sources.len(), 1);
    let mut warnings = vec![];
    let sessions = sources[0].collect(&range(), &mut warnings);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].messages[1].text(), Some("New reply"));
    assert!(warnings.is_empty());
    let previous = Local.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap();
    let mut newest = snapshot(300.0, "Moved outside report range");
    for node in ["user", "new"] {
        newest.conversation["mapping"][node]["message"]["create_time"] =
            json!(previous.timestamp());
    }
    save_snapshot(live.path(), &newest).unwrap();
    assert!(sources[0].collect(&range(), &mut warnings).is_empty());
}

#[test]
fn sync_leaves_local_chatgpt_snapshots_and_pairing_credentials_untouched() {
    let home = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let token = buddy::web::pairing_token(home.path()).unwrap();
    assert_eq!(buddy::web::pairing_token(home.path()).unwrap(), token);
    save_snapshot(home.path(), &snapshot(100.0, "Answer")).unwrap();
    let settings = buddy::cli::EffectiveSync {
        path: archive.path().into(),
        device: "test".into(),
        home: home.path().into(),
        agents: Some(vec![AgentKind::Chatgpt]),
    };
    assert_eq!(buddy::sync::sync(&settings).unwrap().scanned, 0);
    let all_agents = buddy::cli::EffectiveSync {
        agents: None,
        path: settings.path.clone(),
        device: settings.device.clone(),
        home: settings.home.clone(),
    };
    assert_eq!(buddy::sync::sync(&all_agents).unwrap().scanned, 0);
    assert!(!archive.path().join("chatgpt").exists());
    assert!(
        buddy::web::history_root(home.path())
            .join(snapshot(100.0, "Answer").account)
            .join("1970/01/01/conversation-1.json")
            .is_file()
    );
    assert_eq!(buddy::web::pairing_token(home.path()).unwrap(), token);
    assert!(
        !archive
            .path()
            .join("devices/test/home/.local/share/buddy/web/token")
            .exists()
    );
    let unsafe_settings = buddy::cli::EffectiveSync {
        path: buddy::web::history_root(home.path()).join("archive"),
        ..settings
    };
    assert!(buddy::sync::sync(&unsafe_settings).is_err());
}

#[test]
fn sync_does_not_inspect_or_migrate_archived_chatgpt_files() {
    let home = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let paths = [
        archive
            .path()
            .join("devices/legacy/home/.local/share/buddy/web/chatgpt/account/old.json"),
        archive
            .path()
            .join("chatgpt/account/conversation/hash.json"),
    ];
    for path in &paths {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "invalid legacy snapshot").unwrap();
    }
    for agents in [None, Some(vec![AgentKind::Chatgpt])] {
        let settings = buddy::cli::EffectiveSync {
            path: archive.path().into(),
            device: "test".into(),
            home: home.path().into(),
            agents,
        };
        let result = buddy::sync::sync(&settings).unwrap();
        assert_eq!(result.scanned, 0);
        assert!(result.failures.is_empty());
        for path in &paths {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                "invalid legacy snapshot"
            );
        }
    }
}

#[test]
fn shared_conversations_keep_only_latest_and_migrate_device_copies() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let old = snapshot(100.0, "Old");
    let new = snapshot(200.0, "New");
    buddy::web::save_shared_snapshot(archive.path(), &new).unwrap();
    let mut settings = buddy::cli::EffectiveSync {
        path: archive.path().into(),
        device: "a".into(),
        home: a.path().into(),
        agents: Some(vec![AgentKind::Chatgpt]),
    };
    assert_eq!(buddy::sync::sync(&settings).unwrap().created, 0);
    settings.home = b.path().into();
    settings.device = "b".into();
    assert_eq!(buddy::sync::sync(&settings).unwrap().scanned, 0);
    let legacy = archive.path().join("devices/legacy/home");
    std::fs::create_dir_all(&legacy).unwrap();
    save_snapshot(&legacy, &old).unwrap();
    let result = buddy::sync::migrate_chatgpt(settings.home.as_path(), archive.path());
    assert_eq!(result.created, 0);
    assert!(result.failures.is_empty());
    assert!(
        !buddy::web::history_root(&legacy)
            .join(&old.account)
            .join("1970/01/01/conversation-1.json")
            .exists()
    );
    let canonical = archive
        .path()
        .join("chatgpt")
        .join(&new.account)
        .join("1970/01/01")
        .join("conversation-1.json");
    let stored: Snapshot = serde_json::from_slice(&std::fs::read(&canonical).unwrap()).unwrap();
    assert_eq!(stored.revision(), 200.0);
    let empty = tempfile::tempdir().unwrap();
    let sources = buddy::app::build_aggregate_sources(
        empty.path(),
        Some(archive.path()),
        &Some(vec![AgentKind::Chatgpt]),
        false,
    );
    let mut warnings = vec![];
    let sessions = sources[0].collect(&range(), &mut warnings);
    assert!(warnings.is_empty());
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].messages[1].text(), Some("New"));
    assert!(
        !archive
            .path()
            .join("devices/a/home/.local/share/buddy/web/chatgpt")
            .exists()
    );
    let count = walkdir::WalkDir::new(archive.path().join("chatgpt"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(count, 1);
}

#[test]
fn shared_store_replaces_newer_content_and_migrates_hash_history() {
    let archive = tempfile::tempdir().unwrap();
    let first = snapshot(100.0, "First");
    let mut newest = snapshot(200.0, "Latest");
    newest.captured_at = first.captured_at - chrono::Duration::hours(1);
    let legacy = archive
        .path()
        .join("chatgpt")
        .join(&first.account)
        .join(first.id());
    std::fs::create_dir_all(&legacy).unwrap();
    for (name, value) in [("old.json", &first), ("new.json", &newest)] {
        std::fs::write(legacy.join(name), serde_json::to_vec(value).unwrap()).unwrap();
    }
    let result = buddy::sync::migrate_chatgpt(tempfile::tempdir().unwrap().path(), archive.path());
    assert!(result.failures.is_empty(), "{:?}", result.failures);
    assert!(!legacy.exists());
    let path = archive
        .path()
        .join("chatgpt")
        .join(&first.account)
        .join("1970/01/01")
        .join("conversation-1.json");
    let stored: Snapshot = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored.conversation, newest.conversation);
    assert_eq!(
        buddy::web::save_shared_snapshot(archive.path(), &first).unwrap(),
        "stale"
    );
    assert_eq!(
        buddy::web::save_shared_snapshot(archive.path(), &newest).unwrap(),
        "skipped"
    );
    let mut edited = newest.clone();
    edited.captured_at += chrono::Duration::seconds(1);
    edited.conversation["title"] = json!("Renamed");
    assert_eq!(
        buddy::web::save_shared_snapshot(archive.path(), &edited).unwrap(),
        "updated"
    );
    assert_eq!(
        buddy::web::save_shared_snapshot(archive.path(), &newest).unwrap(),
        "stale"
    );
    let mut warnings = vec![];
    let sessions = buddy::sources::chatgpt::ChatgptSource::from_homes(&[])
        .with_archive(Some(archive.path()))
        .collect(&range(), &mut warnings);
    assert!(warnings.is_empty());
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].project.starts_with("Renamed"));
    assert_eq!(sessions[0].messages[1].text(), Some("Latest"));
}

#[test]
fn failed_migration_retains_original_and_concurrent_writes_keep_newest() {
    let home = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let body = snapshot(100.0, "Recoverable");
    let legacy_home = archive.path().join("devices/legacy/home");
    std::fs::create_dir_all(&legacy_home).unwrap();
    save_snapshot(&legacy_home, &body).unwrap();
    std::fs::write(archive.path().join("chatgpt"), "blocked destination").unwrap();
    let result = buddy::sync::migrate_chatgpt(home.path(), archive.path());
    assert!(!result.failures.is_empty());
    assert!(
        buddy::web::history_root(&legacy_home)
            .join(&body.account)
            .join("1970/01/01")
            .join("conversation-1.json")
            .is_file()
    );
    std::fs::remove_file(archive.path().join("chatgpt")).unwrap();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let barrier = &barrier;
        let handles = (1..=8)
            .map(|revision| {
                let root = archive.path();
                scope.spawn(move || {
                    let body = snapshot(revision as f64, &format!("Revision {revision}"));
                    barrier.wait();
                    buddy::web::save_shared_snapshot(root, &body).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    let path = archive
        .path()
        .join("chatgpt")
        .join(&body.account)
        .join("1970/01/01")
        .join("conversation-1.json");
    let stored: Snapshot = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(stored.revision(), 8.0);
}

#[cfg(unix)]
#[test]
fn wr_run_uses_latest_shared_conversation_and_preserves_old_local_copy() {
    let home = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".config/buddy");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[sync]\npath = {:?}\ndevice = \"report-test\"\n",
            archive.path().to_str().unwrap()
        ),
    )
    .unwrap();
    let old = snapshot(100.0, "Obsolete local answer");
    save_snapshot(home.path(), &old).unwrap();
    buddy::web::save_shared_snapshot(archive.path(), &snapshot(200.0, "Latest shared answer"))
        .unwrap();
    assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("buddy"))
        .env("HOME", home.path())
        .args([
            "wr",
            "run",
            "--agents",
            "chatgpt",
            "--days",
            "2",
            "--cmd",
            "/bin/cat",
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let dir = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("report range directory");
    for _ in 0..200 {
        if dir.join("report.md").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        dir.join("report.md").is_file(),
        "{}",
        std::fs::read_to_string(dir.join("report.log")).unwrap_or_default()
    );
    let notes = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.file_name().to_string_lossy().starts_with("202"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notes.contains("Latest shared answer"));
    assert!(!notes.contains("Obsolete local answer"));
    assert!(
        buddy::web::history_root(home.path())
            .join(&old.account)
            .join("1970/01/01/conversation-1.json")
            .exists()
    );
}

#[test]
fn malformed_branch_and_path_payloads_are_rejected() {
    let home = tempfile::tempdir().unwrap();
    let mut body = snapshot(100.0, "Answer");
    body.account = "../escape".into();
    assert!(save_snapshot(home.path(), &body).is_err());
    body.account = "a".repeat(64);
    body.conversation["conversation_id"] = json!("../escape");
    assert!(save_snapshot(home.path(), &body).is_err());
    body.conversation["conversation_id"] = json!("conversation-1");
    body.conversation["mapping"]["user"]["parent"] = json!("new");
    assert!(buddy::sources::chatgpt::parse(&body).is_err());
    body.conversation["mapping"]["user"]["parent"] = json!("root");
    body.conversation["mapping"]["new"]["message"]["create_time"] = serde_json::Value::Null;
    assert!(buddy::sources::chatgpt::parse(&body).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_storage_paths_and_destinations_are_rejected() {
    use std::os::unix::fs::symlink;
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), home.path().join(".local")).unwrap();
    assert!(buddy::web::pairing_token(home.path()).is_err());
    assert!(save_snapshot(home.path(), &snapshot(100.0, "Answer")).is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

struct ReceiverProcess(std::process::Child);
impl Drop for ReceiverProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn receiver_and_cli_collect_enforce_auth_and_render_current_branch() {
    let home = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".config/buddy");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[web]\nmax_body_mib = 48\n[sync]\npath = {:?}\ndevice = \"receiver-test\"\n",
            archive.path().to_str().unwrap()
        ),
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let token = buddy::web::pairing_token(home.path()).unwrap();
    let mut child = ReceiverProcess(
        std::process::Command::new(assert_cmd::cargo::cargo_bin!("buddy"))
            .env("HOME", home.path())
            .args([
                "web",
                "serve",
                "--port",
                &port.to_string(),
                "--home",
                home.path().to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..100 {
        if client
            .get(format!("{base}/health"))
            .bearer_auth(&token)
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "receiver exited early"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready, "receiver did not become ready");
    assert_eq!(
        client
            .get(format!("{base}/health"))
            .send()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/chatgpt"))
            .bearer_auth(&token)
            .body("invalid JSON")
            .send()
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/chatgpt"))
            .bearer_auth(&token)
            .body(vec![b' '; 48 * 1024 * 1024 + 1])
            .send()
            .unwrap()
            .status(),
        413
    );
    let mut payload = snapshot(100.0, "Current answer");
    // Preserve large provider metadata without expanding the rendered conversation.
    payload.conversation["metadata"] = json!("x".repeat(40 * 1024 * 1024));
    let post = || {
        client
            .post(format!("{base}/v1/chatgpt"))
            .bearer_auth(&token)
            .json(&payload)
    };
    assert_eq!(
        post()
            .header("Origin", "https://example.com")
            .send()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        post()
            .header("Host", "example.com")
            .send()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        post()
            .header("Origin", "chrome-extension://test")
            .send()
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        post().send().unwrap().json::<serde_json::Value>().unwrap()["status"],
        "skipped"
    );
    assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("buddy"))
        .env("HOME", home.path())
        .args([
            "web",
            "status",
            "--port",
            &port.to_string(),
            "--home",
            home.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let mut warnings = vec![];
    let sessions = buddy::sources::chatgpt::ChatgptSource::new(home.path())
        .with_archive(Some(archive.path()))
        .collect(&range(), &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(sessions.len(), 1);
    assert!(
        !buddy::web::history_root(home.path())
            .join(&payload.account)
            .join("1970/01/01")
            .join("conversation-1.json")
            .exists()
    );
    assert!(
        archive
            .path()
            .join("chatgpt")
            .join(&payload.account)
            .join("1970/01/01")
            .join("conversation-1.json")
            .is_file()
    );
    assert_eq!(sessions[0].messages[1].text(), Some("Current answer"));
    assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("buddy"))
        .env("HOME", home.path())
        .args([
            "wr",
            "collect",
            "--agents",
            "chatgpt",
            "--days",
            "2",
            "--home",
            home.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let daily = walkdir::WalkDir::new(out.path())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(daily.contains("ChatGPT"));
    assert!(daily.contains("Current answer"));
    assert!(!daily.contains("Discarded branch"));
    assert!(daily.contains("https://chatgpt.com/c/conversation-1"));
}

#[test]
fn update_date_moves_snapshot_and_stale_sync_keeps_latest_date() {
    let archive = tempfile::tempdir().unwrap();
    let old = snapshot(1700000000.0, "Old");
    let new = snapshot(1700259200.0, "New");
    buddy::web::save_shared_snapshot(archive.path(), &old).unwrap();
    buddy::web::save_shared_snapshot(archive.path(), &new).unwrap();
    buddy::web::save_shared_snapshot(archive.path(), &old).unwrap();
    let files: Vec<_> = walkdir::WalkDir::new(archive.path())
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .collect();
    assert_eq!(files.len(), 1);
    let date = chrono::DateTime::from_timestamp(new.revision() as i64, 0)
        .unwrap()
        .with_timezone(&Local)
        .format("%Y/%m/%d")
        .to_string();
    assert_eq!(
        files[0].path(),
        archive
            .path()
            .join("chatgpt")
            .join(&new.account)
            .join(date)
            .join("conversation-1.json")
    );
    let stored: Snapshot =
        serde_json::from_slice(&std::fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(stored.conversation, new.conversation);
}
