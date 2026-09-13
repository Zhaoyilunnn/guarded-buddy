//! External CLI backend: preset table (codex/claude/agy/gemini) + custom commands,
//! stdin preferred (avoids argv length/quoting issues), wait-timeout prevents hangs.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wait_timeout::ChildExt;

use super::{Prompt, ReportError, Summarizer};
use crate::secrets::redact_secrets;

/// How to invoke one external CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSpec {
    pub program: String,
    pub args: Vec<String>,
    /// true: prompt via stdin; false: prompt appended as the last argument.
    pub stdin_prompt: bool,
}

/// Built-in presets: all non-interactive (plan / wr).
pub fn preset(name: &str) -> Option<CliSpec> {
    let (program, args, stdin_prompt) = match name {
        // --skip-git-repo-check: cwd is out/<range>/ (not a git trust root).
        "codex" => ("codex", vec!["exec", "--skip-git-repo-check", "-"], true),
        "claude" => ("claude", vec!["-p"], true),
        "agy" => ("agy", vec!["-p"], false),
        "gemini" => ("gemini", vec!["-p"], false),
        _ => return None,
    };
    Some(CliSpec {
        program: program.to_string(),
        args: args.into_iter().map(str::to_string).collect(),
        stdin_prompt,
    })
}

/// Parse `--cmd` custom command: split on whitespace (v1 does not handle quotes; wrap complex commands in a script),
/// prompt always goes via stdin.
pub fn parse_custom_cmd(template: &str) -> Result<CliSpec, ReportError> {
    let mut parts = template.split_whitespace();
    let Some(program) = parts.next() else {
        return Err(ReportError::ApiParse(
            "custom command is empty (--cmd requires an executable program)".to_string(),
        ));
    };
    Ok(CliSpec {
        program: program.to_string(),
        args: parts.map(str::to_string).collect(),
        stdin_prompt: true,
    })
}

pub struct CliSummarizer {
    spec: CliSpec,
    timeout: Duration,
    log: Option<Arc<Mutex<File>>>,
}

impl CliSummarizer {
    pub fn new(spec: CliSpec, timeout: Duration) -> Self {
        Self {
            spec,
            timeout,
            log: None,
        }
    }

