//! Codex CLI data source: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Message extraction (newer CLIs dropped chat from `event_msg`):
//! 1. Prefer `user_message` and `agent_message` records within `event_msg` (legacy, deduplicated against `response_item`).
//! 2. If none exist, fall back to `message` records within `response_item` with a user or assistant role
//!    (skip developer, injected context, and collab `agent_message` items).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use walkdir::WalkDir;

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
        for file in rollout_files_in_range(&self.root, range, warnings) {
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

/// Scan all date directories, skipping only files modified before the first local day.
/// Message timestamps are filtered later by the collector; mtime has no upper bound.
fn rollout_files_in_range(
    root: &Path,
    range: &DateRange,
    warnings: &mut Vec<SourceError>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if error.depth() == 0
                    && error
                        .io_error()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    continue;
                }
                warnings.push(SourceError::Io {
                    path: error.path().unwrap_or(root).to_path_buf(),
                    source: std::io::Error::other(error),
                });
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_file() || !path.extension().is_some_and(|e| e == "jsonl") {
            continue;
        }
        let modified = std::fs::metadata(path).and_then(|metadata| metadata.modified());
        if modified_since_start(path, modified, range, warnings) {
            files.push(path.to_path_buf());
        }
    }
    files.sort();
    files
}

fn modified_since_start(
    path: &Path,
    modified: std::io::Result<std::time::SystemTime>,
    range: &DateRange,
    warnings: &mut Vec<SourceError>,
) -> bool {
    match modified {
        // Comparing local dates avoids assuming that every timezone has an unambiguous midnight.
        Ok(time) => DateTime::<Local>::from(time).date_naive() >= range.start,
        Err(source) => {
            warnings.push(SourceError::Io {
                path: path.to_path_buf(),
                source,
            });
            true
        }
    }
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
    let mut from_event = Vec::new();
    let mut from_response = Vec::new();
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at: Option<DateTime<Local>> = None;

    for (idx, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
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
                if let Some(msg) = message_from_event_msg(&record) {
                    from_event.push(msg);
                }
            }
            Some("response_item") => {
                if let Some(msg) = message_from_response_item(&record) {
                    from_response.push(msg);
                }
            }
            // turn_context / world_state / token_count etc. are ignored
            _ => {}
        }
    }

    // Legacy rollouts duplicate chat in response_item; prefer event_msg when present.
    let messages = if !from_event.is_empty() {
        from_event
    } else {
        from_response
    };

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

fn message_from_event_msg(record: &serde_json::Value) -> Option<Message> {
    let payload = record.get("payload")?;
    let role = match payload.get("type").and_then(|t| t.as_str()) {
        Some("user_message") => Role::User,
        Some("agent_message") => Role::Assistant,
        _ => return None,
    };
    let text = payload.get("message").and_then(|m| m.as_str())?;
    if text.trim().is_empty() {
        return None;
    }
    let ts = record
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_iso)?;
    Some(Message {
        role,
        timestamp: ts,
        content: MessageContent::Text(cap_ingest(text)),
    })
}

fn message_from_response_item(record: &serde_json::Value) -> Option<Message> {
    let payload = record.get("payload")?;
    // Collaboration and subagent envelopes are not part of the primary conversation.
    if payload.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
        return None;
    }
    if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    let role = match payload.get("role").and_then(|r| r.as_str()) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None, // developer / system / etc.
    };
    let text = join_message_content(payload.get("content")?)?;
    if text.trim().is_empty() {
        return None;
    }
    if role == Role::User && is_injected_user_context(&text) {
        return None;
    }
    let ts = record
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_iso)?;
    Some(Message {
        role,
        timestamp: ts,
        content: MessageContent::Text(cap_ingest(&text)),
    })
}

