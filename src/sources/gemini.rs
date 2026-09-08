//! Gemini data source (dual layout merge):
//!
//! - classic: `~/.gemini/tmp/<slug>/chats/session-*.json` (single JSON document,
//!   `messages[]` with `type: user|gemini`, content as `{text}` parts,
//!   assistant may include `toolCalls`); project name from `<slug>/.project_root` (first line).
//! - agy/Antigravity: `~/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs/transcript.jsonl`
//!   (`USER_INPUT` strips `<USER_REQUEST>`/`<ADDITIONAL_METADATA>` wrappers;
//!   `PLANNER_RESPONSE` content becomes assistant text and tool calls become tool use;
//!   VIEW_FILE and other tool results plus SYSTEM records are skipped).
//! - `antigravity-cli/history.jsonl` (user prompts only, millisecond timestamps, grouped by workspace)
//!   duplicates transcript content, excluded by default; included when `include_prompt_history` is enabled.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Local};
use regex::Regex;
use walkdir::WalkDir;

use super::{HistorySource, SourceError, cap_ingest, compact_json_input};
use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};

pub struct GeminiSource {
    root: PathBuf,
    include_prompt_history: bool,
}

impl GeminiSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".gemini"),
            include_prompt_history: false,
        }
    }

    /// Whether to include antigravity `history.jsonl` (prompt list only; duplicates transcript).
    pub fn include_prompt_history(mut self, include: bool) -> Self {
        self.include_prompt_history = include;
        self
    }

    fn collect_classic(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut sessions = Vec::new();
        let tmp_root = self.root.join("tmp");
        if !tmp_root.exists() {
            return sessions;
        }
        // layout: <slug>/chats/session-*.json (3rd level from tmp_root)
        for entry in WalkDir::new(&tmp_root)
            .min_depth(3)
            .max_depth(3)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let Some(name) = name else { continue };
            if !(name.starts_with("session-") && name.ends_with(".json")) {
                continue;
            }
            if path
                .parent()
                .and_then(|p| p.file_name())
                .is_none_or(|n| n != "chats")
            {
                continue;
            }
            let slug_dir = path.parent().and_then(|p| p.parent()).map(Path::to_path_buf);
            let project = slug_dir
                .as_deref()
                .and_then(project_root_of)
                .unwrap_or_else(|| "unknown".to_string());
            let fallback_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(source) => {
                    warnings.push(SourceError::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                    continue;
                }
            };
            match parse_classic_chat(&text, fallback_id, project) {
                Some(session) => push_if_in_range(&mut sessions, session, range),
                None => warnings.push(SourceError::Parse {
                    path: path.to_path_buf(),
                    line: 0,
                    msg: "invalid chat document or no messages".to_string(),
                }),
            }
        }
        sessions
    }

    fn collect_agy(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut sessions = Vec::new();
        let cli_root = self.root.join("antigravity-cli");
        let brain = cli_root.join("brain");
        if brain.exists() {
            // layout: <uuid>/.system_generated/logs/transcript.jsonl (4th level from brain)
            for entry in WalkDir::new(&brain)
                .min_depth(4)
                .max_depth(4)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if path.file_name().is_none_or(|n| n != "transcript.jsonl") {
                    continue;
                }
                let id = path
                    .ancestors()
                    .nth(3)
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string());
                let lines = match read_lines(path) {
                    Ok(l) => l,
                    Err(e) => {
                        warnings.push(e);
                        continue;
                    }
                };
                let outcome = parse_agy_lines(lines);
                for line in outcome.bad_lines {
                    warnings.push(SourceError::Parse {
                        path: path.to_path_buf(),
                        line,
                        msg: "invalid JSON line".to_string(),
                    });
                }
                if let Some(mut session) = outcome.session {
                    session.id = id;
                    push_if_in_range(&mut sessions, session, range);
                }
            }
        }
        if self.include_prompt_history {
            let history = cli_root.join("history.jsonl");
            if history.exists() {
                match read_lines(&history) {
                    Ok(lines) => {
                        let (history_sessions, bad_lines) = parse_history_lines(lines);
                        for line in bad_lines {
                            warnings.push(SourceError::Parse {
                                path: history.clone(),
                                line,
                                msg: "invalid JSON line".to_string(),
                            });
                        }
                        for session in history_sessions {
                            push_if_in_range(&mut sessions, session, range);
                        }
                    }
                    Err(e) => warnings.push(e),
                }
            }
        }
        sessions
    }
}

