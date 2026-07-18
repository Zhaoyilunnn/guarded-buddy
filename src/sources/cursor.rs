//! Cursor data source: `~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl`.
//!
//! Each line is `{role, message.content[]}` (text / tool_use). No top-level timestamp:
//! user text embeds `<timestamp>Wednesday, Jul 15, 2026, 2:18 PM (UTC+8)</timestamp>`,
//! timestamp fallback chain = embedded timestamp → previous message timestamp → file mtime.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, FixedOffset, Local, NaiveDate, TimeZone};
use regex::Regex;
use walkdir::WalkDir;

use super::{HistorySource, SourceError, cap_ingest, compact_json_input};
use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};

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

    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut sessions = Vec::new();
        if !self.root.exists() {
            return sessions;
        }
        // layout: <slug>/agent-transcripts/<uuid>/<uuid>.jsonl (4th level from root)
        for entry in WalkDir::new(&self.root)
            .min_depth(4)
            .max_depth(4)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .is_none_or(|n| n != "agent-transcripts")
            {
                continue;
            }
            let project = path
                .strip_prefix(&self.root)
                .ok()
                .and_then(|rel| rel.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let fallback_ts = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(DateTime::from)
                .unwrap_or_else(|_| Local::now());

            let lines = match read_lines(path) {
                Ok(l) => l,
                Err(e) => {
                    warnings.push(e);
                    continue;
                }
            };
            let outcome = parse_transcript_lines(lines, project, fallback_ts);
            for line in outcome.bad_lines {
                warnings.push(SourceError::Parse {
                    path: path.to_path_buf(),
                    line,
                    msg: "invalid JSON line".to_string(),
                });
            }
            if let Some(mut session) = outcome.session {
                session.id = id;
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

/// Cursor embedded timestamp in the form "Wednesday, Jul 15, 2026, 2:18 PM (UTC+8)".
static TS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[A-Za-z]+, ([A-Za-z]{3}) (\d{1,2}), (\d{4}), (\d{1,2}):(\d{2}) ([AP]M) \(UTC([+-])(\d{1,2})(?::(\d{2}))?\)$",
    )
    .expect("valid regex")
});

/// Parse Cursor embedded timestamp → time with fixed offset. English month abbreviations, AM/PM,
/// `UTC±h[:mm]` offset.
pub(crate) fn parse_cursor_timestamp(s: &str) -> Option<DateTime<FixedOffset>> {
    let caps = TS_RE.captures(s.trim())?;
    let month = month_from_abbrev(caps.get(1)?.as_str())?;
    let day: u32 = caps.get(2)?.as_str().parse().ok()?;
    let year: i32 = caps.get(3)?.as_str().parse().ok()?;
    let hour12: u32 = caps.get(4)?.as_str().parse().ok()?;
    let minute: u32 = caps.get(5)?.as_str().parse().ok()?;
    let hour = match (caps.get(6)?.as_str(), hour12) {
        ("AM", 12) => 0,
        ("AM", h) if h < 12 => h,
        ("PM", 12) => 12,
        ("PM", h) if h < 12 => h + 12,
        _ => return None,
    };
    let off_h: i32 = caps.get(8)?.as_str().parse().ok()?;
    let off_m: i32 = match caps.get(9) {
        Some(m) => m.as_str().parse().ok()?,
        None => 0,
    };
    let secs = (off_h * 3600 + off_m * 60) * if caps.get(7)?.as_str() == "-" { -1 } else { 1 };
    let offset = FixedOffset::east_opt(secs)?;
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    offset.from_local_datetime(&naive).single()
}

