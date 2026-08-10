//! LLM plan JSON for signoff todos.

use serde::{Deserialize, Serialize};

use super::trust::{WorkspaceTrust, WorkspaceTrustEntry};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    Auto,
    NeedsHuman,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignoffTodo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    pub autonomy: Autonomy,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub needs_human_reason: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub acceptance: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignoffPlan {
    pub todos: Vec<SignoffTodo>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("plan JSON parse error: {0}")]
    Parse(String),
}

/// Build the planning prompt. Configured trusts are hints; unlisted paths default to yolo.
pub fn plan_prompt(context_md: &str, workspaces: &[WorkspaceTrustEntry]) -> String {
    let trust_hint = if workspaces.is_empty() {
        "(none configured — every concrete absolute path defaults to trust=yolo at act time)"
            .to_string()
    } else {
        workspaces
            .iter()
            .map(|e| format!("- {} → {}", e.path.display(), e.trust.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        r#"You are the planner for `buddy signoff` (a guarded assistant).
Read the activity context below (AI coding chats from roughly the last day).
Produce a JSON object ONLY (no markdown fences) with this schema:

{{
  "todos": [
    {{
      "id": "t1",
      "title": "short title",
      "detail": "what to do",
      "autonomy": "auto" | "needs_human",
      "confidence": 0.0,
      "needs_human_reason": null,
      "workspace": "/abs/path or null",
      "acceptance": "how to verify done",
      "evidence": ["short quotes or facts from context"]
    }}
  ]
}}

Rules for autonomy:
- "auto": clear goal, testable acceptance, workspace is a concrete absolute path from context, no product/legal/secret decisions, confidence >= 0.8
- "needs_human": ambiguous, missing path, multiple valid approaches, needs credentials/release/approval, or confidence < 0.8

Configured workspace trusts (unlisted paths default to {default_trust} when the agent runs):
{trust_hint}

--- CONTEXT ---
{context}
"#,
        default_trust = WorkspaceTrust::Yolo.as_str(),
        trust_hint = trust_hint,
        context = context_md
    )
}

pub fn parse_plan_json(raw: &str) -> Result<SignoffPlan, PlanError> {
    let trimmed = raw.trim();
    // Strip optional markdown fences
    let body = if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest
            .strip_prefix("json")
            .or_else(|| rest.strip_prefix("JSON"))
            .unwrap_or(rest);
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else {
        trimmed
    };
    // Find first { ... last }
    let start = body
        .find('{')
        .ok_or_else(|| PlanError::Parse("no JSON object found".into()))?;
    let end = body
        .rfind('}')
        .ok_or_else(|| PlanError::Parse("no JSON object end".into()))?;
    let slice = &body[start..=end];
    serde_json::from_str(slice).map_err(|e| PlanError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{Autonomy, parse_plan_json, plan_prompt};
    use crate::signoff::trust::{WorkspaceTrust, WorkspaceTrustEntry};
    use std::path::PathBuf;

    #[test]
    fn parses_plain_json() {
        let raw = r#"{"todos":[{"id":"t1","title":"fix tests","detail":"x","autonomy":"auto","confidence":0.9,"workspace":"/tmp/p","acceptance":"tests pass","evidence":["a"]}]}"#;
        let plan = parse_plan_json(raw).unwrap();
        assert_eq!(plan.todos.len(), 1);
        assert_eq!(plan.todos[0].autonomy, Autonomy::Auto);
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n{\"todos\":[]}\n```";
        let plan = parse_plan_json(raw).unwrap();
        assert!(plan.todos.is_empty());
    }

    #[test]
    fn prompt_mentions_default_yolo() {
        let p = plan_prompt("ctx", &[]);
        assert!(p.contains("defaults to trust=yolo"));
    }

    #[test]
    fn prompt_lists_configured_trusts() {
        let entries = [WorkspaceTrustEntry {
            path: PathBuf::from("/tmp/proj"),
            trust: WorkspaceTrust::WorkspaceWrite,
        }];
        let p = plan_prompt("ctx", &entries);
        assert!(p.contains("/tmp/proj"));
        assert!(p.contains("workspace-write"));
    }
}