impl HistorySource for GeminiSource {
    fn kind(&self) -> AgentKind {
        AgentKind::Gemini
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut sessions = self.collect_classic(range, warnings);
        sessions.extend(self.collect_agy(range, warnings));
        sessions.sort_by_key(|s| s.started_at);
        sessions
    }
}

fn push_if_in_range(sessions: &mut Vec<Session>, session: Session, range: &DateRange) {
    if range.contains_ts(&session.started_at)
        || session.messages.iter().any(|m| range.contains_ts(&m.timestamp))
    {
        sessions.push(session);
    }
}

/// Project name for classic layout: first line of `<slug>/.project_root` is the workspace path.
fn project_root_of(slug_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(slug_dir.join(".project_root")).ok()?;
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

fn read_lines(path: &Path) -> Result<impl Iterator<Item = String>, SourceError> {
    let file = File::open(path).map_err(|source| SourceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(BufReader::new(file).lines().map_while(Result::ok))
}

/// Convert an ISO 8601/RFC 3339 UTC timestamp to the local time zone.
fn parse_iso(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

static USER_REQUEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<USER_REQUEST>\s*(.*?)\s*</USER_REQUEST>").expect("valid regex")
});

/// Strip `<USER_REQUEST>` wrapper from agy user content (other metadata sections are discarded);
/// returns original text when no wrapper is present.
pub(crate) fn strip_agy_user_wrapper(s: &str) -> String {
    match USER_REQUEST_RE.captures(s) {
        Some(caps) => caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default(),
        None => s.to_string(),
    }
}

pub(crate) struct AgyParseOutcome {
    pub session: Option<Session>,
    pub bad_lines: Vec<usize>,
}

/// Parse one agy transcript.jsonl. session.id is empty and filled by source from brain dir name;
/// project is always "unknown" (transcript has no reliable workspace info).
pub(crate) fn parse_agy_lines<I: Iterator<Item = String>>(lines: I) -> AgyParseOutcome {
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
        let source = record.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let kind = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let Some(ts) = record
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(parse_iso)
        else {
            continue;
        };
        match (source, kind) {
            ("USER_EXPLICIT", "USER_INPUT") => {
                let Some(raw) = record.get("content").and_then(|v| v.as_str()) else {
                    continue;
                };
                let text = strip_agy_user_wrapper(raw);
                if text.trim().is_empty() {
                    continue;
                }
                messages.push(Message {
                    role: Role::User,
                    timestamp: ts,
                    content: MessageContent::Text(cap_ingest(&text)),
                });
            }
            ("MODEL", "PLANNER_RESPONSE") => {
                if let Some(calls) = record.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in calls {
                        let name = call
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let summary = compact_json_input(call.get("args"));
                        messages.push(Message {
                            role: Role::Assistant,
                            timestamp: ts,
                            content: MessageContent::ToolUse { name, summary },
                        });
                    }
                }
                if let Some(text) = record.get("content").and_then(|v| v.as_str())
                    && !text.trim().is_empty()
                {
                    messages.push(Message {
                        role: Role::Assistant,
                        timestamp: ts,
                        content: MessageContent::Text(cap_ingest(text)),
                    });
                }
                // thinking field intentionally ignored
            }
            // SYSTEM / CONVERSATION_HISTORY / CHECKPOINT / VIEW_FILE tool results skipped
            _ => {}
        }
    }

    if messages.is_empty() {
        return AgyParseOutcome {
            session: None,
            bad_lines,
        };
    }
    AgyParseOutcome {
        session: Some(Session {
            agent: AgentKind::Gemini,
            project: "unknown".to_string(),
            id: String::new(), // filled by source from brain directory name
            started_at: messages[0].timestamp,
            messages,
        }),
        bad_lines,
    }
}

