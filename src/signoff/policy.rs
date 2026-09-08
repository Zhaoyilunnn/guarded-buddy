//! Local autonomy gate after LLM planning.

use serde::Serialize;

use super::plan::{Autonomy, SignoffPlan, SignoffTodo};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GatedPlan {
    pub auto: Vec<SignoffTodo>,
    pub needs_human: Vec<SignoffTodo>,
    pub deferred: Vec<SignoffTodo>,
}

/// Apply hard gates: confidence, missing workspace, max auto count.
/// Workspace allowlisting was removed; trust only affects act CLI sandbox flags.
pub fn gate_plan(plan: SignoffPlan, min_confidence: f64, max_auto: usize) -> GatedPlan {
    let mut auto = Vec::new();
    let mut needs_human = Vec::new();
    let mut deferred = Vec::new();

    for mut todo in plan.todos {
        if matches!(todo.autonomy, Autonomy::NeedsHuman) {
            needs_human.push(todo);
            continue;
        }

        // Automatic candidates may be demoted.
        if todo.confidence < min_confidence {
            todo.autonomy = Autonomy::NeedsHuman;
            todo.needs_human_reason = Some(format!(
                "confidence {:.2} < {min_confidence}",
                todo.confidence
            ));
            needs_human.push(todo);
            continue;
        }
        if todo.workspace.is_none() {
            todo.autonomy = Autonomy::NeedsHuman;
            todo.needs_human_reason = Some("missing workspace".into());
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
    fn accepts_any_workspace_path() {
        let plan = SignoffPlan {
            todos: vec![todo("t1", true, 0.95, Some("/tmp/anywhere"))],
        };
        let gated = gate_plan(plan, 0.8, 3);
        assert_eq!(gated.auto.len(), 1);
        assert!(gated.needs_human.is_empty());
    }

    #[test]
    fn demotes_missing_workspace() {
        let plan = SignoffPlan {
            todos: vec![todo("t1", true, 0.95, None)],
        };
        let gated = gate_plan(plan, 0.8, 3);
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
        let gated = gate_plan(plan, 0.8, 2);
        assert_eq!(gated.auto.len(), 2);
        assert_eq!(gated.deferred.len(), 1);
    }
}
