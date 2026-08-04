//! Local autonomy gate after LLM planning.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::plan::{Autonomy, SignoffPlan, SignoffTodo};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GatedPlan {
    pub auto: Vec<SignoffTodo>,
    pub needs_human: Vec<SignoffTodo>,
    pub deferred: Vec<SignoffTodo>,
}

fn workspace_allowed(ws: &str, allowed: &[PathBuf]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let path = Path::new(ws);
    allowed.iter().any(|a| path == a.as_path() || path.starts_with(a))
}

/// Apply hard gates: confidence, allowlist, max auto count.
pub fn gate_plan(
    plan: SignoffPlan,
    allowed: &[PathBuf],
    min_confidence: f64,
    max_auto: usize,
) -> GatedPlan {
    let mut auto = Vec::new();
    let mut needs_human = Vec::new();
    let mut deferred = Vec::new();

    for mut todo in plan.todos {
        if matches!(todo.autonomy, Autonomy::NeedsHuman) {
            needs_human.push(todo);
            continue;
        }

        // Auto candidates — may demote.
        if todo.confidence < min_confidence {
            todo.autonomy = Autonomy::NeedsHuman;
            todo.needs_human_reason = Some(format!(
                "confidence {:.2} < {min_confidence}",
                todo.confidence
            ));
            needs_human.push(todo);
            continue;
        }
        let Some(ws) = todo.workspace.clone() else {
            todo.autonomy = Autonomy::NeedsHuman;
            todo.needs_human_reason = Some("missing workspace".into());
            needs_human.push(todo);
            continue;
        };
        if !workspace_allowed(&ws, allowed) {
            todo.autonomy = Autonomy::NeedsHuman;
            todo.needs_human_reason = Some("workspace not in allowed_workspaces".into());
            needs_human.push(todo);
            continue;
        }
        if auto.len() >= max_auto {
            deferred.push(todo);
            continue;
        }
        auto.push(todo);
    }

    GatedPlan {
        auto,
        needs_human,
        deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::gate_plan;
    use crate::signoff::plan::{Autonomy, SignoffPlan, SignoffTodo};
    use std::path::PathBuf;

    fn todo(id: &str, auto: bool, conf: f64, ws: Option<&str>) -> SignoffTodo {
        SignoffTodo {
            id: id.into(),
            title: id.into(),
            detail: String::new(),
            autonomy: if auto {
                Autonomy::Auto
            } else {
                Autonomy::NeedsHuman
            },
            confidence: conf,
            needs_human_reason: None,
            workspace: ws.map(str::to_string),
            acceptance: String::new(),
            evidence: vec![],
        }
    }

    #[test]
    fn demotes_outside_allowlist() {
        let plan = SignoffPlan {
            todos: vec![todo("t1", true, 0.95, Some("/tmp/other"))],
        };
        let gated = gate_plan(plan, &[PathBuf::from("/tmp/allowed")], 0.8, 3);
        assert!(gated.auto.is_empty());
        assert_eq!(gated.needs_human.len(), 1);
    }

    #[test]
    fn caps_auto_todos() {
        let plan = SignoffPlan {
            todos: vec![
                todo("t1", true, 0.9, Some("/tmp/p")),
                todo("t2", true, 0.9, Some("/tmp/p")),
                todo("t3", true, 0.9, Some("/tmp/p")),
            ],
        };
        let gated = gate_plan(plan, &[PathBuf::from("/tmp/p")], 0.8, 2);
        assert_eq!(gated.auto.len(), 2);
        assert_eq!(gated.deferred.len(), 1);
    }
}
