//! Thin entry: CLI → config → skill dispatch.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use buddy::app;
use buddy::cli::{
    self, Cli, Command, ShellKind, SignoffCommand, WrCommand, resolve_signoff_settings,
};
use buddy::config::Config;
use buddy::job::{self, mail_subject, mail_subject_for_report_path};
use buddy::mail::{self, mutt_available, mutt_missing_hint};
use buddy::signoff::{self, mail_subject_for_day};
use buddy::sources::default_sources;
use chrono::Local;
use clap::{CommandFactory, Parser};
use clap_complete::generate;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default_home = dirs::home_dir().context("unable to determine $HOME directory")?;
    let config = Config::load(&Config::default_path(&default_home))?;
    let today = Local::now().date_naive();

    match cli.command {
        Command::Sync(args) => {
            let settings = cli::resolve_sync(&args, &config, &default_home)?;
            run_sync(&settings)?;
        }
        Command::Completions(args) => {
            let mut cmd = Cli::command();
            let shell = match args.shell {
                ShellKind::Bash => clap_complete::Shell::Bash,
                ShellKind::Zsh => clap_complete::Shell::Zsh,
                ShellKind::Fish => clap_complete::Shell::Fish,
            };
            generate(shell, &mut cmd, "buddy", &mut std::io::stdout());
        }
        Command::Wr(wr) => match wr.command {
            WrCommand::Sources(args) => {
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
            WrCommand::Collect(args) => {
                let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
                let archive = configured_archive(&config, &common)?;
                let outcome = app::collect_into(&common, archive.as_deref())?;
                print_collect_result(&outcome);
            }
            WrCommand::Report(args) => {
                let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
                let backend =
                    cli::resolve_backend(&args.backend, &config, config.wr.mail_to.as_deref())?;
                let dir = common.out_dir.join(common.range.dir_name());
                if backend.worker {
                    run_wr_worker(&backend, &dir, &common.range)?;
                } else {
                    schedule_wr_report(&common, &backend, &dir)?;
                }
            }
            WrCommand::Run(args) => {
                let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
                let backend =
                    cli::resolve_backend(&args.backend, &config, config.wr.mail_to.as_deref())?;
                if backend.worker {
                    let dir = common.out_dir.join(common.range.dir_name());
                    run_wr_worker(&backend, &dir, &common.range)?;
                } else {
                    let sync_settings =
                        cli::configured_sync(&config, &common.home, common.agents.clone())?;
                    if !args.no_sync
                        && let Some(settings) = &sync_settings
                    {
                        run_sync(settings)?;
                    }
                    let archive = sync_settings.as_ref().map(|s| s.path.as_path());
                    let outcome = app::collect_into(&common, archive)?;
                    print_collect_result(&outcome);
                    schedule_wr_report(&common, &backend, &outcome.dir)?;
                }
            }
            WrCommand::Mail(args) => {
                let to = cli::resolve_mail_to(&args.mail_to, config.wr.mail_to.as_deref());
                send_mail_only(&args.report, &to)?;
            }
        },
        Command::Signoff(args) => {
            let (action, run_args) = match args.command {
                None => (SignoffAction::Run, args.run),
                Some(SignoffCommand::Run(r)) => (SignoffAction::Run, r),
                Some(SignoffCommand::Plan(r)) => (SignoffAction::Plan, r),
                Some(SignoffCommand::Mail(m)) => {
                    let to = cli::resolve_mail_to(&m.mail_to, config.signoff.mail_to.as_deref());
                    send_mail_only(&m.report, &to)?;
                    return Ok(());
                }
            };
            let settings = resolve_signoff_settings(&run_args, &config, &default_home);
            let backend =
                cli::resolve_backend(&run_args.llm, &config, Some(settings.mail_to.as_slice()))?;
            match action {
                SignoffAction::Plan => {
                    let (ingest, gated) = signoff::run_plan(&settings, &backend)?;
                    println!(
                        "Signoff plan → {} (auto={}, needs_human={}, deferred={})",
                        ingest.dir.join("plan.json").display(),
                        gated.auto.len(),
                        gated.needs_human.len(),
                        gated.deferred.len()
                    );
                }
                SignoffAction::Run => {
                    if run_args.llm.worker {
                        let path = signoff::run_full(&settings, &backend)?;
                        eprintln!("signoff: wrote {}", path.display());
                        if !settings.mail_to.is_empty() {
                            if !mutt_available() {
                                eprintln!("signoff: {}", mutt_missing_hint());
                            } else {
                                let day = Local::now().date_naive();
                                match mail::send_report_with_mutt(
                                    &settings.mail_to,
                                    &mail_subject_for_day(day),
                                    &path,
                                ) {
                                    Ok(()) => eprintln!(
                                        "signoff: emailed {}",
                                        settings.mail_to.join(", ")
                                    ),
                                    Err(e) => {
                                        eprintln!("signoff: mail failed (signoff.md kept): {e}")
                                    }
                                }
                            }
                        }
                    } else {
                        // Pre-create today's dir for pid/log paths.
                        let day = Local::now().date_naive();
                        let work_dir = settings.out_dir.join("signoff").join(day.to_string());
                        if !settings.mail_to.is_empty() && !mutt_available() {
                            eprintln!("{}", mutt_missing_hint());
                        }
                        let job = job::spawn_signoff_worker(&work_dir, &settings, &backend)?;
                        println!(
                            "Signoff started in background (pid {}) → {} (log: {})",
                            job.pid,
                            job.report_path.display(),
                            job.log_path.display()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn configured_archive(
    config: &Config,
    common: &cli::EffectiveCommon,
) -> Result<Option<std::path::PathBuf>, cli::CliError> {
    Ok(cli::configured_sync(config, &common.home, common.agents.clone())?.map(|s| s.path))
}

fn run_sync(settings: &cli::EffectiveSync) -> anyhow::Result<()> {
    let outcome = buddy::sync::sync(settings)?;
    println!(
        "Synced {} files to {} (created={}, updated={}, skipped={}, failed={})",
        outcome.scanned,
        settings.path.display(),
        outcome.created,
        outcome.updated,
        outcome.skipped,
        outcome.failures.len()
    );
    for failure in &outcome.failures {
        eprintln!("warning: sync failed: {failure}");
    }
    if !outcome.failures.is_empty() {
        anyhow::bail!(
            "sync completed with {} failed file(s)",
            outcome.failures.len()
        );
    }
    Ok(())
}

enum SignoffAction {
    Run,
    Plan,
}

fn print_collect_result(outcome: &buddy::collect::CollectOutcome) {
    for warning in &outcome.warnings {
        eprintln!("warning: {warning}");
    }
    println!(
        "Collected records for {} days → {}",
        outcome.buckets.len(),
        outcome.dir.display()
    );
}

fn schedule_wr_report(
    common: &cli::EffectiveCommon,
    backend: &cli::EffectiveBackend,
    range_dir: &Path,
) -> anyhow::Result<()> {
    let _ = buddy::report::load_collected_dir(range_dir)?;
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

fn run_wr_worker(
    backend: &cli::EffectiveBackend,
    range_dir: &Path,
    range: &buddy::domain::DateRange,
) -> anyhow::Result<()> {
    eprintln!("worker: summarizing {} …", range_dir.display());
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
        Ok(()) => eprintln!("worker: emailed report to {}", backend.mail_to.join(", ")),
        Err(e) => eprintln!("worker: mail failed (report.md kept): {e}"),
    }
    Ok(())
}

fn send_mail_only(report_path: &Path, mail_to: &[String]) -> anyhow::Result<()> {
    if !report_path.is_file() {
        anyhow::bail!(
            "report file not found: {} (generate it with wr/signoff first)",
            report_path.display()
        );
    }
    if mail_to.is_empty() {
        anyhow::bail!("no recipients: set --mail-to or mail_to in the skill config section");
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
    let _ = std::io::stdout().flush();
    Ok(())
}
