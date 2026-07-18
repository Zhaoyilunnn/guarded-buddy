//! 数据源抽象：`HistorySource` trait 与四个 agent 的实现注册表。
//!
//! 开闭原则：新增一个 agent = 新增一个 `sources/<agent>.rs` 模块 + 在
//! `default_sources` 注册一行，无需修改既有代码。

use std::path::{Path, PathBuf};

use crate::domain::{AgentKind, DateRange, Session};

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod gemini;

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse error at {path}:{line}: {msg}")]
    Parse {
        path: PathBuf,
        line: usize,
        msg: String,
    },
}

/// 一个 agent 数据源。实现必须满足：
/// - 数据目录不存在时 `collect` 返回空 Vec 而非报错；
/// - 单行/单文件解析失败计入 `warnings`，绝不中断整体采集。
pub trait HistorySource: Send + Sync {
    fn kind(&self) -> AgentKind;

    /// 该源读取的根目录（用于 `sources` 子命令展示）。
    fn root(&self) -> &Path;

    /// 数据目录是否存在于本机。
    fn detect(&self) -> bool {
        self.root().exists()
    }

    /// 采集与 `range` 有交集的会话。
    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session>;
}

/// 默认注册表：Codex → Cursor → Claude → Gemini。
pub fn default_sources(home: &Path) -> Vec<Box<dyn HistorySource>> {
    vec![
        Box::new(codex::CodexSource::new(home)),
        Box::new(cursor::CursorSource::new(home)),
        Box::new(claude::ClaudeSource::new(home)),
        Box::new(gemini::GeminiSource::new(home)),
    ]
}

/// 采集阶段单条消息的字符上限（防止超大文件撑爆内存）；渲染阶段另有更小的展示上限。
pub(crate) const INGEST_CAP: usize = 64 * 1024;

/// 字符边界安全的采集截断。
pub(crate) fn cap_ingest(text: &str) -> String {
    if text.chars().count() <= INGEST_CAP {
        return text.to_string();
    }
    let cut: String = text.chars().take(INGEST_CAP).collect();
    format!("{cut}…[truncated at ingest]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sources_returns_four_in_order_with_roots_under_home() {
        let home = Path::new("/fake/home");
        let sources = default_sources(home);
        assert_eq!(sources.len(), 4);
        let kinds: Vec<AgentKind> = sources.iter().map(|s| s.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                AgentKind::Codex,
                AgentKind::Cursor,
                AgentKind::Claude,
                AgentKind::Gemini
            ]
        );
        for s in &sources {
            assert!(s.root().starts_with(home), "root not under home: {:?}", s.root());
        }
    }

    #[test]
    fn collect_on_missing_root_returns_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = default_sources(tmp.path());
        let range = DateRange::last_n_days(7, chrono::Local::now().date_naive());
        for s in &sources {
            let mut warnings = Vec::new();
            let sessions = s.collect(&range, &mut warnings);
            assert!(sessions.is_empty(), "{} should be empty", s.kind().slug());
        }
    }

    #[test]
    fn cap_ingest_keeps_short_text() {
        assert_eq!(cap_ingest("短文本"), "短文本");
    }

    #[test]
    fn cap_ingest_truncates_long_text_at_char_boundary() {
        let long: String = "汉".repeat(INGEST_CAP + 100);
        let out = cap_ingest(&long);
        assert!(out.ends_with("…[truncated at ingest]"));
        assert!(out.chars().count() > INGEST_CAP); // cap + marker
    }
}
