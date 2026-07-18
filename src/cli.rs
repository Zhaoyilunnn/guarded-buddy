//! CLI 定义（clap derive）与配置合并：CLI flag > config > 内置默认。

use std::path::PathBuf;

use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand};

use crate::config::Config;
use crate::domain::{AgentKind, DateRange, DomainError};

#[derive(Debug, Parser)]
#[command(
    name = "ai-weekly-report",
    version,
    about = "汇总多个 AI 编程助手的对话历史并生成周报"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 采集对话历史 → out/<起止日期>/<日期>.md
    Collect(CollectArgs),
    /// 对已采集的目录生成周报
    Report(ReportArgs),
    /// collect + report 一键完成
    Run(RunArgs),
    /// 列出本机检测到的数据源
    Sources(SourcesArgs),
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
pub struct SourcesArgs {
    /// 覆盖 $HOME（测试用）
    #[arg(long)]
    pub home: Option<PathBuf>,
}

#[derive(Debug, Default, Args)]
pub struct RangeArgs {
    /// 起始日期（含），YYYY-MM-DD；需与 --to 一起使用
    #[arg(long)]
    pub from: Option<NaiveDate>,
    /// 结束日期（含），YYYY-MM-DD
    #[arg(long)]
    pub to: Option<NaiveDate>,
    /// 回看天数（默认 7，与 --from/--to 互斥优先）
    #[arg(long)]
    pub days: Option<u32>,
}

#[derive(Debug, Default, Args)]
pub struct CommonArgs {
    #[command(flatten)]
    pub range: RangeArgs,
    /// 输出根目录（默认 out）
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// 覆盖 $HOME（测试用）
    #[arg(long)]
    pub home: Option<PathBuf>,
    /// 只采集指定 agent（逗号分隔：codex,cursor,claude,gemini）
    #[arg(long, value_delimiter = ',')]
    pub agents: Option<Vec<String>>,
    /// Gemini/antigravity：同时纳入 history.jsonl（prompt 列表）
    #[arg(long)]
    pub include_prompt_history: bool,
}

