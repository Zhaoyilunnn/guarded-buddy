//! Codex CLI data source: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Deduplication strategy: consume only `user_message` / `agent_message` inside `type == "event_msg"`,
//! completely ignore `response_item` (its messages duplicate event_msg).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local};

use super::{HistorySource, SourceError, cap_ingest};
use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};

pub struct CodexSource {
    root: PathBuf,
}

impl CodexSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".codex").join("sessions"),
        }
    }
}

impl HistorySource for CodexSource {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut sessions = Vec::new();
        for file in rollout_files_in_range(&self.root, range) {
            let fallback_id = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let lines = match read_lines(&file) {
                Ok(l) => l,
                Err(e) => {
                    warnings.push(e);
                    continue;
                }
            };
            let outcome = parse_session_lines(lines, &fallback_id);
            for line in outcome.bad_lines {
                warnings.push(SourceError::Parse {
                    path: file.clone(),
                    line,
                    msg: "invalid JSON line".to_string(),
                });
            }
            if let Some(session) = outcome.session {
                sessions.push(session);
            }
        }
        sessions.sort_by_key(|s| s.started_at);
        sessions
    }
}

/// Pre-filter by `YYYY/MM/DD` directory names; enumerate only date dirs intersecting the range (avoids full scan).
fn rollout_files_in_range(root: &Path, range: &DateRange) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for day in range.days() {
        let dir = root
            .join(format!("{:04}", day.year()))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn read_lines(path: &Path) -> Result<impl Iterator<Item = String>, SourceError> {
    let file = File::open(path).map_err(|source| SourceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(BufReader::new(file).lines().map_while(Result::ok))
}

/// Parse result: session (None when no valid messages) + bad line numbers (1-based).
pub(crate) struct ParseOutcome {
    pub session: Option<Session>,
    pub bad_lines: Vec<usize>,
}

pub(crate) fn parse_session_lines<I: Iterator<Item = String>>(
    lines: I,
    fallback_id: &str,
) -> ParseOutcome {
    let mut bad_lines = Vec::new();
    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at: Option<DateTime<Local>> = None;

    for (idx, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            bad_lines.push(idx + 1);
            continue;
        };

        match record.get("type").and_then(|t| t.as_str()) {
            Some("session_meta") => {
                let payload = record.get("payload").unwrap_or(&serde_json::Value::Null);
                session_id = payload
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or(session_id);
                cwd = payload
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or(cwd);
                if started_at.is_none() {
                    started_at = payload
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .and_then(parse_iso)
                        .or(started_at);
                }
            }
            Some("event_msg") => {
                let payload = record.get("payload").unwrap_or(&serde_json::Value::Null);
                let role = match payload.get("type").and_then(|t| t.as_str()) {
                    Some("user_message") => Role::User,
                    Some("agent_message") => Role::Assistant,
                    _ => continue,
                };
                let Some(text) = payload.get("message").and_then(|m| m.as_str()) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                let Some(ts) = record
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .and_then(parse_iso)
                else {
                    continue;
                };
                messages.push(Message {
                    role,
                    timestamp: ts,
                    content: MessageContent::Text(cap_ingest(text)),
                });
            }
            // response_item / turn_context / world_state / token_count etc. are ignored
            _ => {}
        }
    }

    if messages.is_empty() {
        return ParseOutcome {
            session: None,
            bad_lines,
        };
    }

    let started_at = started_at.unwrap_or(messages[0].timestamp);
    ParseOutcome {
        session: Some(Session {
            agent: AgentKind::Codex,
            project: cwd.unwrap_or_else(|| "unknown".to_string()),
            id: session_id.unwrap_or_else(|| fallback_id.to_string()),
            started_at,
            messages,
        }),
        bad_lines,
    }
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
    use crate::domain::{AgentKind, DateRange, Role};
    use chrono::{DateTime, NaiveDate};

    const SAMPLE: &str = include_str!("../../tests/fixtures/codex/rollout-sample.jsonl");
    const MALFORMED: &str = include_str!("../../tests/fixtures/codex/rollout-malformed.jsonl");

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc(s: &str) -> DateTime<chrono::Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn parses_session_meta_project_and_id() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "fallback-id");
        let session = outcome.session.expect("session should parse");
        assert_eq!(session.agent, AgentKind::Codex);
        assert_eq!(session.id, "0f3a2b7c-1111-4111-8111-abcdefabcdef");
        assert_eq!(session.project, "/home/zhaoyilun/shop-api");
        assert_eq!(session.started_at.to_utc(), utc("2026-07-15T02:04:58Z"));
    }

    #[test]
    fn extracts_user_and_agent_messages_in_order_with_timestamps() {
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let msgs = &session.messages;
        assert_eq!(msgs.len(), 4);

        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].timestamp.to_utc(), utc("2026-07-15T02:05:02Z"));
        assert_eq!(
            msgs[0].text().unwrap(),
            "帮我修复登录接口在 token 过期后返回 500 的问题"
        );

        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].timestamp.to_utc(), utc("2026-07-15T02:05:12Z"));
        assert_eq!(msgs[1].text().unwrap(), "我先定位登录接口的鉴权中间件。");

        assert_eq!(msgs[2].role, Role::User);
        assert_eq!(msgs[2].text().unwrap(), "顺便补一个单元测试");

        assert_eq!(msgs[3].role, Role::Assistant);
        assert!(msgs[3].text().unwrap().contains("已修复"));
    }

    #[test]
    fn ignores_response_item_and_world_state_lines() {
        // if response_item were parsed, message count would double (fixture has a copy for each event_msg)
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let texts: Vec<&str> = session.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts.iter().filter(|t| **t == "我先定位登录接口的鉴权中间件。").count(),
            1,
            "response_item duplicates must not be ingested"
        );
        assert!(!texts.iter().any(|t| t.contains("environment_context")));
        assert!(!texts.iter().any(|t| t.contains("permissions instructions")));
    }

    #[test]
    fn malformed_line_is_skipped_and_counted() {
        let outcome = parse_session_lines(MALFORMED.lines().map(str::to_string), "x");
        assert_eq!(outcome.bad_lines, vec![3]);
        let session = outcome.session.unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text().unwrap(), "第一问");
    }

    #[test]
    fn session_without_event_msgs_returns_none() {
        let lines = vec![
            "{\"timestamp\":\"2026-07-15T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"cwd\":\"/p\"}}".to_string(),
            "{\"timestamp\":\"2026-07-15T00:00:01Z\",\"type\":\"turn_context\",\"payload\":{}}".to_string(),
        ];
        let outcome = parse_session_lines(lines.into_iter(), "x");
        assert!(outcome.session.is_none());
    }

    #[test]
    fn source_collect_filters_sessions_outside_range() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join(".codex/sessions");
        // in range: 2026-07-15; out of range: 2026-07-01 and 2026-08-01
        for day in ["2026/07/15", "2026/07/01", "2026/08/01"] {
            let dir = sessions_dir.join(day);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("rollout-x.jsonl"), SAMPLE).unwrap();
        }

        let source = CodexSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project, "/home/zhaoyilun/shop-api");
    }

    #[test]
    fn source_root_is_codex_sessions_dir() {
        let source = CodexSource::new(std::path::Path::new("/home/u"));
        assert_eq!(
            source.root(),
            std::path::Path::new("/home/u/.codex/sessions")
        );
        assert_eq!(source.kind(), AgentKind::Codex);
    }
}
