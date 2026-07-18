//! Application orchestration: thin glue layer wiring cli/config/sources/collect/report.
//! Business logic lives in each module (unit-testable); this layer only sequences steps and converts types.

use std::path::Path;
use std::time::Duration;

use crate::cli::{BackendKind, EffectiveBackend, EffectiveCommon};
use crate::collect::{CollectOutcome, collect};
use crate::domain::{AgentKind, DateRange};
use crate::report::api_backend::{ApiSummarizer, ReqwestClient};
use crate::report::cli_backend::{CliSummarizer, parse_custom_cmd, preset};
use crate::report::{PromptMode, ReportError, Summarizer, assemble_prompt, load_collected_dir};
use crate::sources::{self, HistorySource};
use crate::template::{TemplateContext, TemplateEngine};

/// Build the data-source registry with optional agent filtering.
pub fn build_sources(
    home: &Path,
    agents: &Option<Vec<AgentKind>>,
    include_prompt_history: bool,
) -> Vec<Box<dyn HistorySource>> {
    sources::default_sources_opts(home, include_prompt_history)
        .into_iter()
        .filter(|s| agents.as_ref().is_none_or(|a| a.contains(&s.kind())))
        .collect()
}

/// Collect and write files under `out_dir/<range.dir_name()>/`.
pub fn collect_into(common: &EffectiveCommon) -> std::io::Result<CollectOutcome> {
    let sources = build_sources(&common.home, &common.agents, common.include_prompt_history);
    collect(&sources, &common.range, &common.out_dir)
}

/// Generate weekly report text from a collected range directory.
pub fn summarize_range_dir(
    backend: &EffectiveBackend,
    range_dir: &Path,
    range: &DateRange,
) -> Result<String, ReportError> {
    let summary = load_collected_dir(range_dir)?;
    let engine = match &backend.template {
        Some(path) => TemplateEngine::from_file(path)
            .map_err(|e| ReportError::ApiParse(e.to_string()))?,
        None => TemplateEngine::default_template(),
    };
    let mode = match backend.kind {
        BackendKind::Cli => PromptMode::FilesManifest,
        BackendKind::Api => PromptMode::Inline,
    };
    let daily_notes_marker = match mode {
        PromptMode::FilesManifest => "（对话记录为当前工作目录下的日文件，请逐一阅读）",
        PromptMode::Inline => "（对话记录全文见下文）",
    };
    let ctx = TemplateContext {
        start_date: range.start.to_string(),
        end_date: range.end.to_string(),
        stats: summary.stats.clone(),
        daily_notes: daily_notes_marker.to_string(),
    };
    let rendered = engine.render(&ctx);
    let prompt = assemble_prompt(&rendered, mode, range_dir, &summary.files);
    match backend.kind {
        BackendKind::Cli => {
            let spec = match &backend.cli_cmd {
                Some(cmd) => parse_custom_cmd(cmd)?,
                None => preset(&backend.cli_name)
                    .ok_or_else(|| ReportError::UnknownCliPreset(backend.cli_name.clone()))?,
            };
            CliSummarizer::new(spec, Duration::from_secs(backend.timeout_secs)).summarize(&prompt)
        }
        BackendKind::Api => ApiSummarizer::new(
            ReqwestClient::new(),
            backend.api_base_url.clone(),
            backend.api_key_env.clone(),
            backend.api_model.clone(),
        )
        .summarize(&prompt),
    }
}