    /// Enable a separate, exclusively created log for this invocation.
    pub fn with_log(mut self, dir: &Path) -> Result<Self, ReportError> {
        let path = dir.canonicalize()?.join(format!(
            "agent-{}-{}.log",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
            std::process::id()
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        eprintln!("worker: agent log: {}", path.display());
        self.log = Some(Arc::new(Mutex::new(file)));
        Ok(self)
    }
}

fn log_output(log: &Option<Arc<Mutex<File>>>, stream: &str, text: &str) {
    if let Some(log) = log {
        let mut file = log.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = writeln!(
            file,
            "[{}] [{stream}] {text}",
            chrono::Utc::now().to_rfc3339()
        )
        .and_then(|_| file.flush())
        {
            eprintln!("worker: agent log write failed: {error}");
        }
    }
}

/// Drain both pipes concurrently, preserving UTF-8 characters across read boundaries.
fn read_output(mut pipe: impl Read, log: Option<Arc<Mutex<File>>>, stream: &str) -> String {
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                output.extend_from_slice(&buffer[..n]);
                pending.extend_from_slice(&buffer[..n]);
                loop {
                    match std::str::from_utf8(&pending) {
                        Ok(text) => {
                            log_output(&log, stream, text);
                            pending.clear();
                            break;
                        }
                        Err(error) => {
                            let valid = error.valid_up_to();
                            if valid > 0 {
                                log_output(
                                    &log,
                                    stream,
                                    std::str::from_utf8(&pending[..valid]).unwrap(),
                                );
                                pending.drain(..valid);
                            }
                            if let Some(n) = error.error_len() {
                                log_output(&log, stream, "\u{fffd}");
                                pending.drain(..n);
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                log_output(&log, "event", &format!("{stream} read failed: {error}"));
                break;
            }
        }
    }
    if !pending.is_empty() {
        log_output(&log, stream, &String::from_utf8_lossy(&pending));
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// Build the detail string for a failed CLI run.
/// Claude Code (and some other CLIs) print API/auth errors to stdout while leaving stderr empty.
fn cli_failure_detail(stdout: &str, stderr: &str) -> String {
    match (stderr.trim().is_empty(), stdout.trim().is_empty()) {
        (false, false) => format!(
            "{}\n--- stdout ---\n{}",
            stderr.trim_end(),
            stdout.trim_end()
        ),
        (false, true) => stderr.to_string(),
        (true, false) => stdout.to_string(),
        (true, true) => "(no stdout/stderr captured)".to_string(),
    }
}

impl Summarizer for CliSummarizer {
    fn summarize(&self, prompt: &Prompt) -> Result<String, ReportError> {
        let started = std::time::Instant::now();
        log_output(
            &self.log,
            "event",
            &format!(
                "starting {}; timeout={}s",
                self.spec.program,
                self.timeout.as_secs()
            ),
        );
        let mut cmd = Command::new(&self.spec.program);
        cmd.args(&self.spec.args)
            .current_dir(&prompt.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !self.spec.stdin_prompt {
            cmd.arg(&prompt.text);
        }
        let mut child = cmd.spawn().map_err(|source| ReportError::Spawn {
            cmd: self.spec.program.clone(),
            source,
        })?;
        let out_log = self.log.clone();
        let err_log = self.log.clone();
        let mut out_thread = child
            .stdout
            .take()
            .map(|pipe| std::thread::spawn(move || read_output(pipe, out_log, "stdout")));
        let mut err_thread = child
            .stderr
            .take()
            .map(|pipe| std::thread::spawn(move || read_output(pipe, err_log, "stderr")));
        if self.spec.stdin_prompt
            && let Some(mut stdin) = child.stdin.take()
        {
            // child may exit early and cause write to fail; ignore (exit code/timeout will report)
            let _ = stdin.write_all(prompt.text.as_bytes());
            let _ = stdin.flush();
            drop(stdin); // close pipe so the child sees EOF
        }

        match child.wait_timeout(self.timeout) {
            Ok(Some(status)) => {
                log_output(
                    &self.log,
                    "event",
                    &format!("exit={status}; elapsed={:?}", started.elapsed()),
                );
                let stdout_s = out_thread
                    .take()
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                let stderr_s = err_thread
                    .take()
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                if status.success() {
                    Ok(stdout_s)
                } else {
                    Err(ReportError::CliFailed {
                        cmd: self.spec.program.clone(),
                        code: status.code(),
                        stderr: redact_secrets(&cli_failure_detail(&stdout_s, &stderr_s)),
                    })
                }
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                log_output(
                    &self.log,
                    "event",
                    &format!(
                        "timed out; termination requested; elapsed={:?}",
                        started.elapsed()
                    ),
                );
                Err(ReportError::CliTimeout {
                    cmd: self.spec.program.clone(),
                    secs: self.timeout.as_secs(),
                })
            }
            Err(source) => Err(ReportError::Spawn {
                cmd: self.spec.program.clone(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CliSummarizer, cli_failure_detail, parse_custom_cmd, preset};
    use crate::report::{Prompt, ReportError, Summarizer};
    use std::path::PathBuf;
    use std::time::Duration;

    /// Write a temporary executable fake script and return its path.
    fn fake_script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fake-cli.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (tmp, path)
    }

    fn prompt(text: &str) -> Prompt {
        Prompt {
            text: text.to_string(),
            dir: std::env::current_dir().unwrap(),
        }
    }

    #[test]
    fn live_log_survives_timeout_without_newlines() {
        let (_script, path) = fake_script("printf progress; printf diagnostic >&2; sleep 3");
        let logs = tempfile::tempdir().unwrap();
        let summarizer = CliSummarizer::new(
            parse_custom_cmd(path.to_str().unwrap()).unwrap(),
            Duration::from_millis(800),
        )
        .with_log(logs.path())
        .unwrap();
        let handle = std::thread::spawn(move || summarizer.summarize(&prompt("")));
        let log = std::fs::read_dir(logs.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let start = std::time::Instant::now();
        loop {
            let text = std::fs::read_to_string(&log).unwrap();
            if text.contains("progress") && text.contains("diagnostic") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_millis(700),
                "output must be logged before timeout"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!handle.is_finished());
        assert!(matches!(
            handle.join().unwrap(),
            Err(ReportError::CliTimeout { .. })
        ));
        assert!(std::fs::read_to_string(log).unwrap().contains("timed out"));
    }

    #[test]
    fn logs_do_not_change_final_stdout() {
        let (_script, path) = fake_script("printf 'report'; printf 'diagnostic' >&2");
        let logs = tempfile::tempdir().unwrap();
        let result = CliSummarizer::new(
            parse_custom_cmd(path.to_str().unwrap()).unwrap(),
            Duration::from_secs(5),
        )
        .with_log(logs.path())
        .unwrap()
        .summarize(&prompt(""))
        .unwrap();
        assert_eq!(result, "report");
    }

    // ---------- preset table ----------

    #[test]
    fn preset_table_matches_known_clis() {
        let codex = preset("codex").unwrap();
        assert_eq!(codex.program, "codex");
        assert_eq!(codex.args, vec!["exec", "--skip-git-repo-check", "-"]);
        assert!(codex.stdin_prompt);

        let claude = preset("claude").unwrap();
        assert_eq!(claude.program, "claude");
        assert_eq!(claude.args, vec!["-p"]);
        assert!(claude.stdin_prompt);

        let agy = preset("agy").unwrap();
        assert_eq!(agy.program, "agy");
        assert_eq!(agy.args, vec!["-p"]);
        assert!(!agy.stdin_prompt);

        let gemini = preset("gemini").unwrap();
        assert_eq!(gemini.program, "gemini");
        assert_eq!(gemini.args, vec!["-p"]);
        assert!(!gemini.stdin_prompt);
    }

    #[test]
    fn unknown_preset_returns_none() {
        assert!(preset("mystery").is_none());
    }

    #[test]
    fn custom_cmd_split_on_whitespace_stdin_mode() {
        let spec = parse_custom_cmd("my-wrapper --fast --model x").unwrap();
        assert_eq!(spec.program, "my-wrapper");
        assert_eq!(spec.args, vec!["--fast", "--model", "x"]);
        assert!(spec.stdin_prompt);
        assert!(parse_custom_cmd("   ").is_err());
    }

    // ---------- execution ----------

    #[test]
    fn stdin_backend_feeds_prompt_and_captures_stdout() {
        let (_t, path) = fake_script("cat");
        let spec = parse_custom_cmd(path.to_str().unwrap()).unwrap();
        let summarizer = CliSummarizer::new(spec, Duration::from_secs(5));
        let out = summarizer.summarize(&prompt("周报 prompt 内容")).unwrap();
        assert_eq!(out.trim(), "周报 prompt 内容");
    }

    #[test]
    fn stdin_backend_runs_in_prompt_dir() {
        let (_t, path) = fake_script("pwd");
        let spec = parse_custom_cmd(path.to_str().unwrap()).unwrap();
        let summarizer = CliSummarizer::new(spec, Duration::from_secs(5));
        let tmp = tempfile::tempdir().unwrap();
        let p = Prompt {
            text: "x".to_string(),
            dir: tmp.path().to_path_buf(),
        };
        let out = summarizer.summarize(&p).unwrap();
        // tempdir may have /private prefix differences on macOS/Linux; compare canonical paths
        let got = PathBuf::from(out.trim());
        let got = got.canonicalize().unwrap_or(got);
        assert_eq!(got, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn arg_backend_passes_prompt_as_last_argument() {
        // for loop takes the last positional argument
        let (_t, path) = fake_script("for last; do :; done; echo \"$last\"");
        let mut spec = parse_custom_cmd(path.to_str().unwrap()).unwrap();
        spec.stdin_prompt = false;
        let summarizer = CliSummarizer::new(spec, Duration::from_secs(5));
        let out = summarizer.summarize(&prompt("作为参数的 prompt")).unwrap();
        assert_eq!(out.trim(), "作为参数的 prompt");
    }

    #[test]
    fn nonzero_exit_reports_stderr_and_code() {
        let (_t, path) = fake_script("echo '出错了' >&2; exit 3");
        let spec = parse_custom_cmd(path.to_str().unwrap()).unwrap();
        let summarizer = CliSummarizer::new(spec, Duration::from_secs(5));
        let err = summarizer.summarize(&prompt("x")).unwrap_err();
        match err {
            ReportError::CliFailed { code, stderr, .. } => {
                assert_eq!(code, Some(3));
                assert!(stderr.contains("出错了"));
            }
            other => panic!("expected CliFailed, got {other:?}"),
        }
    }

    #[test]
    fn nonzero_exit_surfaces_stdout_when_stderr_empty() {
        // Claude Code prints API/auth errors to stdout and leaves stderr empty.
        let (_t, path) = fake_script("echo 'API Error: Request rejected (429)'; exit 1");
        let spec = parse_custom_cmd(path.to_str().unwrap()).unwrap();
        let summarizer = CliSummarizer::new(spec, Duration::from_secs(5));
        let err = summarizer.summarize(&prompt("x")).unwrap_err();
        match err {
            ReportError::CliFailed { code, stderr, .. } => {
                assert_eq!(code, Some(1));
                assert!(
                    stderr.contains("API Error: Request rejected (429)"),
                    "should surface stdout: {stderr}"
                );
            }
            other => panic!("expected CliFailed, got {other:?}"),
        }
    }

    #[test]
    fn cli_failure_detail_combines_both_streams() {
        assert_eq!(cli_failure_detail("out-msg", ""), "out-msg");
        assert_eq!(cli_failure_detail("", "err-msg"), "err-msg");
        let both = cli_failure_detail("out-msg", "err-msg");
        assert!(both.contains("err-msg"));
        assert!(both.contains("out-msg"));
        assert!(both.contains("--- stdout ---"));
    }

    #[test]
    fn missing_binary_reports_spawn_error() {
        let spec = parse_custom_cmd("/nonexistent/definitely-missing-cli").unwrap();
        let summarizer = CliSummarizer::new(spec, Duration::from_secs(5));
        let err = summarizer.summarize(&prompt("x")).unwrap_err();
        assert!(matches!(err, ReportError::Spawn { .. }));
    }

    #[test]
    fn timeout_kills_long_running_cli() {
        let (_t, path) = fake_script("sleep 30");
        let spec = parse_custom_cmd(path.to_str().unwrap()).unwrap();
        let summarizer = CliSummarizer::new(spec, Duration::from_millis(200));
        let start = std::time::Instant::now();
        let err = summarizer.summarize(&prompt("x")).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should be killed promptly"
        );
        assert!(matches!(err, ReportError::CliTimeout { .. }));
    }
}
