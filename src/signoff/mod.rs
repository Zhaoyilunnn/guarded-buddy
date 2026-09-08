//! Signoff skill: ingest the last N hours, plan tasks, optionally act, then email a summary.

mod plan;
mod policy;
mod trust;

pub use plan::{SignoffPlan, SignoffTodo, parse_plan_json, plan_prompt};
pub use policy::{GatedPlan, gate_plan};
pub use trust::{WorkspaceTrust, WorkspaceTrustEntry, preset_for_act, trust_for};

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
    pub workspaces: Vec<WorkspaceTrustEntry>,
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
            workspaces: Vec::new(),
            max_auto_todos: 3,
            act_timeout_secs: 7200,
            dry_run: false,
            min_confidence: 0.8,
            out_dir: PathBuf::from("out"),
            home: PathBuf::from("."),
        }
    }
}

impl SignoffSettings {
    pub fn trust_for(&self, workspace: &Path) -> WorkspaceTrust {
        trust_for(workspace, &self.workspaces)
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
    act_trust: Option<WorkspaceTrust>,
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
                None => match act_trust {
                    Some(trust) => preset_for_act(&backend.cli_name, trust).ok_or_else(|| {
                        ReportError::UnknownCliPreset(backend.cli_name.clone())
                    })?,
                    None => preset(&backend.cli_name)
                        .ok_or_else(|| ReportError::UnknownCliPreset(backend.cli_name.clone()))?,
                },
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
    let prompt = plan_prompt(&context, &settings.workspaces);
    let raw = run_llm(backend, &ingest.dir, &prompt, None, None)?;
    let plan = match parse_plan_json(&raw) {
        Ok(p) => p,
        Err(_) => {
            let retry = format!(
                "Your previous answer was not valid JSON. Reply with ONLY a JSON object matching the schema.\n\nPrevious output:\n{raw}"
            );
            let raw2 = run_llm(backend, &ingest.dir, &retry, None, None)?;
            parse_plan_json(&raw2)?
        }
    };
    let gated = gate_plan(plan, settings.min_confidence, settings.max_auto_todos);
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

const ACT_STATUS_COMPLETED: &str = "BUDDY_SIGNOFF_STATUS: completed";
const ACT_STATUS_INCOMPLETE: &str = "BUDDY_SIGNOFF_STATUS: incomplete";

fn act_status(output: &str) -> Result<(), &'static str> {
    match output.lines().rev().find(|line| !line.trim().is_empty()) {
        Some(line) if line.trim() == ACT_STATUS_COMPLETED => Ok(()),
        Some(line) if line.trim() == ACT_STATUS_INCOMPLETE => {
            Err("agent reported that the task is incomplete")
        }
        _ => Err("agent did not report a valid completion status"),
    }
}

/// Run CLI agent on one auto todo inside its workspace.
pub fn act_one(
    backend: &EffectiveBackend,
    todo: &SignoffTodo,
    actions_dir: &Path,
    timeout_secs: u64,
    trust: WorkspaceTrust,
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
- Print a short summary of what you changed.
- Your final non-empty line MUST be exactly one of:
  BUDDY_SIGNOFF_STATUS: completed
  BUDDY_SIGNOFF_STATUS: incomplete
- Use "incomplete" if you could not finish the todo or verify its acceptance criteria.
"#,
        title = todo.title,
        detail = todo.detail,
        acceptance = todo.acceptance,
    );