#[derive(Debug, Default, Args)]
pub struct BackendArgs {
    /// 总结后端：cli | api（默认 cli）
    #[arg(long)]
    pub backend: Option<String>,
    /// CLI 后端预设：codex | claude | agy | gemini（默认 claude）
    #[arg(long)]
    pub cli_name: Option<String>,
    /// 自定义 CLI 命令（覆盖 --cli-name 预设，按空白切分）
    #[arg(long)]
    pub cmd: Option<String>,
    /// 周报模板文件
    #[arg(long)]
    pub template: Option<PathBuf>,
    /// 周报输出到 stdout 而非写文件
    #[arg(long)]
    pub stdout: bool,
    /// API 后端 base URL（默认 https://api.openai.com/v1）
    #[arg(long)]
    pub base_url: Option<String>,
    /// 存放 API key 的环境变量名（默认 OPENAI_API_KEY）
    #[arg(long)]
    pub api_key_env: Option<String>,
    /// API 模型名（默认 gpt-4o-mini）
    #[arg(long)]
    pub model: Option<String>,
    /// 外部 CLI 超时秒数（默认 600）
    #[arg(long)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("--from 和 --to 必须同时提供")]
    MissingRangeBound,
    #[error(transparent)]
    Range(#[from] DomainError),
    #[error("未知的 backend: {0}（可选: cli, api）")]
    InvalidBackend(String),
    #[error("未知的 agent: {0}（可选: codex, cursor, claude, gemini）")]
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
    pub stdout: bool,
    pub api_base_url: String,
    pub api_key_env: String,
    pub api_model: String,
    pub timeout_secs: u64,
}

/// 合并采集侧选项。`today`/`default_home` 由调用方注入以便测试。
pub fn resolve_common(
    common: &CommonArgs,
    config: &Config,
    today: NaiveDate,
    default_home: &std::path::Path,
) -> Result<EffectiveCommon, CliError> {
    let range = match (common.range.from, common.range.to) {
        (Some(from), Some(to)) => DateRange::new(from, to)?,
        (None, None) => {
            let days = common.range.days.or(config.days).unwrap_or(7);
            DateRange::last_n_days(days, today)
        }
        _ => return Err(CliError::MissingRangeBound),
    };
    let out_dir = common
        .out
        .clone()
        .or_else(|| config.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("out"));
    let home = common.home.clone().unwrap_or_else(|| default_home.to_path_buf());
    let agents = common
        .agents
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|s| {
                    AgentKind::from_slug(s).ok_or_else(|| CliError::InvalidAgent(s.clone()))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(EffectiveCommon {
        range,
        out_dir,
        home,
        agents,
        include_prompt_history: common.include_prompt_history
            || config.include_prompt_history.unwrap_or(false),
    })
}

/// 合并总结后端选项。
pub fn resolve_backend(args: &BackendArgs, config: &Config) -> Result<EffectiveBackend, CliError> {
    let kind = match args
        .backend
        .as_deref()
        .or(config.backend.as_deref())
        .unwrap_or("cli")
    {
        "cli" => BackendKind::Cli,
        "api" => BackendKind::Api,
        other => return Err(CliError::InvalidBackend(other.to_string())),
    };
    Ok(EffectiveBackend {
        kind,
        cli_name: args
            .cli_name
            .clone()
            .or_else(|| config.cli_name.clone())
            .unwrap_or_else(|| "claude".to_string()),
        cli_cmd: args.cmd.clone().or_else(|| config.cli_cmd.clone()),
        template: args.template.clone().or_else(|| config.template.clone()),
        stdout: args.stdout,
        api_base_url: args
            .base_url
            .clone()
            .or_else(|| config.api_base_url.clone())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        api_key_env: args
            .api_key_env
            .clone()
            .or_else(|| config.api_key_env.clone())
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        api_model: args
            .model
            .clone()
            .or_else(|| config.api_model.clone())
            .unwrap_or_else(|| "gpt-4o-mini".to_string()),
        timeout_secs: args
            .timeout_secs
            .or(config.timeout_secs)
            .unwrap_or(600),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BackendArgs, BackendKind, Cli, CliError, Command, CommonArgs, RangeArgs, resolve_backend,
        resolve_common,
    };
    use crate::config::Config;
    use crate::domain::{AgentKind, DomainError};
    use chrono::NaiveDate;
    use clap::{CommandFactory, Parser};
    use std::path::{Path, PathBuf};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn today() -> NaiveDate {
        d("2026-07-18")
    }

    fn fake_home() -> PathBuf {
        PathBuf::from("/fake/home")
    }

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_collect_with_dates_and_agents() {
        let cli = Cli::try_parse_from([
            "ai-weekly-report",
            "collect",
            "--from",
            "2026-07-12",
            "--to",
            "2026-07-18",
            "--agents",
            "codex,claude",
        ])
        .unwrap();
        let Command::Collect(args) = cli.command else {
            panic!("expected collect")
        };
        assert_eq!(args.common.range.from, Some(d("2026-07-12")));
        assert_eq!(args.common.range.to, Some(d("2026-07-18")));
        assert_eq!(
            args.common.agents,
            Some(vec!["codex".to_string(), "claude".to_string()])
        );
    }

    #[test]
    fn parses_run_with_backend_flags() {
        let cli = Cli::try_parse_from([
            "ai-weekly-report",
            "run",
            "--days",
            "14",
            "--backend",
            "api",
            "--model",
            "deepseek-chat",
            "--stdout",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.common.range.days, Some(14));
        assert_eq!(args.backend.backend.as_deref(), Some("api"));
        assert_eq!(args.backend.model.as_deref(), Some("deepseek-chat"));
        assert!(args.backend.stdout);
    }

    // ---------- resolve_common ----------

    #[test]
    fn resolve_range_prefers_from_to_over_days() {
        let common = CommonArgs {
            range: RangeArgs {
                from: Some(d("2026-07-12")),
                to: Some(d("2026-07-18")),
                days: Some(3),
            },
            ..Default::default()
        };
        let eff = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap();
        assert_eq!(eff.range.start, d("2026-07-12"));
        assert_eq!(eff.range.end, d("2026-07-18"));
    }

    #[test]
    fn resolve_range_days_flag_over_config_over_default() {
        let config = Config {
            days: Some(14),
            ..Default::default()
        };
        // flag > config
        let common = CommonArgs {
            range: RangeArgs {
                days: Some(3),
                ..Default::default()
            },
            ..Default::default()
        };
        let eff = resolve_common(&common, &config, today(), &fake_home()).unwrap();
        assert_eq!(eff.range.start, d("2026-07-16")); // 今天往前 2 天
        assert_eq!(eff.range.end, today());
        // config > default
        let eff = resolve_common(&CommonArgs::default(), &config, today(), &fake_home()).unwrap();
        assert_eq!(eff.range.start, d("2026-07-05")); // 14 天
        // default = 7
        let eff = resolve_common(
            &CommonArgs::default(),
            &Config::default(),
            today(),
            &fake_home(),
        )
        .unwrap();
        assert_eq!(eff.range.start, d("2026-07-12")); // 7 天
    }

    #[test]
    fn resolve_range_single_bound_errors() {
        let common = CommonArgs {
            range: RangeArgs {
                from: Some(d("2026-07-12")),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap_err();
        assert!(matches!(err, CliError::MissingRangeBound));
    }

    #[test]
    fn resolve_range_inverted_propagates_domain_error() {
        let common = CommonArgs {
            range: RangeArgs {
                from: Some(d("2026-07-18")),
                to: Some(d("2026-07-12")),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap_err();
        assert!(matches!(
            err,
            CliError::Range(DomainError::InvertedRange { .. })
        ));
    }

    #[test]
    fn resolve_out_dir_precedence() {
        // flag > config
        let common = CommonArgs {
            out: Some(PathBuf::from("flag-out")),
            ..Default::default()
        };
        let config = Config {
            out_dir: Some("config-out".to_string()),
            ..Default::default()
        };
        let eff = resolve_common(&common, &config, today(), &fake_home()).unwrap();
        assert_eq!(eff.out_dir, PathBuf::from("flag-out"));
        // config > default
        let eff = resolve_common(&CommonArgs::default(), &config, today(), &fake_home()).unwrap();
        assert_eq!(eff.out_dir, PathBuf::from("config-out"));
        // default
        let eff = resolve_common(
            &CommonArgs::default(),
            &Config::default(),
            today(),
            &fake_home(),
        )
        .unwrap();
        assert_eq!(eff.out_dir, PathBuf::from("out"));
    }

    #[test]
    fn resolve_home_flag_over_default() {
        let common = CommonArgs {
            home: Some(PathBuf::from("/custom/home")),
            ..Default::default()
        };
        let eff = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap();
        assert_eq!(eff.home, PathBuf::from("/custom/home"));
        let eff = resolve_common(
            &CommonArgs::default(),
            &Config::default(),
            today(),
            &fake_home(),
        )
        .unwrap();
        assert_eq!(eff.home, fake_home());
    }

    #[test]
    fn resolve_agents_parsed_and_invalid_rejected() {
        let common = CommonArgs {
            agents: Some(vec!["codex".to_string(), "gemini".to_string()]),
            ..Default::default()
        };
        let eff = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap();
        assert_eq!(
            eff.agents,
            Some(vec![AgentKind::Codex, AgentKind::Gemini])
        );

        let common = CommonArgs {
            agents: Some(vec!["nonexistent".to_string()]),
            ..Default::default()
        };
        let err = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap_err();
        assert!(matches!(err, CliError::InvalidAgent(_)));
    }

    // ---------- resolve_backend ----------

    #[test]
    fn resolve_backend_defaults() {
        let eff = resolve_backend(&BackendArgs::default(), &Config::default()).unwrap();
        assert_eq!(eff.kind, BackendKind::Cli);
        assert_eq!(eff.cli_name, "claude");
        assert_eq!(eff.timeout_secs, 600);
        assert_eq!(eff.api_base_url, "https://api.openai.com/v1");
        assert_eq!(eff.api_key_env, "OPENAI_API_KEY");
        assert_eq!(eff.api_model, "gpt-4o-mini");
        assert!(!eff.stdout);
    }

    #[test]
    fn resolve_backend_flag_over_config() {
        let config = Config {
            backend: Some("api".to_string()),
            cli_name: Some("codex".to_string()),
            timeout_secs: Some(60),
            ..Default::default()
        };
        // config 生效
        let eff = resolve_backend(&BackendArgs::default(), &config).unwrap();
        assert_eq!(eff.kind, BackendKind::Api);
        assert_eq!(eff.cli_name, "codex");
        assert_eq!(eff.timeout_secs, 60);
        // flag > config
        let args = BackendArgs {
            backend: Some("cli".to_string()),
            cli_name: Some("agy".to_string()),
            ..Default::default()
        };
        let eff = resolve_backend(&args, &config).unwrap();
        assert_eq!(eff.kind, BackendKind::Cli);
        assert_eq!(eff.cli_name, "agy");
        assert_eq!(eff.timeout_secs, 60); // 未覆盖的仍取 config
    }

    #[test]
    fn resolve_backend_api_fields_from_config() {
        let config = Config {
            backend: Some("api".to_string()),
            api_base_url: Some("https://api.deepseek.com/v1".to_string()),
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            api_model: Some("deepseek-chat".to_string()),
            ..Default::default()
        };
        let eff = resolve_backend(&BackendArgs::default(), &config).unwrap();
        assert_eq!(eff.kind, BackendKind::Api);
        assert_eq!(eff.api_base_url, "https://api.deepseek.com/v1");
        assert_eq!(eff.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(eff.api_model, "deepseek-chat");
    }

    #[test]
    fn resolve_backend_invalid_backend_errors() {
        let args = BackendArgs {
            backend: Some("mystery".to_string()),
            ..Default::default()
        };
        let err = resolve_backend(&args, &Config::default()).unwrap_err();
        assert!(matches!(err, CliError::InvalidBackend(_)));
    }

    #[test]
    fn resolve_common_include_prompt_history_flag_or_config() {
        let config = Config {
            include_prompt_history: Some(true),
            ..Default::default()
        };
        let eff = resolve_common(&CommonArgs::default(), &config, today(), &fake_home()).unwrap();
        assert!(eff.include_prompt_history);
        let common = CommonArgs {
            include_prompt_history: true,
            ..Default::default()
        };
        let eff = resolve_common(&common, &Config::default(), today(), &fake_home()).unwrap();
        assert!(eff.include_prompt_history);
        let eff = resolve_common(
            &CommonArgs::default(),
            &Config::default(),
            today(),
            &fake_home(),
        )
        .unwrap();
        assert!(!eff.include_prompt_history);
    }

    #[test]
    fn resolve_template_flag_over_config() {
        let config = Config {
            template: Some(PathBuf::from("/config/tpl.md")),
            ..Default::default()
        };
        let eff = resolve_backend(&BackendArgs::default(), &config).unwrap();
        assert_eq!(eff.template, Some(PathBuf::from("/config/tpl.md")));
        let args = BackendArgs {
            template: Some(PathBuf::from("/flag/tpl.md")),
            ..Default::default()
        };
        let eff = resolve_backend(&args, &config).unwrap();
        assert_eq!(eff.template, Some(PathBuf::from("/flag/tpl.md")));
    }

    #[test]
    fn sources_subcommand_parses() {
        let cli = Cli::try_parse_from(["ai-weekly-report", "sources"]).unwrap();
        assert!(matches!(cli.command, Command::Sources(_)));
    }

    #[test]
    fn home_used_in_sources_args() {
        let cli = Cli::try_parse_from(["ai-weekly-report", "sources", "--home", "/tmp/h"]).unwrap();
        let Command::Sources(args) = cli.command else {
            panic!("expected sources")
        };
        assert_eq!(args.home, Some(Path::new("/tmp/h").to_path_buf()));
    }
}
