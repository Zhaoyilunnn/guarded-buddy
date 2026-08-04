//! Signoff skill: ingest last N hours → plan todos → optional act → summary email.

mod plan;
mod policy;

pub use plan::{SignoffPlan, SignoffTodo, parse_plan_json, plan_prompt};
pub use policy::{GatedPlan, gate_plan};

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Local, NaiveDate};
use serde::Serialize;

use crate::cli::{BackendKind, EffectiveBackend};
use crate::domain::{TimeWindow, filter_sessions_to_window};
use crate::report::cli_backend::{CliSummarizer, parse_custom_cmd, preset};
use crate::report::{Prompt, ReportError, Summarizer};
use crate::report::api_backend::{ApiSummarizer, ReqwestClient};
use crate::render::daily::render_daily;
use crate::sources;

#[derive(Debug, Clone)]
pub struct SignoffSettings {
    pub window_hours: u64,
    pub mail_to: Vec<String>,
    pub allowed_workspaces: Vec<PathBuf>,
    pub max_auto_todos: usize,
    pub act_timeout_secs: u64,
    pub dry_run: bool,
    pub min_confidence: f64,
    pub out_dir: PathBuf,
    pub home: PathBuf,
}

impl Default for SignoffSettings {
    fn default() -> Self {
        Self {
            window_hours: 24,
            mail_to: Vec::new(),
            allowed_workspaces: Vec::new(),
            max_auto_todos: 3,
            act_timeout_secs: 1800,
            dry_run: false,
            min_confidence: 0.8,
            out_dir: PathBuf::from("out"),
            home: PathBuf::from("."),
        }
    }
}

#[derive(Debug)]
pub struct SignoffIngest {
    pub dir: PathBuf,
    pub context_path: PathBuf,
    pub window: TimeWindow,
    pub session_count: usize,
    pub message_count: usize,
}

#[derive(Debug, Serialize)]
struct MetaJson {
    window_start: String,
    window_end: String,
    session_count: usize,
    message_count: usize,
}

/// Collect last `window_hours` of AI chat into `out/signoff/<date>/`.
pub fn ingest(settings: &SignoffSettings) -> std::io::Result<SignoffIngest> {
    let end = Local::now();
    let window = TimeWindow::last_hours(settings.window_hours, end);
    let covering = window.covering_date_range();
    let sources = sources::default_sources(&settings.home);
    let mut warnings = Vec::new();
    let mut sessions = Vec::new();
    for source in &sources {
        sessions.extend(source.collect(&covering, &mut warnings));
    }
    sessions = filter_sessions_to_window(sessions, &window);

    let day: NaiveDate = end.date_naive();
    let dir = settings.out_dir.join("signoff").join(day.to_string());
    std::fs::create_dir_all(&dir)?;

    let message_count: usize = sessions.iter().map(|s| s.messages.len()).sum();
    let session_count = sessions.len();

    // Reuse daily renderer for a single "context day" bucket.
    let context_md = if sessions.is_empty() {
        format!(
            "# Signoff context {}\n\n_No AI coding-assistant activity in the last {} hours._\n",
            day, settings.window_hours
        )
    } else {
        render_daily(day, &sessions)
    };
    let context_path = dir.join("context.md");
    std::fs::write(&context_path, &context_md)?;

    let meta = MetaJson {
        window_start: window.start.to_rfc3339(),
        window_end: window.end.to_rfc3339(),
        session_count,
        message_count,
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap())?;

    for w in warnings {
        eprintln!("warning: {w}");
    }

    Ok(SignoffIngest {
        dir,
        context_path,
        window,
        session_count,
        message_count,
    })
}

fn run_llm(
    backend: &EffectiveBackend,
    cwd: &Path,
    text: &str,
    timeout_secs: Option<u64>,
) -> Result<String, ReportError> {
    let timeout = timeout_secs.unwrap_or(backend.timeout_secs);
    let prompt = Prompt {
        text: text.to_string(),
        dir: cwd.to_path_buf(),
    };
    match backend.kind {
        BackendKind::Cli => {
            let spec = match &backend.cli_cmd {
                Some(cmd) => parse_custom_cmd(cmd)?,
                None => preset(&backend.cli_name)
                    .ok_or_else(|| ReportError::UnknownCliPreset(backend.cli_name.clone()))?,
            };
            CliSummarizer::new(spec, Duration::from_secs(timeout)).summarize(&prompt)
        }
        BackendKind::Api => ApiSummarizer::new(
            ReqwestClient::with_timeout(Duration::from_secs(timeout)),
            backend.api_base_url.clone(),
            backend.api_key_env.clone(),
            backend.api_model.clone(),
        )
        .summarize(&prompt),
    }
}

