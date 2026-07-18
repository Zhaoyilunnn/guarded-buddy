//! Gemini 数据源（stub，待 TDD 实现）。

use std::path::{Path, PathBuf};

use super::{HistorySource, SourceError};
use crate::domain::{AgentKind, DateRange, Session};

pub struct GeminiSource {
    root: PathBuf,
}

impl GeminiSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".gemini"),
        }
    }
}

impl HistorySource for GeminiSource {
    fn kind(&self) -> AgentKind {
        AgentKind::Gemini
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn collect(&self, _range: &DateRange, _warnings: &mut Vec<SourceError>) -> Vec<Session> {
        Vec::new()
    }
}
