//! CLI: nested skills `wr` / `signoff` / `completions`.

use std::path::PathBuf;

use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::domain::{AgentKind, DateRange, DomainError};
use crate::signoff::SignoffSettings;

#[derive(Debug, Parser)]
#[command(
    name = "buddy",
    version,
    about = "A guarded buddy — agents on a leash for weekly reports and signoff"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Weekly report skill
    #[command(name = "wr", visible_alias = "weekly-report")]
    Wr(WrArgs),
    /// End-of-day silent progress skill
    Signoff(SignoffArgs),
    /// Generate shell completion script
    Completions(CompletionsArgs),
}

#[derive(Debug, Args)]
pub struct WrArgs {
    #[command(subcommand)]
    pub command: WrCommand,
}

#[derive(Debug, Subcommand)]
pub enum WrCommand {
    Collect(CollectArgs),
    Report(ReportArgs),
    Run(RunArgs),
    Mail(MailArgs),
    Sources(SourcesArgs),
}

#[derive(Debug, Args)]
pub struct SignoffArgs {
    #[command(subcommand)]
    pub command: Option<SignoffCommand>,
    #[command(flatten)]
    pub run: SignoffRunArgs,
}

#[derive(Debug, Subcommand)]
pub enum SignoffCommand {
    /// Ingest → plan → act → email (default when no subcommand)
    Run(SignoffRunArgs),
    /// Ingest → plan only (no act, no email)
    Plan(SignoffRunArgs),
    /// Resend an existing signoff.md
    Mail(MailArgs),
}

#[derive(Debug, Default, Args)]
pub struct SignoffRunArgs {
    #[command(flatten)]
    pub llm: BackendArgs,
    /// Output root (default out)
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Override $HOME
    #[arg(long)]
    pub home: Option<PathBuf>,
    /// Lookback hours (default 24)
    #[arg(long)]
    pub window_hours: Option<u64>,
    /// Force dry-run (plan only actions, still writes signoff.md on run)
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    pub shell: ShellKind,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Args)]
pub struct CollectArgs {
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub backend: BackendArgs,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    #[command(flatten)]
    pub backend: BackendArgs,
}

#[derive(Debug, Args)]
pub struct MailArgs {
    pub report: PathBuf,
    #[arg(long, value_delimiter = ',')]
    pub mail_to: Option<Vec<String>>,
}

#[derive(Debug, Args)]
pub struct SourcesArgs {
    #[arg(long)]
    pub home: Option<PathBuf>,
}

#[derive(Debug, Default, Args)]
pub struct RangeArgs {
    #[arg(long)]
    pub from: Option<NaiveDate>,
    #[arg(long)]
    pub to: Option<NaiveDate>,
    #[arg(long)]
    pub days: Option<u32>,
}

#[derive(Debug, Default, Args)]
pub struct CommonArgs {
    #[command(flatten)]
    pub range: RangeArgs,
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long)]
    pub home: Option<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    pub agents: Option<Vec<String>>,
    #[arg(long)]
    pub include_prompt_history: bool,
}

#[derive(Debug, Default, Args)]
pub struct BackendArgs {
    #[arg(long)]
    pub backend: Option<String>,
    #[arg(long)]
    pub cli_name: Option<String>,
    #[arg(long)]
    pub cmd: Option<String>,
    #[arg(long)]
    pub template: Option<PathBuf>,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub api_key_env: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    #[arg(long, value_delimiter = ',')]
    pub mail_to: Option<Vec<String>>,
    #[arg(long, hide = true)]
    pub worker: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("--from and --to must be provided together")]
    MissingRangeBound,
    #[error(transparent)]
    Range(#[from] DomainError),
    #[error("unknown backend: {0} (expected: cli, api)")]
    InvalidBackend(String),
    #[error("unknown agent: {0} (expected: codex, cursor, claude, gemini)")]
    InvalidAgent(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Cli,
    Api,
}

#[derive(Debug)]
pub struct EffectiveCommon {
    pub range: DateRange,
    pub out_dir: PathBuf,
    pub home: PathBuf,
    pub agents: Option<Vec<AgentKind>>,
    pub include_prompt_history: bool,
}

#[derive(Debug)]
pub struct EffectiveBackend {
    pub kind: BackendKind,
    pub cli_name: String,
    pub cli_cmd: Option<String>,
    pub template: Option<PathBuf>,
    pub api_base_url: String,
    pub api_key_env: String,
    pub api_model: String,
    pub timeout_secs: u64,
    pub mail_to: Vec<String>,
    pub worker: bool,
}

pub fn resolve_common(
    common: &CommonArgs,
    config: &Config,
    today: NaiveDate,
    default_home: &std::path::Path,
) -> Result<EffectiveCommon, CliError> {
    let range = match (common.range.from, common.range.to) {
        (Some(from), Some(to)) => DateRange::new(from, to)?,
        (None, None) => {
            let days = common.range.days.or(config.wr.days).unwrap_or(7);
            DateRange::last_n_days(days, today)
        }
        _ => return Err(CliError::MissingRangeBound),
    };
    let out_dir = common
        .out
        .clone()
        .or_else(|| config.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("out"));
    let home = common
        .home
        .clone()
        .unwrap_or_else(|| default_home.to_path_buf());
    let agents = common
        .agents
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|s| AgentKind::from_slug(s).ok_or_else(|| CliError::InvalidAgent(s.clone())))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(EffectiveCommon {
        range,
        out_dir,
        home,
        agents,
        include_prompt_history: common.include_prompt_history
            || config.wr.include_prompt_history.unwrap_or(false),
    })
}

