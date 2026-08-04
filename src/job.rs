//! Detached background workers for wr report and signoff.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::NaiveDate;

use crate::cli::{BackendKind, EffectiveBackend, EffectiveCommon};
use crate::domain::DateRange;
use crate::signoff::SignoffSettings;

#[derive(Debug)]
pub struct SpawnedJob {
    pub pid: u32,
    pub report_path: PathBuf,
    pub log_path: PathBuf,
    pub pid_path: PathBuf,
}

fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
}

fn append_backend_args(cmd: &mut Command, backend: &EffectiveBackend) {
    cmd.arg("--backend")
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
        .arg(backend.timeout_secs.to_string());
    if let Some(c) = &backend.cli_cmd {
        cmd.arg("--cmd").arg(c);
    }
    if let Some(t) = &backend.template {
        cmd.arg("--template").arg(t);
    }
    if !backend.mail_to.is_empty() {
        cmd.arg("--mail-to").arg(backend.mail_to.join(","));
    }
}

/// Spawn `buddy wr report --worker ...`.
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
    cmd.arg("wr")
        .arg("report")
        .arg("--worker")
        .arg("--from")
        .arg(common.range.start.to_string())
        .arg("--to")
        .arg(common.range.end.to_string())
        .arg("--out")
        .arg(&common.out_dir)
        .arg("--home")
        .arg(&common.home);
    append_backend_args(&mut cmd, backend);
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
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    detach(&mut cmd);

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

/// Spawn `buddy signoff run --worker ...`.
pub fn spawn_signoff_worker(
    work_dir: &Path,
    settings: &SignoffSettings,
    backend: &EffectiveBackend,
) -> std::io::Result<SpawnedJob> {
    std::fs::create_dir_all(work_dir)?;
    let log_path = work_dir.join("signoff.log");
    let pid_path = work_dir.join("signoff.pid");
    let report_path = work_dir.join("signoff.md");

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("signoff")
        .arg("run")
        .arg("--worker")
        .arg("--out")
        .arg(&settings.out_dir)
        .arg("--home")
        .arg(&settings.home)
        .arg("--window-hours")
        .arg(settings.window_hours.to_string());
    if settings.dry_run {
        cmd.arg("--dry-run");
    }
    append_backend_args(&mut cmd, backend);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    detach(&mut cmd);

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

pub fn mail_subject(range: &DateRange) -> String {
    format!("AI weekly report {} ~ {}", range.start, range.end)
}

pub fn mail_subject_for_report_path(report_path: &Path) -> String {
    let parent_name = report_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if let Some((start, end)) = parent_name.split_once('_')
        && NaiveDate::parse_from_str(start, "%Y-%m-%d").is_ok()
        && NaiveDate::parse_from_str(end, "%Y-%m-%d").is_ok()
    {
        return format!("AI weekly report {start} ~ {end}");
    }
    if parent_name.chars().filter(|c| *c == '-').count() == 2
        && NaiveDate::parse_from_str(parent_name, "%Y-%m-%d").is_ok()
    {
        return format!("buddy signoff {parent_name}");
    }
    "buddy mail".to_string()
}

#[cfg(test)]
mod tests {
    use super::mail_subject_for_report_path;
    use std::path::Path;

    #[test]
    fn subject_from_wr_range_dir() {
        assert_eq!(
            mail_subject_for_report_path(Path::new("out/2026-07-12_2026-07-18/report.md")),
            "AI weekly report 2026-07-12 ~ 2026-07-18"
        );
    }

    #[test]
    fn subject_from_signoff_day_dir() {
        assert_eq!(
            mail_subject_for_report_path(Path::new("out/signoff/2026-08-04/signoff.md")),
            "buddy signoff 2026-08-04"
        );
    }
}
