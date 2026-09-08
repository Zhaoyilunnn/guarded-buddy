//! Email finished weekly reports via `mutt`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Whether `mutt` is on PATH and runnable.
pub fn mutt_available() -> bool {
    mutt_available_with(Path::new("mutt"))
}

fn mutt_available_with(program: &Path) -> bool {
    Command::new(program)
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One-line tip when mail is configured but mutt is missing.
pub fn mutt_missing_hint() -> &'static str {
    "hint: install and configure `mutt` to email finished reports (mail_to is set)"
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("mutt is not installed or not on PATH")]
    MuttMissing,
    #[error("no mail recipients")]
    NoRecipients,
    #[error("cannot read report file {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to spawn mutt: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("mutt exited with code {code:?}: {stderr}")]
    Failed { code: Option<i32>, stderr: String },
}

/// Send `report_path` as the email body to `to` with `subject` using mutt.
pub fn send_report_with_mutt(
    to: &[String],
    subject: &str,
    report_path: &Path,
) -> Result<(), MailError> {
    send_report_with_program(Path::new("mutt"), to, subject, report_path)
}

fn send_report_with_program(
    program: &Path,
    to: &[String],
    subject: &str,
    report_path: &Path,
) -> Result<(), MailError> {
    if to.is_empty() {
        return Err(MailError::NoRecipients);
    }
    if !mutt_available_with(program) {
        return Err(MailError::MuttMissing);
    }
    let body = std::fs::read_to_string(report_path).map_err(|source| MailError::Read {
        path: report_path.to_path_buf(),
        source,
    })?;

    let mut child = Command::new(program)
        .arg("-s")
        .arg(subject)
        .arg("--")
        .args(to)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(MailError::Spawn)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes()).map_err(MailError::Spawn)?;
    }

    let output = child.wait_with_output().map_err(MailError::Spawn)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MailError::Failed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MailError, mutt_available_with, mutt_missing_hint, send_report_with_mutt,
        send_report_with_program,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn send_with_empty_recipients_errors() {
        let err = send_report_with_mutt(&[], "subj", Path::new("/tmp/x")).unwrap_err();
        assert!(matches!(err, MailError::NoRecipients));
    }

    #[test]
    fn hint_mentions_mutt() {
        assert!(mutt_missing_hint().contains("mutt"));
    }

    #[test]
    fn send_with_missing_mutt_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_mutt = tmp.path().join("mutt");
        assert!(!mutt_available_with(&missing_mutt));
        let to = vec!["nobody@example.com".to_string()];
        let err =
            send_report_with_program(&missing_mutt, &to, "subj", Path::new("/tmp/x")).unwrap_err();
        assert!(matches!(err, MailError::MuttMissing));
    }

    #[test]
    fn send_with_fake_mutt_succeeds() {
        let bin = tempfile::tempdir().unwrap();
        let mutt = bin.path().join("mutt");
        std::fs::write(
            &mutt,
            "#!/bin/sh\n# fake mutt: accept -v and consume the mail body.\nif [ \"$1\" = \"-v\" ]; then exit 0; fi\nwhile IFS= read -r _; do :; done\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mutt, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let report_dir = tempfile::tempdir().unwrap();
        let report = report_dir.path().join("report.md");
        std::fs::write(&report, "# hello report\n").unwrap();

        assert!(mutt_available_with(&mutt));
        send_report_with_program(
            &mutt,
            &["a@example.com".to_string()],
            "AI weekly report",
            &report,
        )
        .unwrap();
    }

    #[test]
    fn send_with_missing_file_when_mutt_present() {
        let bin = tempfile::tempdir().unwrap();
        let mutt = bin.path().join("mutt");
        std::fs::write(&mutt, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mutt, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let to = vec!["nobody@example.com".to_string()];
        let missing = PathBuf::from("/nonexistent/aiw-report-missing.md");
        let err = send_report_with_program(&mutt, &to, "subj", &missing).unwrap_err();
        assert!(matches!(err, MailError::Read { .. }));
    }
}
