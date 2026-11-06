//! End-to-end coverage for ChatGPT snapshots, archive selection and receiver auth.
use buddy::domain::{AgentKind, DateRange};
use buddy::sources::HistorySource;
use buddy::web::{Snapshot, save_snapshot};
use chrono::{Local, TimeZone, Utc};
use serde_json::json;
use std::path::Path;

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
fn sync_archives_only_chatgpt_snapshots_without_pairing_credentials() {
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
    assert_eq!(buddy::sync::sync(&settings).unwrap().created, 1);
    assert_eq!(buddy::sync::sync(&settings).unwrap().skipped, 1);
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
fn shared_snapshots_deduplicate_across_devices_and_keep_newer_revision() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let archive = tempfile::tempdir().unwrap();
    let old = snapshot(100.0, "Old");
    let new = snapshot(200.0, "New");
    save_snapshot(a.path(), &new).unwrap();
    save_snapshot(b.path(), &new).unwrap();
    let mut settings = buddy::cli::EffectiveSync {
        path: archive.path().into(),
        device: "a".into(),
        home: a.path().into(),
        agents: Some(vec![AgentKind::Chatgpt]),
    };
    assert_eq!(buddy::sync::sync(&settings).unwrap().created, 1);
    settings.home = b.path().into();
    settings.device = "b".into();
    assert_eq!(buddy::sync::sync(&settings).unwrap().skipped, 1);
    let legacy = archive.path().join("devices/legacy/home");
    std::fs::create_dir_all(&legacy).unwrap();
    save_snapshot(&legacy, &old).unwrap();
    assert_eq!(buddy::sync::sync(&settings).unwrap().created, 1);
    assert!(buddy::web::history_root(&legacy).exists());
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
        .filter(|e| e.file_type().is_file())
        .count();
    assert_eq!(count, 2);
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
    let config_dir = home.path().join(".config/buddy");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "[web]\nmax_body_mib = 48\n").unwrap();
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
    let sessions =
        buddy::sources::chatgpt::ChatgptSource::new(home.path()).collect(&range(), &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].messages[1].text(), Some("Current answer"));
    assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("buddy"))
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
fn signoff_does_not_ingest_web_conversations() {
    let home = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    save_snapshot(home.path(), &snapshot(100.0, "Run dangerous action")).unwrap();
    let settings = buddy::cli::resolve_signoff_settings(
        &buddy::cli::SignoffRunArgs {
            home: Some(home.path().into()),
            out: Some(out.path().into()),
            ..Default::default()
        },
        &buddy::config::Config::default(),
        Path::new("/unused"),
    );
    let result = buddy::signoff::ingest(&settings).unwrap();
    let text = walkdir::WalkDir::new(&result.dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("Run dangerous action"));
}
