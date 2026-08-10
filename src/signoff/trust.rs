//! Per-workspace trust for signoff act (Codex sandbox flags).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::report::cli_backend::{CliSpec, preset};

/// How freely the act agent may touch a workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceTrust {
    /// Bypass approvals and sandbox (`--dangerously-bypass-approvals-and-sandbox`).
    #[default]
    Yolo,
    /// Writable cwd / workspace roots (`-s workspace-write`).
    WorkspaceWrite,
    /// Read-only sandbox (`-s read-only`).
    ReadOnly,
}

impl WorkspaceTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yolo => "yolo",
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustEntry {
    pub path: PathBuf,
    pub trust: WorkspaceTrust,
}

/// Preset for signoff act: inject Codex sandbox flags from workspace trust.
/// Non-codex CLIs keep the same args as [`preset`].
pub fn preset_for_act(name: &str, trust: WorkspaceTrust) -> Option<CliSpec> {
    match name {
        "codex" => {
            let mut args = vec!["exec".to_string()];
            match trust {
                WorkspaceTrust::Yolo => {
                    args.push("--dangerously-bypass-approvals-and-sandbox".into());
                }
                WorkspaceTrust::WorkspaceWrite => {
                    args.push("-s".into());
                    args.push("workspace-write".into());
                }
                WorkspaceTrust::ReadOnly => {
                    args.push("-s".into());
                    args.push("read-only".into());
                }
            }
            args.push("--skip-git-repo-check".into());
            args.push("-".into());
            Some(CliSpec {
                program: "codex".into(),
                args,
                stdin_prompt: true,
            })
        }
        other => preset(other),
    }
}

/// Longest configured prefix match; unlisted paths default to [`WorkspaceTrust::Yolo`].
pub fn trust_for(workspace: &Path, entries: &[WorkspaceTrustEntry]) -> WorkspaceTrust {
    let mut best: Option<(&WorkspaceTrustEntry, usize)> = None;
    for e in entries {
        if workspace == e.path.as_path() || workspace.starts_with(&e.path) {
            let len = e.path.as_os_str().len();
            if best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((e, len));
            }
        }
    }
    best.map(|(e, _)| e.trust).unwrap_or(WorkspaceTrust::Yolo)
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceTrust, WorkspaceTrustEntry, preset_for_act, trust_for};
    use crate::report::cli_backend::preset;
    use std::path::PathBuf;

    fn entry(path: &str, trust: WorkspaceTrust) -> WorkspaceTrustEntry {
        WorkspaceTrustEntry {
            path: PathBuf::from(path),
            trust,
        }
    }

    #[test]
    fn defaults_to_yolo_when_unlisted() {
        assert_eq!(
            trust_for(PathBuf::from("/tmp/anywhere").as_path(), &[]),
            WorkspaceTrust::Yolo
        );
    }

    #[test]
    fn exact_and_prefix_match() {
        let entries = vec![
            entry("/home/u/proj", WorkspaceTrust::WorkspaceWrite),
            entry("/home/u/proj/nested", WorkspaceTrust::ReadOnly),
        ];
        assert_eq!(
            trust_for(PathBuf::from("/home/u/proj").as_path(), &entries),
            WorkspaceTrust::WorkspaceWrite
        );
        assert_eq!(
            trust_for(PathBuf::from("/home/u/proj/nested/x").as_path(), &entries),
            WorkspaceTrust::ReadOnly
        );
    }

    #[test]
    fn preset_for_act_injects_codex_trust_flags() {
        let yolo = preset_for_act("codex", WorkspaceTrust::Yolo).unwrap();
        assert_eq!(
            yolo.args,
            vec![
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "-"
            ]
        );
        let ww = preset_for_act("codex", WorkspaceTrust::WorkspaceWrite).unwrap();
        assert_eq!(
            ww.args,
            vec!["exec", "-s", "workspace-write", "--skip-git-repo-check", "-"]
        );
        let ro = preset_for_act("codex", WorkspaceTrust::ReadOnly).unwrap();
        assert_eq!(
            ro.args,
            vec!["exec", "-s", "read-only", "--skip-git-repo-check", "-"]
        );
        assert_eq!(
            preset_for_act("claude", WorkspaceTrust::Yolo).unwrap(),
            preset("claude").unwrap()
        );
    }
}