/// Resolve LLM backend; `mail_to` comes from skill section (wr or signoff).
pub fn resolve_backend(
    args: &BackendArgs,
    config: &Config,
    skill_mail_to: Option<&[String]>,
) -> Result<EffectiveBackend, CliError> {
    let kind = match args
        .backend
        .as_deref()
        .or(config.llm.backend.as_deref())
        .unwrap_or("cli")
    {
        "cli" => BackendKind::Cli,
        "api" => BackendKind::Api,
        other => return Err(CliError::InvalidBackend(other.to_string())),
    };
    let mail_to = args
        .mail_to
        .clone()
        .or_else(|| skill_mail_to.map(|s| s.to_vec()))
        .unwrap_or_default();
    Ok(EffectiveBackend {
        kind,
        cli_name: args
            .cli_name
            .clone()
            .or_else(|| config.llm.cli_name.clone())
            .unwrap_or_else(|| "claude".to_string()),
        cli_cmd: args.cmd.clone().or_else(|| config.llm.cli_cmd.clone()),
        template: args
            .template
            .clone()
            .or_else(|| config.wr.template.clone())
            .or_else(|| config.llm.template.clone()),
        api_base_url: args
            .base_url
            .clone()
            .or_else(|| config.llm.api_base_url.clone())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        api_key_env: args
            .api_key_env
            .clone()
            .or_else(|| config.llm.api_key_env.clone())
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        api_model: args
            .model
            .clone()
            .or_else(|| config.llm.api_model.clone())
            .unwrap_or_else(|| "gpt-4o-mini".to_string()),
        timeout_secs: args
            .timeout_secs
            .or(config.llm.timeout_secs)
            .unwrap_or(600),
        mail_to,
        worker: args.worker,
    })
}

pub fn resolve_mail_to(cli: &Option<Vec<String>>, skill_default: Option<&[String]>) -> Vec<String> {
    cli.clone()
        .or_else(|| skill_default.map(|s| s.to_vec()))
        .unwrap_or_default()
}

pub fn resolve_signoff_settings(
    args: &SignoffRunArgs,
    config: &Config,
    default_home: &std::path::Path,
) -> SignoffSettings {
    let conf = &config.signoff;
    let min_confidence = conf
        .min_confidence
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.8);
    SignoffSettings {
        window_hours: args.window_hours.or(conf.window_hours).unwrap_or(24),
        mail_to: args
            .llm
            .mail_to
            .clone()
            .or_else(|| conf.mail_to.clone())
            .unwrap_or_default(),
        allowed_workspaces: conf
            .allowed_workspaces
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        allow_all_workspaces: conf.allow_all_workspaces.unwrap_or(false),
        max_auto_todos: conf.max_auto_todos.unwrap_or(3),
        act_timeout_secs: conf.act_timeout_secs.unwrap_or(7200),
        dry_run: args.dry_run || conf.dry_run.unwrap_or(false),
        min_confidence,
        out_dir: args
            .out
            .clone()
            .or_else(|| config.out_dir.as_ref().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("out")),
        home: args
            .home
            .clone()
            .unwrap_or_else(|| default_home.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_wr_run() {
        let cli = Cli::try_parse_from([
            "buddy",
            "wr",
            "run",
            "--days",
            "7",
            "--backend",
            "api",
        ])
        .unwrap();
        let Command::Wr(wr) = cli.command else {
            panic!("expected wr")
        };
        let WrCommand::Run(args) = wr.command else {
            panic!("expected run")
        };
        assert_eq!(args.common.range.days, Some(7));
        assert_eq!(args.backend.backend.as_deref(), Some("api"));
    }

    #[test]
    fn parses_weekly_report_alias() {
        let cli = Cli::try_parse_from(["buddy", "weekly-report", "sources"]).unwrap();
        assert!(matches!(cli.command, Command::Wr(_)));
    }

    #[test]
    fn parses_signoff_default_and_plan() {
        let cli = Cli::try_parse_from(["buddy", "signoff", "--dry-run"]).unwrap();
        let Command::Signoff(args) = cli.command else {
            panic!("expected signoff")
        };
        assert!(args.command.is_none());
        assert!(args.run.dry_run);

        let cli = Cli::try_parse_from(["buddy", "signoff", "plan"]).unwrap();
        let Command::Signoff(args) = cli.command else {
            panic!("expected signoff")
        };
        assert!(matches!(args.command, Some(SignoffCommand::Plan(_))));
    }

    #[test]
    fn parses_completions() {
        let cli = Cli::try_parse_from(["buddy", "completions", "bash"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions(CompletionsArgs {
                shell: ShellKind::Bash
            })
        ));
    }

    #[test]
    fn resolve_backend_uses_wr_mail() {
        let config = Config {
            wr: crate::config::WrConfig {
                mail_to: Some(vec!["w@example.com".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let eff = resolve_backend(
            &BackendArgs::default(),
            &config,
            config.wr.mail_to.as_deref(),
        )
        .unwrap();
        assert_eq!(eff.mail_to, vec!["w@example.com".to_string()]);
    }
}
