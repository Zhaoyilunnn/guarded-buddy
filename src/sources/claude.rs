//! Claude Code data source: `~/.claude/projects/<slug>/<session-uuid>.jsonl`.
//!
//! Consumes only lines with `type == "user" | "assistant"`: text blocks become Text messages,
//! `tool_use` blocks become ToolUse messages (input compressed to one line); skips system / summary /
//! thinking / tool_result / isMeta etc.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use walkdir::WalkDir;

use super::{HistorySource, SourceError, cap_ingest, compact_json_input};
use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};

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

    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut sessions = Vec::new();
        if !self.root.exists() {
            return sessions;
        }
        for entry in WalkDir::new(&self.root)
            .min_depth(2)
            .max_depth(2)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let fallback_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let lines = match read_lines(path) {
                Ok(l) => l,
                Err(e) => {
                    warnings.push(e);
                    continue;
                }
            };
            let outcome = parse_session_lines(lines, &fallback_id);
            for line in outcome.bad_lines {
                warnings.push(SourceError::Parse {
                    path: path.to_path_buf(),
                    line,
                    msg: "invalid JSON line".to_string(),
                });
            }
            if let Some(session) = outcome.session {
                // coarse session filter by started_at (message-level day filter happens in collect.rs)
                if range.contains_ts(&session.started_at)
                    || session.messages.iter().any(|m| range.contains_ts(&m.timestamp))
                {
                    sessions.push(session);
                }
            }
        }
        sessions.sort_by_key(|s| s.started_at);
        sessions
    }
}

fn read_lines(path: &Path) -> Result<impl Iterator<Item = String>, SourceError> {
    let file = File::open(path).map_err(|source| SourceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(BufReader::new(file).lines().map_while(Result::ok))
}

pub(crate) struct ParseOutcome {
    pub session: Option<Session>,
    pub bad_lines: Vec<usize>,
}

pub(crate) fn parse_session_lines<I: Iterator<Item = String>>(
    lines: I,
    fallback_id: &str,
) -> ParseOutcome {
    let mut bad_lines = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;

    for (idx, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            bad_lines.push(idx + 1);
            continue;
        };
        if record.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(kind) = record.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let Some(ts) = record
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_iso)
        else {
            continue;
        };
        session_id = session_id.or_else(|| {
            record
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
        cwd = cwd.or_else(|| {
            record
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

        let role = if kind == "user" {
            Role::User
        } else {
            Role::Assistant
        };
        let Some(message) = record.get("message") else {
            continue;
        };
        let Some(content) = message.get("content") else {
            continue;
        };

        match content {
            serde_json::Value::String(text) => {
                push_text(&mut messages, role, ts, text);
            }
            serde_json::Value::Array(blocks) => {
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                push_text(&mut messages, role, ts, text);
                            }
                        }
                        Some("tool_use") if role == Role::Assistant => {
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let summary = compact_json_input(block.get("input"));
                            messages.push(Message {
                                role,
                                timestamp: ts,
                                content: MessageContent::ToolUse { name, summary },
                            });
                        }
                        // thinking / tool_result / other block types skipped
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    messages.sort_by_key(|m| m.timestamp);
    if messages.is_empty() {
        return ParseOutcome {
            session: None,
            bad_lines,
        };
    }
    ParseOutcome {
        session: Some(Session {
            agent: AgentKind::Claude,
            project: cwd.unwrap_or_else(|| "unknown".to_string()),
            id: session_id.unwrap_or_else(|| fallback_id.to_string()),
            started_at: messages[0].timestamp,
            messages,
        }),
        bad_lines,
    }
}

fn push_text(messages: &mut Vec<Message>, role: Role, ts: DateTime<Local>, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    messages.push(Message {
        role,
        timestamp: ts,
        content: MessageContent::Text(cap_ingest(text)),
    });
}

/// ISO8601/RFC3339 (UTC) → local timezone.
fn parse_iso(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentKind, DateRange, MessageContent, Role};
    use chrono::{DateTime, NaiveDate};

    const SAMPLE: &str = include_str!("../../tests/fixtures/claude/session-sample.jsonl");

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc(s: &str) -> DateTime<chrono::Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn uses_session_id_and_cwd_fields() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "fallback");
        let session = outcome.session.expect("should parse");
        assert_eq!(session.agent, AgentKind::Claude);
        assert_eq!(session.id, "a1b2c3d4-3333-4333-8333-abcdefabcdef");
        assert_eq!(session.project, "/home/zhaoyilun/notes-app");
        assert_eq!(session.started_at.to_utc(), utc("2026-07-16T01:01:00Z"));
    }

    #[test]
    fn extracts_string_content_user_message() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let first = &session.messages[0];
        assert_eq!(first.role, Role::User);
        assert_eq!(first.text().unwrap(), "帮我给笔记应用加一个全文搜索功能");
        assert_eq!(first.timestamp.to_utc(), utc("2026-07-16T01:01:00Z"));
    }

    #[test]
    fn extracts_text_blocks_skips_thinking() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let assistant_texts: Vec<&str> = session
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .filter_map(|m| m.text())
            .collect();
        assert!(assistant_texts.iter().any(|t| t.contains("FTS5")));
        assert!(!assistant_texts.iter().any(|t| t.contains("search implementations")));
    }

    #[test]
    fn tool_use_becomes_tooluse_message_with_summary() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let tool = session
            .messages
            .iter()
            .find(|m| matches!(m.content, MessageContent::ToolUse { .. }))
            .expect("should have a tool use message");
        match &tool.content {
            MessageContent::ToolUse { name, summary } => {
                assert_eq!(name, "Read");
                assert!(summary.contains("db.rs"));
                assert!(summary.len() <= 120);
            }
            _ => unreachable!(),
        }
        assert_eq!(tool.timestamp.to_utc(), utc("2026-07-16T01:01:25Z"));
    }

    #[test]
    fn tool_result_only_user_line_yields_no_message() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        assert!(
            !session
                .messages
                .iter()
                .any(|m| m.text().is_some_and(|t| t.contains("pub struct Db"))),
            "tool_result content must not become a message"
        );
    }

    #[test]
    fn skips_system_summary_and_mode_lines() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        assert!(!session
            .messages
            .iter()
            .any(|m| m.text().is_some_and(|t| t.contains("Auto mode"))));
        // summary line content must not become a standalone message
        assert!(!session
            .messages
            .iter()
            .any(|m| m.text() == Some("为笔记应用添加全文搜索")));
        // message total: user×2 + assistant text×2 + tool_use×1 = 5
        assert_eq!(session.messages.len(), 5);
    }

    #[test]
    fn messages_sorted_by_timestamp() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let mut sorted = session.messages.clone();
        sorted.sort_by_key(|m| m.timestamp);
        let orig: Vec<_> = session.messages.iter().map(|m| m.timestamp).collect();
        let new: Vec<_> = sorted.iter().map(|m| m.timestamp).collect();
        assert_eq!(orig, new);
    }

    #[test]
    fn source_collects_sessions_from_project_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude/projects/-home-zhaoyilun-notes-app");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a1b2c3d4.jsonl"), SAMPLE).unwrap();

        let source = ClaudeSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, AgentKind::Claude);
    }

    #[test]
    fn source_filters_sessions_outside_range() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join(".claude/projects/p");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s.jsonl"), SAMPLE).unwrap();

        let source = ClaudeSource::new(tmp.path());
        let range = DateRange::new(d("2026-08-01"), d("2026-08-07")).unwrap();
        let mut warnings = Vec::new();
        assert!(source.collect(&range, &mut warnings).is_empty());
    }
}