/// Parse classic `session-*.json` document. Returns None on failure (bad JSON / no messages).
pub(crate) fn parse_classic_chat(
    text: &str,
    fallback_id: String,
    project: String,
) -> Option<Session> {
    let doc = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let id = doc
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or(fallback_id);
    let records = doc.get("messages")?.as_array()?;
    let mut messages: Vec<Message> = Vec::new();

    for record in records {
        let role = match record.get("type").and_then(|v| v.as_str()) {
            Some("user") => Role::User,
            Some("gemini" | "model" | "assistant") => Role::Assistant,
            _ => continue,
        };
        let Some(ts) = record
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_iso)
        else {
            continue;
        };
        // content: string or [{text}] parts
        match record.get("content") {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
                messages.push(Message {
                    role,
                    timestamp: ts,
                    content: MessageContent::Text(cap_ingest(s)),
                });
            }
            Some(serde_json::Value::Array(parts)) => {
                for part in parts {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str())
                        && !t.trim().is_empty()
                    {
                        messages.push(Message {
                            role,
                            timestamp: ts,
                            content: MessageContent::Text(cap_ingest(t)),
                        });
                    }
                }
            }
            _ => {}
        }
        if role == Role::Assistant
            && let Some(calls) = record.get("toolCalls").and_then(|v| v.as_array())
        {
            for call in calls {
                let name = call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let summary = compact_json_input(call.get("args"));
                messages.push(Message {
                    role,
                    timestamp: ts,
                    content: MessageContent::ToolUse { name, summary },
                });
            }
        }
    }

    messages.sort_by_key(|m| m.timestamp);
    if messages.is_empty() {
        return None;
    }
    Some(Session {
        agent: AgentKind::Gemini,
        project,
        id,
        started_at: messages[0].timestamp,
        messages,
    })
}

/// Parse antigravity `history.jsonl`: user prompts only, grouped by workspace into
/// one session per workspace (id fixed to "prompt-history"). Skips `slash_command`.
pub(crate) fn parse_history_lines<I: Iterator<Item = String>>(
    lines: I,
) -> (Vec<Session>, Vec<usize>) {
    let mut bad_lines = Vec::new();
    let mut by_workspace: BTreeMap<String, Vec<Message>> = BTreeMap::new();

    for (idx, line) in lines.enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            bad_lines.push(idx + 1);
            continue;
        };
        if record.get("type").and_then(|v| v.as_str()) == Some("slash_command") {
            continue;
        }
        let (Some(display), Some(ms)) = (
            record.get("display").and_then(|v| v.as_str()),
            record.get("timestamp").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        if display.trim().is_empty() {
            continue;
        }
        let Some(ts) = DateTime::from_timestamp_millis(ms) else {
            continue;
        };
        let workspace = record
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        by_workspace
            .entry(workspace)
            .or_default()
            .push(Message {
                role: Role::User,
                timestamp: ts.with_timezone(&Local),
                content: MessageContent::Text(cap_ingest(display)),
            });
    }

    let sessions = by_workspace
        .into_iter()
        .filter_map(|(workspace, mut messages)| {
            messages.sort_by_key(|m| m.timestamp);
            let started_at = messages.first()?.timestamp;
            Some(Session {
                agent: AgentKind::Gemini,
                project: workspace,
                id: "prompt-history".to_string(),
                started_at,
                messages,
            })
        })
        .collect();
    (sessions, bad_lines)
}

#[cfg(test)]
mod tests {
    use crate::domain::{AgentKind, DateRange, MessageContent, Role};
    use crate::sources::gemini::{
        GeminiSource, parse_agy_lines, parse_classic_chat, parse_history_lines,
        strip_agy_user_wrapper,
    };
    use crate::sources::{HistorySource, SourceError};
    use chrono::{DateTime, NaiveDate};

    const AGY: &str = include_str!("../../tests/fixtures/gemini/agy-transcript.jsonl");
    const CLASSIC: &str = include_str!("../../tests/fixtures/gemini/chat-classic.json");
    const HISTORY: &str = include_str!("../../tests/fixtures/gemini/history.jsonl");

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc(s: &str) -> DateTime<chrono::Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    // ---------- agy user wrapper stripping ----------

    #[test]
    fn agy_wrapper_stripped_keeps_only_request_text() {
        let raw = "<USER_REQUEST>\n就按这个方案改\n</USER_REQUEST>\n\
                   <ADDITIONAL_METADATA>\nThe current local time is: x.\n</ADDITIONAL_METADATA>\n\
                   <USER_SETTINGS_CHANGE>\nchanged model\n</USER_SETTINGS_CHANGE>";
        assert_eq!(strip_agy_user_wrapper(raw), "就按这个方案改");
    }

