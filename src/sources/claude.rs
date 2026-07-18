//! Claude Code 数据源（stub，待 TDD 实现）。

use std::path::{Path, PathBuf};

use super::{HistorySource, SourceError};
use crate::domain::{AgentKind, DateRange, Session};

pub struct ClaudeSource {
    root: PathBuf,
}

impl ClaudeSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".claude").join("projects"),
        }
    }
}

impl HistorySource for ClaudeSource {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn collect(&self, _range: &DateRange, _warnings: &mut Vec<SourceError>) -> Vec<Session> {
        Vec::new()
    }
}
