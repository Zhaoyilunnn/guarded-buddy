//! Cursor 数据源（stub，待 TDD 实现）。

use std::path::{Path, PathBuf};

use super::{HistorySource, SourceError};
use crate::domain::{AgentKind, DateRange, Session};

pub struct CursorSource {
    root: PathBuf,
}

impl CursorSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".cursor").join("projects"),
        }
    }
}

impl HistorySource for CursorSource {
    fn kind(&self) -> AgentKind {
        AgentKind::Cursor
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn collect(&self, _range: &DateRange, _warnings: &mut Vec<SourceError>) -> Vec<Session> {
        Vec::new()
    }
}
