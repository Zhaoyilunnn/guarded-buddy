//! End-to-end integration tests for `buddy`.
//!
//! Config is isolated via a temporary `HOME` so tests never read
//! `~/.config/buddy/config.toml` or email real recipients via mutt.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use std::time::{Duration, Instant};

const CLAUDE_SAMPLE: &str = include_str!("fixtures/claude/session-sample.jsonl");
const CURSOR_SAMPLE: &str = include_str!("fixtures/cursor/transcript-sample.jsonl");

const CODEX_BOUNDARY: &str = concat!(
    "{\"timestamp\":\"2026-07-15T16:25:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"bbbb2222-0000-4000-8000-000000000000\",\"cwd\":\"/home/zhaoyilun/boundary-proj\"}}\n",
    "{\"timestamp\":\"2026-07-15T16:30:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"跨日边界的提问\"}}\n",
    "{\"timestamp\":\"2026-07-15T16:31:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"跨日边界的回答\"}}\n",
);

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn fake_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    write(
        &h.join(".codex/sessions/2026/07/15/rollout-2026-07-15T16-25-00-bbbb2222.jsonl"),
        CODEX_BOUNDARY,
    );
    write(
        &h.join(".claude/projects/-home-zhaoyilun-notes-app/a1b2c3d4.jsonl"),
        CLAUDE_SAMPLE,
    );
    write(
        &h.join(".cursor/projects/-home-zhaoyilun-docs/agent-transcripts/b76effdc-7aea-4593-9b77-64611a6ad4cc/b76effdc-7aea-4593-9b77-64611a6ad4cc.jsonl"),
        CURSOR_SAMPLE,
    );
    write(
        &h.join(".gemini/antigravity-cli/brain/4194a992-fddf-4a11-8f86-050f9c2470c9/.system_generated/logs/transcript.jsonl"),
        &agy_fixture(),
    );
    home
}

fn agy_fixture() -> String {
    let mut s = String::new();
    s.push_str("{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-07-14T01:10:00Z\",\"content\":\"<USER_REQUEST>\\n帮我调整量子蓝图\\n</USER_REQUEST>\\n<ADDITIONAL_METADATA>\\ntime\\n</ADDITIONAL_METADATA>\"}\n");
    s.push_str("{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-07-14T01:10:30Z\",\"content\":\"已调整蓝图。\"}\n");
    s
}

fn fake_backend_script() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fake-backend.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\ncat > ./stdin-capture.txt\necho '# 假周报：本周写了代码'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (tmp, path)
}

fn fake_plan_script() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fake-plan.sh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
cat > /dev/null
cat <<'EOF'
{"todos":[{"id":"t1","title":"noop","detail":"x","autonomy":"needs_human","confidence":0.5,"needs_human_reason":"demo","workspace":null,"acceptance":"n/a","evidence":[]}]}
EOF
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (tmp, path)
}

/// Empty config HOME (no `~/.config/buddy`) + chat `--home` stays separate.
/// Keep the returned TempDir alive for the whole test (including detached workers).
fn base_cmd() -> (tempfile::TempDir, Command) {
    let config_home = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("buddy").unwrap();
    cmd.env("TZ", "Asia/Shanghai");
    cmd.env("HOME", config_home.path());
    (config_home, cmd)
}