    match run_llm(
        backend,
        &ws_path,
        &prompt_text,
        Some(timeout_secs),
        Some(trust),
    ) {
        Ok(out) => {
            let _ = std::fs::write(&log_path, &out);
            let status = act_status(&out);
            let detail = match status {
                Ok(()) => out.chars().take(500).collect(),
                Err(reason) => format!("{reason}\n\n{}", out.chars().take(500).collect::<String>()),
            };
            ActResult {
                id,
                ok: status.is_ok(),
                log_path,
                detail,
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

/// Run the full signoff flow: ingest, plan, act unless dry-run, then write `signoff.md`.
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
            let trust = todo
                .workspace
                .as_deref()
                .map(Path::new)
                .map(|p| settings.trust_for(p))
                .unwrap_or_default();
            eprintln!(
                "signoff: acting on {} (trust={}) …",
                todo.id,
                trust.as_str()
            );
            acts.push(act_one(
                backend,
                todo,
                &actions_dir,
                settings.act_timeout_secs,
                trust,
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

#[cfg(test)]
mod tests {
    use super::plan::Autonomy;
    use super::{ActResult, GatedPlan, SignoffTodo, WorkspaceTrust, act_one, render_signoff_md};
    use crate::cli::{BackendKind, EffectiveBackend};
    use chrono::NaiveDate;
    use std::path::{Path, PathBuf};

    fn fake_script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fake-agent.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (tmp, path)
    }

    fn backend(script: &Path) -> EffectiveBackend {
        EffectiveBackend {
            kind: BackendKind::Cli,
            cli_name: "unused".into(),
            cli_cmd: Some(script.display().to_string()),
            template: None,
            api_base_url: String::new(),
            api_key_env: String::new(),
            api_model: String::new(),
            timeout_secs: 5,
            mail_to: Vec::new(),
            worker: false,
        }
    }

    fn todo(workspace: &Path) -> SignoffTodo {
        SignoffTodo {
            id: "t1".into(),
            title: "fix signoff".into(),
            detail: "make the change".into(),
            autonomy: Autonomy::Auto,
            confidence: 1.0,
            needs_human_reason: None,
            workspace: Some(workspace.display().to_string()),
            acceptance: "tests pass".into(),
            evidence: Vec::new(),
        }
    }

    fn rendered(todo: SignoffTodo, result: ActResult) -> String {
        let plan = GatedPlan {
            auto: vec![todo],
            needs_human: Vec::new(),
            deferred: Vec::new(),
        };
        render_signoff_md(
            NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(),
            &plan,
            &[result],
            false,
        )
    }

    #[test]
    fn completed_agent_report_is_marked_ok() {
        let workspace = tempfile::tempdir().unwrap();
        let actions = tempfile::tempdir().unwrap();
        let (_script_dir, script) = fake_script(
            "cat > /dev/null\nprintf 'Implemented the fix and tests pass.\\nBUDDY_SIGNOFF_STATUS: completed\\n'",
        );
        let todo = todo(workspace.path());

        let result = act_one(
            &backend(&script),
            &todo,
            actions.path(),
            5,
            WorkspaceTrust::Yolo,
        );

        assert!(result.ok, "{}", result.detail);
        assert!(rendered(todo, result).contains("[ok]"));
    }

    #[test]
    fn exit_zero_incomplete_agent_report_is_marked_failed() {
        let workspace = tempfile::tempdir().unwrap();
        let actions = tempfile::tempdir().unwrap();
        let (_script_dir, script) = fake_script(
            "cat > /dev/null\nprintf 'I could not complete the task.\\nBUDDY_SIGNOFF_STATUS: incomplete\\n'\nexit 0",
        );
        let todo = todo(workspace.path());

        let result = act_one(
            &backend(&script),
            &todo,
            actions.path(),
            5,
            WorkspaceTrust::Yolo,
        );

        assert!(
            !result.ok,
            "exit code 0 must not override an incomplete report"
        );
        let md = rendered(todo, result);
        assert!(md.contains("[failed]"));
        assert!(!md.contains("[ok]"));
    }

    #[test]
    fn exit_zero_without_completion_status_is_marked_failed() {
        let workspace = tempfile::tempdir().unwrap();
        let actions = tempfile::tempdir().unwrap();
        let (_script_dir, script) = fake_script(
            "cat > /dev/null\nprintf 'I made some changes, but gave no final status.\\n'",
        );
        let todo = todo(workspace.path());

        let result = act_one(
            &backend(&script),
            &todo,
            actions.path(),
            5,
            WorkspaceTrust::Yolo,
        );

        assert!(!result.ok);
        assert!(
            result.detail.contains("valid completion status"),
            "{}",
            result.detail
        );
        assert!(rendered(todo, result).contains("[failed]"));
    }

    #[test]
    fn timed_out_agent_is_marked_failed() {
        let workspace = tempfile::tempdir().unwrap();
        let actions = tempfile::tempdir().unwrap();
        let (_script_dir, script) = fake_script("while :; do :; done");
        let todo = todo(workspace.path());

        let result = act_one(
            &backend(&script),
            &todo,
            actions.path(),
            0,
            WorkspaceTrust::Yolo,
        );

        assert!(!result.ok);
        assert!(result.detail.contains("timed out"), "{}", result.detail);
        assert!(rendered(todo, result).contains("[failed]"));
    }
}
