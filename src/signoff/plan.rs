//! LLM plan JSON for signoff todos.

use std::path::Path;

use serde::{Deserialize, Serialize};

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

/// Build the planning prompt. `allowed` workspaces are hints for the model.
pub fn plan_prompt(
    context_md: &str,
    allowed: &[impl AsRef<Path>],
    allow_all: bool,
) -> String {
    let allow = if allow_all {
        "(all workspaces allowed — any absolute project path from context is fine)".to_string()
    } else if allowed.is_empty() {
        "(none configured — mark all workspace-bound work as needs_human)".to_string()
    } else {
        allowed
            .iter()
            .map(|p| format!("- {}", p.as_ref().display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let auto_ws_rule = if allow_all {
        r#"- "auto": clear goal, testable acceptance, workspace is a concrete absolute path from context, no product/legal/secret decisions, confidence >= 0.8"#
    } else {
        r#"- "auto": clear goal, testable acceptance, workspace is one of the allowed paths below, no product/legal/secret decisions, confidence >= 0.8"#
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
{auto_ws_rule}
- "needs_human": ambiguous, missing path, multiple valid approaches, needs credentials/release/approval, or confidence < 0.8

Allowed workspaces:
{allow}

--- CONTEXT ---
{context}
"#,
        auto_ws_rule = auto_ws_rule,
        allow = allow,
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
    use std::path::Path;

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
    fn prompt_mentions_all_when_allow_all() {
        let p = plan_prompt("ctx", &[] as &[&Path], true);
        assert!(p.contains("all workspaces allowed"));
    }
}
