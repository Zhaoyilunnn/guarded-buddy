//! Thin entry point: CLI → config → dispatch → exit code.

use std::path::Path;

use ai_weekly_report::app;
use ai_weekly_report::cli::{self, Cli, Command};
use ai_weekly_report::config::Config;
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
            let report = app::summarize_range_dir(&backend, &dir, &common.range)?;
            emit_report(&report, &dir, backend.stdout)?;
        }
        Command::Run(args) => {
            let common = cli::resolve_common(&args.common, &config, today, &default_home)?;
            let backend = cli::resolve_backend(&args.backend, &config)?;
            let outcome = app::collect_into(&common)?;
            print_collect_result(&outcome);
            let report = app::summarize_range_dir(&backend, &outcome.dir, &common.range)?;
            emit_report(&report, &outcome.dir, backend.stdout)?;
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

fn emit_report(report: &str, dir: &Path, stdout: bool) -> anyhow::Result<()> {
    if stdout {
        println!("{report}");
    } else {
        let path = dir.join("report.md");
        std::fs::write(&path, report)?;
        println!("Weekly report written to {}", path.display());
    }
    Ok(())
}