fn wait_for_file(path: &Path, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if path.exists()
            && let Ok(content) = std::fs::read_to_string(path)
            && !content.is_empty()
        {
            return content;
        }
        if start.elapsed() > timeout {
            panic!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn collect_end_to_end_with_day_boundary() {
    let (_cfg, mut cmd) = base_cmd();
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    cmd.args([
        "wr",
        "collect",
        "--from",
        "2026-07-12",
        "--to",
        "2026-07-18",
        "--home",
        home.path().to_str().unwrap(),
        "--out",
        out.path().to_str().unwrap(),
    ])
    .assert()
    .success();

    let dir = out.path().join("2026-07-12_2026-07-18");
    let d16 = std::fs::read_to_string(dir.join("2026-07-16.md")).unwrap();
    assert!(d16.contains("跨日边界的提问"));
}

#[test]
fn sync_then_collect_reads_archived_device() {
    let source_home = fake_home();
    let archive = tempfile::tempdir().unwrap();
    let (_cfg, mut sync_cmd) = base_cmd();
    sync_cmd
        .args([
            "sync",
            "--home",
            source_home.path().to_str().unwrap(),
            "--path",
            archive.path().to_str().unwrap(),
            "--device",
            "work-laptop",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("created="));

    let config_home = tempfile::tempdir().unwrap();
    write(
        &config_home.path().join(".config/buddy/config.toml"),
        &format!(
            "[sync]\npath = {:?}\ndevice = \"home-pc\"\n",
            archive.path().to_str().unwrap()
        ),
    );
    let empty_home = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut collect = Command::cargo_bin("buddy").unwrap();
    collect
        .env("TZ", "Asia/Shanghai")
        .env("HOME", config_home.path())
        .args([
            "wr",
            "collect",
            "--from",
            "2026-07-12",
            "--to",
            "2026-07-18",
            "--home",
            empty_home.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let day =
        std::fs::read_to_string(out.path().join("2026-07-12_2026-07-18/2026-07-16.md")).unwrap();
    assert!(day.contains("跨日边界的提问"));
}

#[test]
fn wr_run_auto_syncs_when_configured() {
    let config_home = tempfile::tempdir().unwrap();
    let source_home = fake_home();
    let archive = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let (_scripts, backend) = fake_backend_script();
    write(
        &config_home.path().join(".config/buddy/config.toml"),
        &format!(
            "[sync]\npath = {:?}\ndevice = \"work-laptop\"\n",
            archive.path().to_str().unwrap()
        ),
    );

    let mut cmd = Command::cargo_bin("buddy").unwrap();
    cmd.env("TZ", "Asia/Shanghai")
        .env("HOME", config_home.path())
        .args([
            "wr",
            "run",
            "--from",
            "2026-07-12",
            "--to",
            "2026-07-18",
            "--home",
            source_home.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
            "--cmd",
            backend.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Synced"));

    assert!(
        archive
            .path()
            .join(
                "devices/work-laptop/home/.claude/projects/-home-zhaoyilun-notes-app/a1b2c3d4.jsonl"
            )
            .is_file()
    );
    let report = wait_for_file(
        &out.path().join("2026-07-12_2026-07-18/report.md"),
        Duration::from_secs(10),
    );
    assert!(report.contains("假周报"));
}

#[test]
fn wr_run_schedules_background() {
    let (_cfg, mut cmd) = base_cmd();
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    let (_t, script) = fake_backend_script();
    cmd.args([
        "wr",
        "run",
        "--from",
        "2026-07-12",
        "--to",
        "2026-07-18",
        "--home",
        home.path().to_str().unwrap(),
        "--out",
        out.path().to_str().unwrap(),
        "--cmd",
        script.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "Summarization started in background",
    ));

    let dir = out.path().join("2026-07-12_2026-07-18");
    let report = wait_for_file(&dir.join("report.md"), Duration::from_secs(10));
    assert!(report.contains("假周报"));
    // No mail_to in isolated config → worker must not create a mail attempt that needs mutt.
    // (If real config leaked, mutt could fire; isolation is the guard.)
}

#[test]
fn wr_report_worker_writes_report_md() {
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    let range_args = [
        "--from",
        "2026-07-12",
        "--to",
        "2026-07-18",
        "--home",
        home.path().to_str().unwrap(),
        "--out",
        out.path().to_str().unwrap(),
    ];
    let (_cfg1, mut collect) = base_cmd();
    collect
        .args(["wr", "collect"])
        .args(range_args)
        .assert()
        .success();
    let (_t, script) = fake_backend_script();
    let (_cfg2, mut report_cmd) = base_cmd();
    report_cmd
        .args(["wr", "report", "--worker"])
        .args(range_args)
        .args(["--cmd", script.to_str().unwrap()])
        .assert()
        .success();
    let report =
        std::fs::read_to_string(out.path().join("2026-07-12_2026-07-18/report.md")).unwrap();
    assert!(report.contains("假周报"));
}

#[test]
fn wr_api_without_key_exits_1() {
    let (_cfg, mut cmd) = base_cmd();
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    cmd.args([
        "wr",
        "run",
        "--from",
        "2026-07-12",
        "--to",
        "2026-07-18",
        "--home",
        home.path().to_str().unwrap(),
        "--out",
        out.path().to_str().unwrap(),
        "--backend",
        "api",
        "--api-key-env",
        "AIW_IT_DEFINITELY_MISSING_KEY",
    ])
    .env_remove("AIW_IT_DEFINITELY_MISSING_KEY")
    .assert()
    .failure()
    .code(1)
    .stderr(predicate::str::contains("not set"));
}

#[test]
fn wr_mail_missing_report_fails() {
    let (_cfg, mut cmd) = base_cmd();
    cmd.args([
        "wr",
        "mail",
        "/nonexistent/aiw-report.md",
        "--mail-to",
        "a@example.com",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("not found"));
}

#[test]
fn signoff_plan_with_fake_llm() {
    let (_cfg, mut cmd) = base_cmd();
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    let (_t, script) = fake_plan_script();
    cmd.args([
        "signoff",
        "plan",
        "--home",
        home.path().to_str().unwrap(),
        "--out",
        out.path().to_str().unwrap(),
        "--cmd",
        script.to_str().unwrap(),
        "--window-hours",
        "876000",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Signoff plan"));

    let signoff_root = out.path().join("signoff");
    let day_dir = std::fs::read_dir(&signoff_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(day_dir.join("plan.json").exists());
    assert!(day_dir.join("context.md").exists());
}

#[test]
fn completions_bash_emits_script() {
    let (_cfg, mut cmd) = base_cmd();
    cmd.args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("buddy"));
}

#[test]
fn sources_lists_detected_and_missing() {
    let (_cfg, mut cmd) = base_cmd();
    let home = tempfile::tempdir().unwrap();
    write(
        &home.path().join(".claude/projects/p/s.jsonl"),
        CLAUDE_SAMPLE,
    );
    cmd.args(["wr", "sources", "--home", home.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ Claude Code"))
        .stdout(predicate::str::contains("✗ Codex"));
}

#[test]
fn mail_sends_with_fake_mutt() {
    let (_cfg, mut cmd) = base_cmd();
    let report_dir = tempfile::tempdir().unwrap();
    let range_dir = report_dir.path().join("2026-07-12_2026-07-18");
    std::fs::create_dir_all(&range_dir).unwrap();
    let report = range_dir.join("report.md");
    std::fs::write(&report, "# weekly\n").unwrap();

    let bin = tempfile::tempdir().unwrap();
    let mutt = bin.path().join("mutt");
    std::fs::write(
        &mutt,
        "#!/bin/sh\nif [ \"$1\" = \"-v\" ]; then exit 0; fi\ncat > /dev/null\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&mutt, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![bin.path().to_path_buf()];
    if let Some(old) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&old));
    }
    let path = std::env::join_paths(paths).unwrap();

    cmd.env("PATH", &path)
        .args([
            "wr",
            "mail",
            report.to_str().unwrap(),
            "--mail-to",
            "a@example.com",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Emailed"));
}