    #[test]
    fn agy_plain_content_passes_through() {
        assert_eq!(strip_agy_user_wrapper("普通文本"), "普通文本");
    }

    // ---------- agy transcript parsing ----------

    #[test]
    fn parses_agy_fixture_end_to_end() {
        let outcome = parse_agy_lines(AGY.lines().map(str::to_string));
        let session = outcome.session.expect("should parse");
        assert_eq!(session.agent, AgentKind::Gemini);
        assert_eq!(session.started_at.to_utc(), utc("2026-07-14T01:10:00Z"));

        let msgs = &session.messages;
        // Two user messages, two assistant texts, and one tool call total five messages.
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(
            msgs[0].text().unwrap(),
            "帮我把量子计算蓝图里的“系统层”改成突出芯片的说法"
        );
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(matches!(msgs[1].content, MessageContent::ToolUse { .. }));
        assert_eq!(msgs[2].timestamp.to_utc(), utc("2026-07-14T01:10:30Z"));
        assert_eq!(msgs[3].role, Role::User);
        assert_eq!(msgs[3].text().unwrap(), "就按这个方案改");
        assert!(msgs[4].text().unwrap().contains("智能量子芯片层"));
    }

    #[test]
    fn agy_tool_calls_become_compact_tooluse() {
        let outcome = parse_agy_lines(AGY.lines().map(str::to_string));
        let session = outcome.session.unwrap();
        let tools: Vec<_> = session
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::ToolUse { name, summary } => Some((name, summary)),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].0, "view_file");
        assert!(tools[0].1.contains("蓝图.svg"));
        assert!(!tools[0].1.contains('\n'));
        assert!(tools[0].1.len() <= 121);
    }

    #[test]
    fn agy_skips_system_thinking_and_tool_result_records() {
        let outcome = parse_agy_lines(AGY.lines().map(str::to_string));
        let session = outcome.session.unwrap();
        for m in &session.messages {
            if let Some(t) = m.text() {
                assert!(!t.contains("Analyzing"), "thinking must not become a message");
                assert!(!t.contains("Created At:"), "tool results must not become messages");
            }
        }
    }

    #[test]
    fn agy_session_id_is_placeholder_filled_by_source() {
        let outcome = parse_agy_lines(AGY.lines().map(str::to_string));
        assert_eq!(outcome.session.unwrap().id, "");
    }

    // ---------- classic chat parsing ----------

    #[test]
    fn parses_classic_chat_fixture() {
        let session = parse_classic_chat(CLASSIC, "fallback-id".to_string(), "proj".to_string())
            .expect("should parse");
        assert_eq!(session.agent, AgentKind::Gemini);
        assert_eq!(session.id, "c3d4e5f6-1111-4222-8333-abcdefabcdef");
        assert_eq!(session.started_at.to_utc(), utc("2026-07-14T02:00:00Z"));
        // Two user messages, two assistant messages, and one tool call total five messages.
        assert_eq!(session.messages.len(), 5);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text().unwrap(), "帮我查一下这个项目的 license");
        let tool = session
            .messages
            .iter()
            .find(|m| matches!(m.content, MessageContent::ToolUse { .. }))
            .expect("should have tool");
        match &tool.content {
            MessageContent::ToolUse { name, summary } => {
                assert_eq!(name, "read_file");
                assert!(summary.contains("LICENSE"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn classic_content_string_form_accepted() {
        let doc = r#"{"sessionId":"s1","messages":[
            {"timestamp":"2026-07-14T03:00:00Z","type":"user","content":"字符串形式的提问"},
            {"timestamp":"2026-07-14T03:00:10Z","type":"model","content":"字符串形式的回答"}
        ]}"#;
        let session = parse_classic_chat(doc, "fb".to_string(), "p".to_string()).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text().unwrap(), "字符串形式的回答");
    }

    #[test]
    fn classic_bad_json_returns_none() {
        assert!(parse_classic_chat("not json", "fb".to_string(), "p".to_string()).is_none());
    }

    // ---------- history.jsonl parsing ----------

    #[test]
    fn history_groups_by_workspace_and_skips_slash_commands() {
        let (sessions, bad) = parse_history_lines(HISTORY.lines().map(str::to_string));
        assert!(bad.is_empty());
        assert_eq!(sessions.len(), 2);
        let xq = sessions
            .iter()
            .find(|s| s.project == "/mnt/d/Research/XQ")
            .unwrap();
        assert_eq!(xq.messages.len(), 1); // slash_command skipped
        assert_eq!(xq.messages[0].role, Role::User);
        assert_eq!(xq.messages[0].text().unwrap(), "把周报生成器加上 --stdout 选项");
        assert_eq!(xq.messages[0].timestamp.to_utc(), utc("2026-07-14T04:00:00Z"));
        assert_eq!(xq.id, "prompt-history");
        let interview = sessions
            .iter()
            .find(|s| s.project == "/mnt/d/计算所/面试")
            .unwrap();
        assert_eq!(interview.messages.len(), 1);
    }

    // ---------- source collection (fake $HOME) ----------

    /// Build agy + classic + history layouts under a fake home.
    fn build_fake_home() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        // agy
        let agy_dir = tmp.path().join(
            ".gemini/antigravity-cli/brain/4194a992-fddf-4a11-8f86-050f9c2470c9/.system_generated/logs",
        );
        std::fs::create_dir_all(&agy_dir).unwrap();
        std::fs::write(agy_dir.join("transcript.jsonl"), AGY).unwrap();
        std::fs::write(
            tmp.path().join(".gemini/antigravity-cli/history.jsonl"),
            HISTORY,
        )
        .unwrap();
        // classic
        let chats = tmp.path().join(".gemini/tmp/xq/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("session-2026-07-14T10-00-c3d4e5f6.json"), CLASSIC).unwrap();
        std::fs::write(
            tmp.path().join(".gemini/tmp/xq/.project_root"),
            "/mnt/d/Research/XQ\nxq\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn source_merges_classic_and_agy_excluding_history_by_default() {
        let tmp = build_fake_home();
        let source = GeminiSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings: Vec<SourceError> = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        // One Antigravity session and one classic session; history is excluded by default.
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .any(|s| s.id == "4194a992-fddf-4a11-8f86-050f9c2470c9")
        );
        assert!(
            sessions
                .iter()
                .any(|s| s.id == "c3d4e5f6-1111-4222-8333-abcdefabcdef")
        );
        assert!(!sessions.iter().any(|s| s.id == "prompt-history"));
    }

    #[test]
    fn source_fills_agy_id_from_brain_dir_and_project_unknown() {
        let tmp = build_fake_home();
        let source = GeminiSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        let agy = sessions
            .iter()
            .find(|s| s.id == "4194a992-fddf-4a11-8f86-050f9c2470c9")
            .unwrap();
        assert_eq!(agy.project, "unknown");
    }

    #[test]
    fn source_reads_classic_project_from_project_root_file() {
        let tmp = build_fake_home();
        let source = GeminiSource::new(tmp.path());
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        let classic = sessions
            .iter()
            .find(|s| s.id == "c3d4e5f6-1111-4222-8333-abcdefabcdef")
            .unwrap();
        assert_eq!(classic.project, "/mnt/d/Research/XQ");
    }

    #[test]
    fn source_includes_history_when_flag_enabled() {
        let tmp = build_fake_home();
        let source = GeminiSource::new(tmp.path()).include_prompt_history(true);
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let mut warnings = Vec::new();
        let sessions = source.collect(&range, &mut warnings);
        // One Antigravity session, one classic session, and two history groups by workspace.
        assert_eq!(sessions.len(), 4);
        assert_eq!(
            sessions
                .iter()
                .filter(|s| s.id == "prompt-history")
                .count(),
            2
        );
    }

    #[test]
    fn source_filters_sessions_outside_range() {
        let tmp = build_fake_home();
        let source = GeminiSource::new(tmp.path()).include_prompt_history(true);
        let range = DateRange::new(d("2026-08-01"), d("2026-08-07")).unwrap();
        let mut warnings = Vec::new();
        assert!(source.collect(&range, &mut warnings).is_empty());
    }
}