/// Ingest + plan (no act, no mail). Writes `plan.json`.
pub fn run_plan(
    settings: &SignoffSettings,
    backend: &EffectiveBackend,
) -> Result<(SignoffIngest, GatedPlan), anyhow::Error> {
    let ingest = ingest(settings)?;
    let context = std::fs::read_to_string(&ingest.context_path)?;
    let prompt = plan_prompt(&context, &settings.allowed_workspaces);
    let raw = run_llm(backend, &ingest.dir, &prompt, None)?;
    let plan = match parse_plan_json(&raw) {
        Ok(p) => p,
        Err(_) => {
            let retry = format!(
                "Your previous answer was not valid JSON. Reply with ONLY a JSON object matching the schema.\n\nPrevious output:\n{raw}"
            );
            let raw2 = run_llm(backend, &ingest.dir, &retry, None)?;
            parse_plan_json(&raw2)?
        }
    };
    let gated = gate_plan(
        plan,
        &settings.allowed_workspaces,
        settings.min_confidence,
        settings.max_auto_todos,
    );
    std::fs::write(
        ingest.dir.join("plan.json"),
        serde_json::to_string_pretty(&gated).unwrap(),
    )?;
    Ok((ingest, gated))
}

#[derive(Debug)]
pub struct ActResult {
    pub id: String,
    pub ok: bool,
    pub log_path: PathBuf,
    pub detail: String,
}

/// Run CLI agent on one auto todo inside its workspace.
pub fn act_one(
    backend: &EffectiveBackend,
    todo: &SignoffTodo,
    actions_dir: &Path,
    timeout_secs: u64,
) -> ActResult {
    let id = todo.id.clone();
    let log_path = actions_dir.join(format!("{id}.log"));
    let Some(ws) = todo.workspace.as_ref() else {
        return ActResult {
            id,
            ok: false,
            log_path,
            detail: "missing workspace".into(),
        };
    };
    let ws_path = PathBuf::from(ws);
    if !ws_path.is_dir() {
        return ActResult {
            id,
            ok: false,
            log_path,
            detail: format!("workspace not a directory: {ws}"),
        };
    }

    let prompt_text = format!(
        r#"You are running unattended as part of `buddy signoff`.

Complete this todo in the current working directory.

Title: {title}
Detail: {detail}
Acceptance: {acceptance}

Constraints:
- Do NOT force-push, rewrite git history, or change global git config.
- Do NOT read or write secrets / .env with real credentials.
- Stay inside this workspace.
- When done, print a short summary of what you changed.
"#,
        title = todo.title,
        detail = todo.detail,
        acceptance = todo.acceptance,
    );

    match run_llm(backend, &ws_path, &prompt_text, Some(timeout_secs)) {
        Ok(out) => {
            let _ = std::fs::write(&log_path, &out);
            ActResult {
                id,
                ok: true,
                log_path,
                detail: out.chars().take(500).collect(),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = std::fs::write(&log_path, &msg);
            ActResult {
                id,
                ok: false,
                log_path,
                detail: msg,
            }
        }
    }
}

pub fn render_signoff_md(
    day: NaiveDate,
    gated: &GatedPlan,
    acts: &[ActResult],
    dry_run: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# buddy signoff {day}\n\n"));
    if dry_run {
        out.push_str("_dry_run: no agent actions were executed_\n\n");
    }

    out.push_str("## Auto (executed or would execute)\n\n");
    if gated.auto.is_empty() {
        out.push_str("_none_\n\n");
    } else {
        for t in &gated.auto {
            let act = acts.iter().find(|a| a.id == t.id);
            let status = match (dry_run, act) {
                (true, _) => "would_run",
                (false, Some(a)) if a.ok => "ok",
                (false, Some(_)) => "failed",
                (false, None) => "skipped",
            };
            out.push_str(&format!(
                "- **[{status}] {}** (`{}`)\n  - {}\n",
                t.title,
                t.workspace.as_deref().unwrap_or("?"),
                t.detail
            ));
            if let Some(a) = act {
                out.push_str(&format!("  - log: `{}`\n", a.log_path.display()));
            }
        }
        out.push('\n');
    }

    out.push_str("## Needs human\n\n");
    if gated.needs_human.is_empty() {
        out.push_str("_none_\n\n");
    } else {
        for t in &gated.needs_human {
            out.push_str(&format!(
                "- **{}** — {}\n  - reason: {}\n",
                t.title,
                t.detail,
                t.needs_human_reason
                    .as_deref()
                    .unwrap_or("pending human decision")
            ));
        }
        out.push('\n');
    }

    if !gated.deferred.is_empty() {
        out.push_str("## Deferred (over max_auto_todos)\n\n");
        for t in &gated.deferred {
            out.push_str(&format!("- {}\n", t.title));
        }
        out.push('\n');
    }

    out
}

/// Full signoff run: ingest → plan → act (unless dry_run) → signoff.md.
pub fn run_full(
    settings: &SignoffSettings,
    backend: &EffectiveBackend,
) -> Result<PathBuf, anyhow::Error> {
    let (ingest, gated) = run_plan(settings, backend)?;
    let actions_dir = ingest.dir.join("actions");
    std::fs::create_dir_all(&actions_dir)?;

    let mut acts = Vec::new();
    if !settings.dry_run {
        for todo in &gated.auto {
            eprintln!("signoff: acting on {} …", todo.id);
            acts.push(act_one(
                backend,
                todo,
                &actions_dir,
                settings.act_timeout_secs,
            ));
        }
    }

    let day = Local::now().date_naive();
    let md = render_signoff_md(day, &gated, &acts, settings.dry_run);
    let path = ingest.dir.join("signoff.md");
    std::fs::write(&path, md)?;
    Ok(path)
}

pub fn mail_subject_for_day(day: NaiveDate) -> String {
    format!("buddy signoff {day}")
}
