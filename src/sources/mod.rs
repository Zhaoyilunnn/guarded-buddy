//! Data source abstraction: `HistorySource` trait and registry of four agent implementations.
//!
//! Open/closed principle: add a new agent = add a `sources/<agent>.rs` module +
//! one line in `default_sources`, without modifying existing code.

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

/// One agent data source. Implementations must satisfy:
/// - when the data directory is missing, `collect` returns an empty Vec rather than an error;
/// - single-line/file parse failures go into `warnings` and never abort collection.
pub trait HistorySource: Send + Sync {
    fn kind(&self) -> AgentKind;

    /// Root directory this source reads from (shown by the `sources` subcommand).
    fn root(&self) -> &Path;

    /// Whether the data directory exists on this machine.
    fn detect(&self) -> bool {
        self.root().exists()
    }

    /// Collect sessions that overlap `range`.
    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session>;
}

/// Default registry order: Codex, Cursor, Claude, then Gemini.
pub fn default_sources(home: &Path) -> Vec<Box<dyn HistorySource>> {
    default_sources_opts(home, false)
}

/// Registry with options: `include_prompt_history` is forwarded to the Gemini source.
pub fn default_sources_opts(
    home: &Path,
    include_prompt_history: bool,
) -> Vec<Box<dyn HistorySource>> {
    vec![
        Box::new(codex::CodexSource::new(home)),
        Box::new(cursor::CursorSource::new(home)),
        Box::new(claude::ClaudeSource::new(home)),
        Box::new(gemini::GeminiSource::new(home).include_prompt_history(include_prompt_history)),
    ]
}

/// Per-message character cap during ingestion (prevents huge files from exhausting memory); render stage has a smaller display cap.
pub(crate) const INGEST_CAP: usize = 64 * 1024;

/// Character-boundary-safe ingestion truncation.
pub(crate) fn cap_ingest(text: &str) -> String {
    if text.chars().count() <= INGEST_CAP {
        return text.to_string();
    }
    let cut: String = text.chars().take(INGEST_CAP).collect();
    format!("{cut}…[truncated at ingest]")
}

/// Compress tool input to a single-line JSON string, capped at 120 chars (ellipsis when longer).
pub(crate) fn compact_json_input(value: Option<&serde_json::Value>) -> String {
    const MAX: usize = 120;
    let raw = value
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    let mut out: String = raw.chars().take(MAX).collect();
    if raw.chars().count() > MAX {
        out.push('…');
    }
    out
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

    #[test]
    fn compact_json_input_produces_single_line_capped_output() {
        let v = serde_json::json!({"path": "/a/b.rs", "note": "line1\nline2"});
        let out = compact_json_input(Some(&v));
        assert!(!out.contains('\n'));
        assert!(out.contains("/a/b.rs"));

        let long = serde_json::json!({"data": "x".repeat(500)});
        let out = compact_json_input(Some(&long));
        assert!(out.chars().count() == 121);
        assert!(out.ends_with('…'));

        assert_eq!(compact_json_input(None), "");
    }
}
