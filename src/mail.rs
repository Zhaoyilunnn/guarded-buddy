//! Email finished weekly reports via `mutt`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Whether `mutt` is on PATH and runnable.
pub fn mutt_available() -> bool {
    Command::new("mutt")
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
    if to.is_empty() {
        return Err(MailError::NoRecipients);
    }
    if !mutt_available() {
        return Err(MailError::MuttMissing);
    }
    let body = std::fs::read_to_string(report_path).map_err(|source| MailError::Read {
        path: report_path.to_path_buf(),
        source,
    })?;

    let mut child = Command::new("mutt")
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
        stdin
            .write_all(body.as_bytes())
            .map_err(MailError::Spawn)?;
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
    use super::{MailError, mutt_available, mutt_missing_hint, send_report_with_mutt};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    // Serialize PATH mutations across mail unit tests.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // Empty PATH → mutt not found.
        let old = std::env::var_os("PATH");
        // SAFETY: single-threaded under PATH_LOCK for this test process section.
        unsafe { std::env::set_var("PATH", tmp.path()) };
        assert!(!mutt_available());
        let to = vec!["nobody@example.com".to_string()];
        let err = send_report_with_mutt(&to, "subj", Path::new("/tmp/x")).unwrap_err();
        assert!(matches!(err, MailError::MuttMissing));
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }

    #[test]
    fn send_with_fake_mutt_succeeds() {
        let _guard = PATH_LOCK.lock().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let mutt = bin.path().join("mutt");
        std::fs::write(
            &mutt,
            "#!/bin/sh\n# fake mutt: accept -v and -s ...\nif [ \"$1\" = \"-v\" ]; then exit 0; fi\ncat > /dev/null\nexit 0\n",
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

        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", bin.path()) };
        assert!(mutt_available());
        send_report_with_mutt(
            &["a@example.com".to_string()],
            "AI weekly report",
            &report,
        )
        .unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }

    #[test]
    fn send_with_missing_file_when_mutt_present() {
        let _guard = PATH_LOCK.lock().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let mutt = bin.path().join("mutt");
        std::fs::write(&mutt, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mutt, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", bin.path()) };
        let to = vec!["nobody@example.com".to_string()];
        let missing = PathBuf::from("/nonexistent/aiw-report-missing.md");
        let err = send_report_with_mutt(&to, "subj", &missing).unwrap_err();
        assert!(matches!(err, MailError::Read { .. }));
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}