fn join_message_content(content: &serde_json::Value) -> Option<String> {
    let parts = content.as_array()?;
    let mut out = String::new();
    for part in parts {
        let kind = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if !matches!(kind, "input_text" | "output_text" | "text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Drop automatic context blocks that newer Codex injects as `role=user` messages.
fn is_injected_user_context(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<environment_context>")
        || t.starts_with("<permissions instructions>")
        || t.starts_with("# AGENTS.md instructions")
        || t.starts_with("<INSTRUCTIONS>")
}

/// Convert an ISO 8601/RFC 3339 UTC timestamp to the local time zone.
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
    const SAMPLE_V2: &str =
        include_str!("../../tests/fixtures/codex/rollout-v2-response-item-only.jsonl");
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
    fn ignores_response_item_when_event_msg_present() {
        // if response_item were also parsed, message count would double
        let outcome = parse_session_lines(SAMPLE.lines().map(str::to_string), "x");
        let session = outcome.session.unwrap();
        let texts: Vec<&str> = session.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts
                .iter()
                .filter(|t| **t == "我先定位登录接口的鉴权中间件。")
                .count(),
            1,
            "response_item duplicates must not be ingested when event_msg exists"
        );
        assert!(!texts.iter().any(|t| t.contains("environment_context")));
        assert!(!texts.iter().any(|t| t.contains("permissions instructions")));
    }

    #[test]
    fn v2_response_item_only_rollout_extracts_chat() {
        let outcome = parse_session_lines(SAMPLE_V2.lines().map(str::to_string), "fallback");
        let session = outcome.session.expect("v2 session should parse");
        assert_eq!(session.id, "019fe58e-aaaa-bbbb-cccc-ddddeeeeffff");
        assert_eq!(session.project, "/mnt/d/Research/XQ");
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, Role::User);
        assert!(
            session.messages[0]
                .text()
                .unwrap()
                .contains("09-future-supercomputer.html")
        );
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert!(
            session.messages[1]
                .text()
                .unwrap()
                .contains("先读当前愿景页")
        );
        assert_eq!(session.messages[2].role, Role::Assistant);
        assert!(
            session.messages[2]
                .text()
                .unwrap()
                .contains("已完成路径对比小修")
        );

        let texts: Vec<&str> = session.messages.iter().filter_map(|m| m.text()).collect();
        assert!(!texts.iter().any(|t| t.contains("environment_context")));
        assert!(!texts.iter().any(|t| t.contains("AGENTS.md instructions")));
        assert!(!texts.iter().any(|t| t.contains("Subagent noise")));
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
    fn session_without_chat_messages_returns_none() {
        let lines = vec![
            "{\"timestamp\":\"2026-07-15T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"cwd\":\"/p\"}}".to_string(),
            "{\"timestamp\":\"2026-07-15T00:00:01Z\",\"type\":\"turn_context\",\"payload\":{}}".to_string(),
            "{\"timestamp\":\"2026-07-15T00:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}".to_string(),
        ];
        let outcome = parse_session_lines(lines.into_iter(), "x");
        assert!(outcome.session.is_none());
    }

    #[test]
    fn source_collect_uses_mtime_not_directory_date() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join(".codex/sessions");
        // Both older and newer directory names must remain eligible.
        for day in ["2026/07/15", "2026/07/01", "2026/08/01"] {
            let dir = sessions_dir.join(day);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("rollout-x.jsonl");
            std::fs::write(&path, SAMPLE).unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(utc("2026-08-02T12:00:00Z").into())
                .unwrap();
        }

        let source = CodexSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].project, "/home/zhaoyilun/shop-api");
        assert!(warnings.is_empty());
        let buckets =
            crate::collect::group_by_day(crate::collect::merge_sessions(sessions), &range);
        assert!(!buckets.is_empty());
        assert!(buckets.iter().all(|bucket| range.contains_day(bucket.date)));
    }

    #[test]
    fn mtime_cutoff_is_inclusive_local_midnight_without_upper_bound() {
        use chrono::TimeZone;
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let start = Local
            .from_local_datetime(&range.start.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .unwrap();
        let mut warnings = Vec::new();
        for (time, expected) in [
            (start - chrono::Duration::seconds(1), false),
            (start, true),
            (start + chrono::Duration::days(30), true),
        ] {
            assert_eq!(
                modified_since_start(
                    Path::new("test.jsonl"),
                    Ok(time.into()),
                    &range,
                    &mut warnings
                ),
                expected
            );
        }
        assert!(warnings.is_empty());
    }

    #[test]
    fn unavailable_mtime_warns_and_keeps_file() {
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        assert!(modified_since_start(
            Path::new("test.jsonl"),
            Err(std::io::Error::other("mtime unavailable")),
            &range,
            &mut warnings
        ));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn old_mtime_is_skipped_even_in_recent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".codex/sessions/2026/07/15");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, SAMPLE).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(utc("2026-07-01T12:00:00Z").into())
            .unwrap();
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        assert!(
            CodexSource::new(tmp.path())
                .collect(&range, &mut warnings)
                .is_empty()
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn missing_root_is_empty_without_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        assert!(
            CodexSource::new(tmp.path())
                .collect(&range, &mut warnings)
                .is_empty()
        );
        assert!(warnings.is_empty());
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