fn month_from_abbrev(s: &str) -> Option<u32> {
    Some(match s {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

static WRAPPER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<timestamp>(.*?)</timestamp>\s*<user_query>\s*(.*?)\s*</user_query>")
        .expect("valid regex")
});

/// Strip `<timestamp>` and `<user_query>` wrappers from user text; returns (timestamp, query).
pub(crate) fn strip_user_wrapper(s: &str) -> Option<(DateTime<FixedOffset>, String)> {
    let caps = WRAPPER_RE.captures(s)?;
    let ts = parse_cursor_timestamp(caps.get(1)?.as_str())?;
    let query = caps.get(2)?.as_str().to_string();
    Some((ts, query))
}

pub(crate) struct ParseOutcome {
    pub session: Option<Session>,
    pub bad_lines: Vec<usize>,
}

/// Parse one transcript file. `fallback_ts` is used for messages with no timestamp info;
/// returned session.id is empty and filled by the caller (source) from the filename.
pub(crate) fn parse_transcript_lines<I: Iterator<Item = String>>(
    lines: I,
    project: String,
    fallback_ts: DateTime<Local>,
) -> ParseOutcome {
    let mut bad_lines = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    for (idx, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            bad_lines.push(idx + 1);
            continue;
        };
        let role = match record.get("role").and_then(|r| r.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let prev_ts = messages.last().map(|m: &Message| m.timestamp);
        let Some(content) = record
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    let Some(raw) = block.get("text").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if raw.trim().is_empty() {
                        continue;
                    }
                    let (ts, text) = if role == Role::User {
                        match strip_user_wrapper(raw) {
                            Some((ts, query)) => (ts.with_timezone(&Local), query),
                            None => (prev_ts.unwrap_or(fallback_ts), raw.to_string()),
                        }
                    } else {
                        (prev_ts.unwrap_or(fallback_ts), raw.to_string())
                    };
                    messages.push(Message {
                        role,
                        timestamp: ts,
                        content: MessageContent::Text(cap_ingest(&text)),
                    });
                }
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let summary = compact_json_input(block.get("input"));
                    messages.push(Message {
                        role,
                        timestamp: prev_ts.unwrap_or(fallback_ts),
                        content: MessageContent::ToolUse { name, summary },
                    });
                }
                _ => {}
            }
        }
    }

    if messages.is_empty() {
        return ParseOutcome {
            session: None,
            bad_lines,
        };
    }
    ParseOutcome {
        session: Some(Session {
            agent: AgentKind::Cursor,
            project,
            id: String::new(), // filled by source from filename
            started_at: messages[0].timestamp,
            messages,
        }),
        bad_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentKind, DateRange, MessageContent, Role};
    use chrono::{DateTime, FixedOffset, Local, NaiveDate, TimeZone};

    const SAMPLE: &str = include_str!("../../tests/fixtures/cursor/transcript-sample.jsonl");

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc(s: &str) -> DateTime<chrono::Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn mtime() -> DateTime<Local> {
        Local
            .from_local_datetime(&NaiveDate::parse_from_str("2026-07-15", "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap())
            .unwrap()
    }

    #[test]
    fn parse_cursor_timestamp_with_positive_offset() {
        let ts = parse_cursor_timestamp("Wednesday, Jul 15, 2026, 2:18 PM (UTC+8)").unwrap();
        assert_eq!(ts.offset(), &FixedOffset::east_opt(8 * 3600).unwrap());
        assert_eq!(ts.to_utc(), utc("2026-07-15T06:18:00Z"));
    }

    #[test]
    fn parse_cursor_timestamp_with_negative_offset() {
        let ts = parse_cursor_timestamp("Tuesday, Jul 14, 2026, 9:05 AM (UTC-4)").unwrap();
        assert_eq!(ts.offset(), &FixedOffset::west_opt(4 * 3600).unwrap());
        assert_eq!(ts.to_utc(), utc("2026-07-14T13:05:00Z"));
    }

    #[test]
    fn parse_cursor_timestamp_with_minutes_offset() {
        let ts = parse_cursor_timestamp("Wednesday, Jul 15, 2026, 8:00 PM (UTC+5:30)").unwrap();
        assert_eq!(ts.offset(), &FixedOffset::east_opt(5 * 3600 + 1800).unwrap());
        assert_eq!(ts.to_utc(), utc("2026-07-15T14:30:00Z"));
    }

    #[test]
    fn parse_cursor_timestamp_midnight_and_noon_edge() {
        let am = parse_cursor_timestamp("Monday, Jul 13, 2026, 12:00 AM (UTC+8)").unwrap();
        assert_eq!(am.to_utc(), utc("2026-07-12T16:00:00Z"));
        let pm = parse_cursor_timestamp("Monday, Jul 13, 2026, 12:00 PM (UTC+8)").unwrap();
        assert_eq!(pm.to_utc(), utc("2026-07-13T04:00:00Z"));
    }

    #[test]
    fn parse_cursor_timestamp_rejects_garbage() {
        assert!(parse_cursor_timestamp("not a timestamp").is_none());
        assert!(parse_cursor_timestamp("2026-07-15T14:18:00Z").is_none());
        assert!(parse_cursor_timestamp("Wednesday, Foo 15, 2026, 2:18 PM (UTC+8)").is_none());
    }

    #[test]
    fn strip_user_wrapper_extracts_query_and_strips_tags() {
        let raw = "<timestamp>Wednesday, Jul 15, 2026, 2:18 PM (UTC+8)</timestamp>\n<user_query>\n@README.md 审查这个文档\n</user_query>";
        let (ts, query) = strip_user_wrapper(raw).unwrap();
        assert_eq!(ts.to_utc(), utc("2026-07-15T06:18:00Z"));
        assert_eq!(query, "@README.md 审查这个文档");
    }

    #[test]
    fn strip_user_wrapper_returns_none_without_tags() {
        assert!(strip_user_wrapper("plain text").is_none());
    }

    #[test]
    fn parses_fixture_end_to_end() {
        let outcome = parse_transcript_lines(
            SAMPLE.lines().map(str::to_string),
            "-home-zhaoyilun-docs".to_string(),
            mtime(),
        );
        let session = outcome.session.expect("should parse");
        assert_eq!(session.agent, AgentKind::Cursor);
        assert_eq!(session.project, "-home-zhaoyilun-docs");
        assert_eq!(session.started_at.to_utc(), utc("2026-07-15T06:18:00Z"));

        let msgs = &session.messages;
        // user×2 + assistant text×3 + tool_use×2 = 7
        assert_eq!(msgs.len(), 7);

        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].timestamp.to_utc(), utc("2026-07-15T06:18:00Z"));
        assert!(msgs[0].text().unwrap().contains("审查一下这个文档"));
        assert!(!msgs[0].text().unwrap().contains("user_query"));

        // assistant has no embedded timestamp → inherit previous message timestamp
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].timestamp.to_utc(), utc("2026-07-15T06:18:00Z"));

        assert_eq!(msgs[4].role, Role::User);
        assert_eq!(msgs[4].timestamp.to_utc(), utc("2026-07-15T06:25:00Z"));
    }

    #[test]
    fn tool_use_rendered_as_compact_oneliner() {
        let outcome = parse_transcript_lines(
            SAMPLE.lines().map(str::to_string),
            "p".to_string(),
            mtime(),
        );
        let session = outcome.session.unwrap();
        let tools: Vec<_> = session
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::ToolUse { name, summary } => Some((name, summary)),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].0, "Read");
        assert!(tools[0].1.contains("README.md"));
        assert!(!tools[0].1.contains('\n'), "summary must be single-line");
        assert!(tools[0].1.len() <= 121); // 120 + ellipsis
    }

    #[test]
    fn user_message_without_wrapper_falls_back_to_mtime() {
        let lines = vec![
            "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"没有包装的用户消息\"}]}}".to_string(),
        ];
        let outcome = parse_transcript_lines(lines.into_iter(), "p".to_string(), mtime());
        let session = outcome.session.unwrap();
        assert_eq!(session.messages[0].timestamp, mtime());
        assert_eq!(session.messages[0].text().unwrap(), "没有包装的用户消息");
    }

    #[test]
    fn session_id_is_transcript_uuid() {
        let outcome = parse_transcript_lines(
            SAMPLE.lines().map(str::to_string),
            "p".to_string(),
            mtime(),
        );
        // no id at parse stage (source fills from filename); placeholder convention is empty string
        assert_eq!(outcome.session.unwrap().id, "");
    }

    #[test]
    fn source_collects_transcripts_and_fills_id_and_project() {
        let tmp = tempfile::tempdir().unwrap();
        let tdir = tmp.path().join(
            ".cursor/projects/-home-zhaoyilun-docs/agent-transcripts/b76effdc-7aea-4593-9b77-64611a6ad4cc",
        );
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(
            tdir.join("b76effdc-7aea-4593-9b77-64611a6ad4cc.jsonl"),
            SAMPLE,
        )
        .unwrap();

        let source = CursorSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "b76effdc-7aea-4593-9b77-64611a6ad4cc");
        assert_eq!(sessions[0].project, "-home-zhaoyilun-docs");
    }

    #[test]
    fn source_filters_sessions_outside_range() {
        let tmp = tempfile::tempdir().unwrap();
        let tdir = tmp.path().join(".cursor/projects/p/agent-transcripts/u1");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("u1.jsonl"), SAMPLE).unwrap();

        let source = CursorSource::new(tmp.path());
        let range = DateRange::new(d("2026-08-01"), d("2026-08-07")).unwrap();
        let mut warnings = Vec::new();
        assert!(source.collect(&range, &mut warnings).is_empty());
    }
}
