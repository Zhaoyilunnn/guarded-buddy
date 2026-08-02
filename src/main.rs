//! Thin entry point: CLI → config → dispatch → exit code.

use std::path::Path;

use ai_weekly_report::app;
use ai_weekly_report::cli::{self, Cli, Command};
use ai_weekly_report::config::Config;
use ai_weekly_report::job::{self, mail_subject, mail_subject_for_report_path};
use ai_weekly_report::mail::{self, mutt_available, mutt_missing_hint};
use ai_weekly_report::sources::default_sources;
use anyhow::Context;
use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default_home = dirs::home_dir().context("unable to determine $HOME directory")?;
    let config = Config::load(&Config::default_path(&default_home))?;
    let today = chrono::Local::now().date_naive();

    match cli.command {
        Command::Sources(args) => {
            let home = args.home.as_deref().unwrap_or(&default_home);
            for source in default_sources(home) {
                let mark = if source.detect() { "✓" } else { "✗" };
                println!(
                    "{mark} {:<12} {}",
                    source.kind().display_name(),
                    source.root().display()
                );
            }
        }
        Command::Collect(args) => {
            let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
            let outcome = app::collect_into(&common)?;
            print_collect_result(&outcome);
        }
        Command::Report(args) => {
            let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
            let backend = cli::resolve_backend(&args.backend, &config)?;
            let dir = common.out_dir.join(common.range.dir_name());
            if backend.worker {
                run_worker(&backend, &dir, &common.range)?;
            } else {
                schedule_report(&common, &backend, &dir)?;
            }
        }
        Command::Run(args) => {
            let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
            let backend = cli::resolve_backend(&args.backend, &config)?;
            if backend.worker {
                // Worker is always invoked as `report --worker`; keep a safe path.
                let dir = common.out_dir.join(common.range.dir_name());
                run_worker(&backend, &dir, &common.range)?;
            } else {
                let outcome = app::collect_into(&common)?;
                print_collect_result(&outcome);
                schedule_report(&common, &backend, &outcome.dir)?;
            }
        }
        Command::Mail(args) => {
            send_mail_only(&args.report, &cli::resolve_mail_to(&args.mail_to, &config))?;
        }
    }
    Ok(())
}

fn print_collect_result(outcome: &ai_weekly_report::collect::CollectOutcome) {
    for warning in &outcome.warnings {
        eprintln!("warning: {warning}");
    }
    println!(
        "Collected records for {} days → {}",
        outcome.buckets.len(),
        outcome.dir.display()
    );
}

fn schedule_report(
    common: &cli::EffectiveCommon,
    backend: &cli::EffectiveBackend,
    range_dir: &Path,
) -> anyhow::Result<()> {
    // Fail fast if there is nothing to summarize (same checks as load_collected_dir).
    let _ = ai_weekly_report::report::load_collected_dir(range_dir)?;

    // Fail fast on missing API key before detaching the worker.
    if matches!(backend.kind, cli::BackendKind::Api) {
        std::env::var(&backend.api_key_env).map_err(|_| {
            anyhow::anyhow!(
                "environment variable {} is not set (required for --backend api)",
                backend.api_key_env
            )
        })?;
    }

    if !backend.mail_to.is_empty() && !mutt_available() {
        eprintln!("{}", mutt_missing_hint());
    }

    let job = job::spawn_report_worker(range_dir, common, backend)?;
    println!(
        "Summarization started in background (pid {}) → {} (log: {})",
        job.pid,
        job.report_path.display(),
        job.log_path.display()
    );
    Ok(())
}

fn run_worker(
    backend: &cli::EffectiveBackend,
    range_dir: &Path,
    range: &ai_weekly_report::domain::DateRange,
) -> anyhow::Result<()> {
    eprintln!(
        "worker: summarizing {} …",
        range_dir.display()
    );
    let report = app::summarize_range_dir(backend, range_dir, range)?;
    let path = range_dir.join("report.md");
    std::fs::write(&path, &report)?;
    eprintln!("worker: wrote {}", path.display());

    if backend.mail_to.is_empty() {
        return Ok(());
    }
    if !mutt_available() {
        eprintln!("worker: {}", mutt_missing_hint());
        return Ok(());
    }
    let subject = mail_subject(range);
    match mail::send_report_with_mutt(&backend.mail_to, &subject, &path) {
        Ok(()) => eprintln!(
            "worker: emailed report to {}",
            backend.mail_to.join(", ")
        ),
        Err(e) => eprintln!("worker: mail failed (report.md kept): {e}"),
    }
    Ok(())
}

/// Retry sending an already-written report.md (foreground; fails hard on mail errors).
fn send_mail_only(report_path: &Path, mail_to: &[String]) -> anyhow::Result<()> {
    if !report_path.is_file() {
        anyhow::bail!(
            "report file not found: {} (generate it with report/run first)",
            report_path.display()
        );
    }
    if mail_to.is_empty() {
        anyhow::bail!("no recipients: set --mail-to or mail_to in config.toml");
    }
    if !mutt_available() {
        anyhow::bail!("{}", mutt_missing_hint());
    }
    let subject = mail_subject_for_report_path(report_path);
    mail::send_report_with_mutt(mail_to, &subject, report_path)?;
    println!(
        "Emailed {} to {}",
        report_path.display(),
        mail_to.join(", ")
    );
    Ok(())
}
