//! End-to-end integration tests: fake $HOME (four source layouts) + TZ=Asia/Shanghai subprocess +
//! fake CLI backend script.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CLAUDE_SAMPLE: &str = include_str!("fixtures/claude/session-sample.jsonl");
const CURSOR_SAMPLE: &str = include_str!("fixtures/cursor/transcript-sample.jsonl");

/// Cross-day boundary codex rollout: message at 2026-07-15T16:30Z (07-16 00:30 under UTC+8).
const CODEX_BOUNDARY: &str = concat!(
    "{\"timestamp\":\"2026-07-15T16:25:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"bbbb2222-0000-4000-8000-000000000000\",\"cwd\":\"/home/zhaoyilun/boundary-proj\"}}\n",
    "{\"timestamp\":\"2026-07-15T16:30:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"跨日边界的提问\"}}\n",
    "{\"timestamp\":\"2026-07-15T16:31:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"跨日边界的回答\"}}\n",
);

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Build a fake $HOME with all four source layouts (gemini uses dynamically generated agy transcript fixture).
fn fake_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    // codex (cross-day boundary)
    write(
        &h.join(".codex/sessions/2026/07/15/rollout-2026-07-15T16-25-00-bbbb2222.jsonl"),
        CODEX_BOUNDARY,
    );
    // claude
    write(
        &h.join(".claude/projects/-home-zhaoyilun-notes-app/a1b2c3d4.jsonl"),
        CLAUDE_SAMPLE,
    );
    // cursor
    write(
        &h.join(".cursor/projects/-home-zhaoyilun-docs/agent-transcripts/b76effdc-7aea-4593-9b77-64611a6ad4cc/b76effdc-7aea-4593-9b77-64611a6ad4cc.jsonl"),
        CURSOR_SAMPLE,
    );
    // gemini agy
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

/// Fake CLI backend: save stdin to cwd, then print a stub weekly report.
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

fn base_cmd() -> Command {
    let mut cmd = Command::cargo_bin("ai-weekly-report").unwrap();
    cmd.env("TZ", "Asia/Shanghai");
    cmd
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
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    base_cmd()
        .args([
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
    // cross-day boundary: UTC 07-15 16:30 is 07-16 00:30 in +08 → lands in 2026-07-16.md
    let d16 = std::fs::read_to_string(dir.join("2026-07-16.md")).unwrap();
    assert!(d16.contains("跨日边界的提问"));
    assert!(d16.contains("# 2026-07-16 周四 · AI 对话记录"));
    // claude fixture (07-16 UTC 01:01 → 09:01 +08, also 07-16)
    assert!(d16.contains("全文搜索"));
    // cursor fixture (embedded UTC+8 07-15 14:18 → 07-15)
    let d15 = std::fs::read_to_string(dir.join("2026-07-15.md")).unwrap();
    assert!(d15.contains("审查"));
    assert!(
        !d15.contains("跨日边界的提问"),
        "cross-day codex messages must not land in 07-15"
    );
    // gemini agy (07-14)
    let d14 = std::fs::read_to_string(dir.join("2026-07-14.md")).unwrap();
    assert!(d14.contains("量子蓝图"));
    // index totals
    let index = std::fs::read_to_string(dir.join("index.md")).unwrap();
    assert!(index.contains("[2026-07-16](2026-07-16.md)"));
    assert!(index.contains("合计"));
}

#[test]
fn run_schedules_background_and_worker_writes_report() {
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    let (_t, script) = fake_backend_script();
    base_cmd()
        .args([
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
        .stdout(predicate::str::contains("Summarization started in background"));

    let dir = out.path().join("2026-07-12_2026-07-18");
    assert!(dir.join("report.pid").exists());
    assert!(dir.join("report.log").exists());

    let report = wait_for_file(&dir.join("report.md"), Duration::from_secs(10));
    assert!(report.contains("假周报"));
    // prompt via stdin in FilesManifest mode (lists filenames, does not inline bodies)
    let captured = wait_for_file(&dir.join("stdin-capture.txt"), Duration::from_secs(5));
    assert!(captured.contains("2026-07-14.md"));
    assert!(captured.contains("输入文件"));
    assert!(!captured.contains("跨日边界的提问"), "manifest mode must not inline message bodies");
    assert!(captured.contains("有记录 3 天"), "stats placeholder should be substituted: {captured}");
}

#[test]
fn report_worker_writes_report_md_synchronously() {
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
    base_cmd().args(["collect"]).args(range_args).assert().success();
    let (_t, script) = fake_backend_script();
    base_cmd()
        .args(["report", "--worker"])
        .args(range_args)
        .args(["--cmd", script.to_str().unwrap()])
        .assert()
        .success();
    let report = std::fs::read_to_string(out.path().join("2026-07-12_2026-07-18/report.md")).unwrap();
    assert!(report.contains("假周报"));
}

#[test]
fn run_api_backend_without_key_exits_1() {
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    base_cmd()
        .args([
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
fn report_on_uncollected_dir_fails_cleanly() {
    let home = fake_home();
    let out = tempfile::tempdir().unwrap();
    let (_t, script) = fake_backend_script();
    base_cmd()
        .args([
            "report",
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
        .failure()
        .stderr(predicate::str::contains("run collect first"));
}

#[test]
fn report_subcommand_schedules_background() {
    // collect first, then report (no re-collect)
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
    base_cmd().args(["collect"]).args(range_args).assert().success();
    let (_t, script) = fake_backend_script();
    base_cmd()
        .args(["report"])
        .args(range_args)
        .args(["--cmd", script.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Summarization started in background"));
    let dir: PathBuf = out.path().join("2026-07-12_2026-07-18");
    assert!(dir.join("report.pid").exists());
    let report = wait_for_file(&dir.join("report.md"), Duration::from_secs(10));
    assert!(report.contains("假周报"));
}

#[test]
fn mail_sends_existing_report_with_fake_mutt() {
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

    base_cmd()
        .env("PATH", &path)
        .args([
            "mail",
            report.to_str().unwrap(),
            "--mail-to",
            "a@example.com",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Emailed"));
}

#[test]
fn mail_missing_report_fails() {
    base_cmd()
        .args([
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
fn sources_lists_detected_and_missing() {
    let home = tempfile::tempdir().unwrap();
    write(
        &home.path().join(".claude/projects/p/s.jsonl"),
        CLAUDE_SAMPLE,
    );
    base_cmd()
        .args(["sources", "--home", home.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ Claude Code"))
        .stdout(predicate::str::contains("✗ Codex"))
        .stdout(predicate::str::contains("✗ Cursor"))
        .stdout(predicate::str::contains("✗ Gemini"));
}
