//! External CLI backend: preset table (codex/claude/agy/gemini) + custom commands,
//! stdin preferred (avoids argv length/quoting issues), wait-timeout prevents hangs.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
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

/// Built-in presets: all non-interactive.
pub fn preset(name: &str) -> Option<CliSpec> {
    let (program, args, stdin_prompt) = match name {
        "codex" => ("codex", vec!["exec", "-"], true),
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
}

impl CliSummarizer {
    pub fn new(spec: CliSpec, timeout: Duration) -> Self {
        Self { spec, timeout }
    }
}

/// Build the detail string for a failed CLI run.
/// Claude Code (and some other CLIs) print API/auth errors to stdout while leaving stderr empty.
fn cli_failure_detail(stdout: &str, stderr: &str) -> String {
    match (stderr.trim().is_empty(), stdout.trim().is_empty()) {
        (false, false) => format!("{}\n--- stdout ---\n{}", stderr.trim_end(), stdout.trim_end()),
        (false, true) => stderr.to_string(),
        (true, false) => stdout.to_string(),
        (true, true) => "(no stdout/stderr captured)".to_string(),
    }
}

impl Summarizer for CliSummarizer {
    fn summarize(&self, prompt: &Prompt) -> Result<String, ReportError> {
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
        if self.spec.stdin_prompt
            && let Some(mut stdin) = child.stdin.take()
        {
            // child may exit early and cause write to fail; ignore (exit code/timeout will report)
            let _ = stdin.write_all(prompt.text.as_bytes());
            let _ = stdin.flush();
            drop(stdin); // close pipe so the child sees EOF
        }
        // spawn reader threads for stdout/stderr to avoid pipe buffer deadlock
        let mut out_thread = child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = pipe.read_to_string(&mut buf);
                buf
            })
        });
        let mut err_thread = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = pipe.read_to_string(&mut buf);
                buf
            })
        });

        match child.wait_timeout(self.timeout) {
            Ok(Some(status)) => {
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

    // ---------- preset table ----------

    #[test]
    fn preset_table_matches_known_clis() {
        let codex = preset("codex").unwrap();
        assert_eq!(codex.program, "codex");
        assert_eq!(codex.args, vec!["exec", "-"]);
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
        assert_eq!(
            cli_failure_detail("out-msg", ""),
            "out-msg"
        );
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
        assert!(start.elapsed() < Duration::from_secs(5), "should be killed promptly");
        assert!(matches!(err, ReportError::CliTimeout { .. }));
    }
}
