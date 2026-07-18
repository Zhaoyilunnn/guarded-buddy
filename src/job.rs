//! Detached background worker for async report generation.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::{BackendKind, EffectiveBackend, EffectiveCommon};
use crate::domain::DateRange;

#[derive(Debug)]
pub struct SpawnedJob {
    pub pid: u32,
    pub report_path: PathBuf,
    pub log_path: PathBuf,
    pub pid_path: PathBuf,
}

/// Spawn a detached copy of this binary to run `report --worker ...`.
///
/// Stdout/stderr of the worker go to `range_dir/report.log`. PID is written to
/// `range_dir/report.pid`.
pub fn spawn_report_worker(
    range_dir: &Path,
    common: &EffectiveCommon,
    backend: &EffectiveBackend,
) -> std::io::Result<SpawnedJob> {
    std::fs::create_dir_all(range_dir)?;
    let log_path = range_dir.join("report.log");
    let pid_path = range_dir.join("report.pid");
    let report_path = range_dir.join("report.md");

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("report")
        .arg("--worker")
        .arg("--from")
        .arg(common.range.start.to_string())
        .arg("--to")
        .arg(common.range.end.to_string())
        .arg("--out")
        .arg(&common.out_dir)
        .arg("--home")
        .arg(&common.home)
        .arg("--backend")
        .arg(match backend.kind {
            BackendKind::Cli => "cli",
            BackendKind::Api => "api",
        })
        .arg("--cli-name")
        .arg(&backend.cli_name)
        .arg("--base-url")
        .arg(&backend.api_base_url)
        .arg("--api-key-env")
        .arg(&backend.api_key_env)
        .arg("--model")
        .arg(&backend.api_model)
        .arg("--timeout-secs")
        .arg(backend.timeout_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));

    if let Some(cmd_template) = &backend.cli_cmd {
        cmd.arg("--cmd").arg(cmd_template);
    }
    if let Some(template) = &backend.template {
        cmd.arg("--template").arg(template);
    }
    if !backend.mail_to.is_empty() {
        cmd.arg("--mail-to").arg(backend.mail_to.join(","));
    }
    if let Some(agents) = &common.agents {
        let list = agents
            .iter()
            .map(|a| a.slug())
            .collect::<Vec<_>>()
            .join(",");
        cmd.arg("--agents").arg(list);
    }
    if common.include_prompt_history {
        cmd.arg("--include-prompt-history");
    }

    // Detach from the controlling terminal so the worker survives shell exit.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // setsid() fails only if already a leader; ignore that case.
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    let pid = child.id();
    let _ = std::fs::write(&pid_path, format!("{pid}\n"));

    Ok(SpawnedJob {
        pid,
        report_path,
        log_path,
        pid_path,
    })
}

/// Build the email subject line for a date range.
pub fn mail_subject(range: &DateRange) -> String {
    format!("AI weekly report {} ~ {}", range.start, range.end)
}
