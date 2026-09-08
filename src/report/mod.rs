//! Weekly report summarization: `Summarizer` trait, prompt assembly (FilesManifest / Inline modes),
//! and stats line construction.

pub mod api_backend;
pub mod cli_backend;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::collect::DayBucket;
use crate::domain::AgentKind;

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("external CLI `{cmd}` failed (exit code {code:?}): {stderr}")]
    CliFailed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("external CLI `{cmd}` timed out after {secs} seconds and was terminated")]
    CliTimeout { cmd: String, secs: u64 },
    #[error("failed to spawn external CLI `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("environment variable {0} is not set (used to read API key)")]
    MissingApiKey(String),
    #[error("API request failed: {0}")]
    Http(String),
    #[error("API returned status {status}: {body}")]
    ApiStatus { status: u16, body: String },
    #[error("failed to parse API response: {0}")]
    ApiParse(String),
    #[error("collected directory does not exist: {0} (run collect first)")]
    DirNotFound(PathBuf),
    #[error("no daily record files under collected directory {0} (run collect first)")]
    EmptyDir(PathBuf),
    #[error("unknown CLI preset: {0} (expected: codex, claude, agy, gemini; or use --cmd for a custom command)")]
    UnknownCliPreset(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Prompt assembly mode: CLI backend uses a file manifest (reads the directory itself); API backend inlines full text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    FilesManifest,
    Inline,
}

#[derive(Debug)]
pub struct Prompt {
    pub text: String,
    /// Working directory for the CLI backend (the range directory).
    pub dir: PathBuf,
}

/// Summarization backend abstraction (CLI / API implementations).
pub trait Summarizer {
    fn summarize(&self, prompt: &Prompt) -> Result<String, ReportError>;
}

/// Total character budget for inlined full text in Inline mode (200KB).
/// Inline mode total character budget for embedded daily notes (~48KB).
/// Kept modest so API backends start streaming before CDN first-byte timeouts.
pub const INLINE_BUDGET: usize = 48 * 1024;

/// Assemble a prompt. `files` is (filename, content) pairs (usually daily files excluding index).
pub fn assemble_prompt(
    rendered_template: &str,
    mode: PromptMode,
    dir: &Path,
    files: &[(String, String)],
) -> Prompt {
    let text = match mode {
        PromptMode::FilesManifest => {
            let list = files
                .iter()
                .map(|(name, _)| format!("- {name}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{rendered_template}\n\n## 输入文件\n\n对话记录位于当前工作目录（`{}`）下：\n{list}\n\n请逐一阅读这些文件后撰写周报。\n",
                dir.display()
            )
        }
        PromptMode::Inline => {
            let total: usize = files.iter().map(|(_, c)| c.chars().count()).sum();
            let mut text = format!("{rendered_template}\n\n## 对话记录\n");
            if total <= INLINE_BUDGET {
                for (name, content) in files {
                    let safe = crate::secrets::redact_secrets(content);
                    text.push_str(&format!("\n### 文件 {name}\n\n{safe}\n"));
                }
            } else {
                // over budget: split budget evenly across files and truncate each
                let share = INLINE_BUDGET / files.len().max(1);
                for (name, content) in files {
                    let safe = crate::secrets::redact_secrets(content);
                    let count = safe.chars().count();
                    let body: String = safe.chars().take(share).collect();
                    if count > share {
                        text.push_str(&format!(
                            "\n### 文件 {name}\n\n{body}…（已截断，原文共 {count} 字符）\n"
                        ));
                    } else {
                        text.push_str(&format!("\n### 文件 {name}\n\n{body}\n"));
                    }
                }
            }
            text
        }
    };
    Prompt {
        text,
        dir: dir.to_path_buf(),
    }
}

/// Build summary stats lines (content for template `{{stats}}` placeholder).
pub fn build_stats(buckets: &[DayBucket]) -> String {
    if buckets.is_empty() {
        return "本周无对话记录".to_string();
    }
    let mut per_agent: BTreeMap<AgentKind, usize> = BTreeMap::new();
    let mut messages = 0usize;
    for bucket in buckets {
        messages += bucket.message_count();
        for s in &bucket.sessions {
            *per_agent.entry(s.agent).or_default() += 1;
        }
    }
    format_stats(buckets.len(), &per_agent, messages)
}

fn format_stats(days: usize, per_agent: &BTreeMap<AgentKind, usize>, messages: usize) -> String {
    let sessions: usize = per_agent.values().sum();
    let agent_part = per_agent
        .iter()
        .map(|(kind, n)| format!("{} × {}", kind.display_name(), n))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("有记录 {days} 天 · 会话 {sessions} 个（{agent_part}）· 消息 {messages} 条")
}

fn agent_from_heading(heading: &str) -> Option<AgentKind> {
    AgentKind::ALL
        .into_iter()
        .find(|k| k.display_name() == heading)
}

/// Loaded collected directory content + stats inferred from files (respects manual edits to daily files).
#[derive(Debug)]
pub struct DirSummary {
    /// (filename, content) pairs sorted by filename, excluding index.md.
    pub files: Vec<(String, String)>,
    pub stats: String,
}

/// Read all daily files under `out/<range>/` and count sessions/messages.
/// Counts are anchored to exact agent headings, structured session headings, and session metadata lines
/// so markdown headings inside message bodies do not skew counts.
pub fn load_collected_dir(dir: &Path) -> Result<DirSummary, ReportError> {
    if !dir.is_dir() {
        return Err(ReportError::DirNotFound(dir.to_path_buf()));
    }
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".md") && n != "index.md")
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(ReportError::EmptyDir(dir.to_path_buf()));
    }
    let mut files = Vec::new();
    let mut per_agent: BTreeMap<AgentKind, usize> = BTreeMap::new();
    let mut messages = 0usize;
    for name in names {
        let content = std::fs::read_to_string(dir.join(&name))?;
        let mut current_agent: Option<AgentKind> = None;
        for line in content.lines() {
            if let Some(heading) = line.strip_prefix("## ") {
                // only exact agent names switch sections; `## xxx` in message bodies is ignored
                if let Some(kind) = agent_from_heading(heading.trim()) {
                    current_agent = Some(kind);
                }
            } else if is_session_heading(line) {
                if let Some(kind) = current_agent {
                    *per_agent.entry(kind).or_default() += 1;
                }
            } else if let Some(n) = parse_session_meta_count(line) {
                messages += n;
            }
        }
        files.push((name, content));
    }
    let stats = format_stats(files.len(), &per_agent, messages);
    Ok(DirSummary { files, stats })
}

/// Match only rendered session headings containing a backtick-delimited session ID.
/// Natural headings in message bodies lack that ID segment and do not match.
fn is_session_heading(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("### 会话 `") else {
        return false;
    };
    let Some(end) = rest.find('`') else {
        return false;
    };
    end > 0 && rest[end + 1..].starts_with(" · ")
}

/// Parse the message count from a rendered session metadata line.
fn parse_session_meta_count(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("- 时间: ")?;
    let count_part = rest.strip_suffix(" 条消息")?;
    let n = count_part.rsplit(" · ").next()?;
    // The time span must contain exactly two clock values to avoid false positives in body text.
    let time_part = count_part.strip_suffix(&format!(" · {n}"))?;
    let mut halves = time_part.split(" – ");
    let valid = matches!(
        (halves.next(), halves.next(), halves.next()),
        (Some(a), Some(b), None) if a.len() == 5 && b.len() == 5
            && a.as_bytes()[2] == b':' && b.as_bytes()[2] == b':'
    );
    if valid { n.parse().ok() } else { None }
}

#[cfg(test)]
mod tests {
    use super::{
        INLINE_BUDGET, PromptMode, ReportError, assemble_prompt, build_stats, load_collected_dir,
    };
    use crate::collect::DayBucket;
    use crate::domain::{AgentKind, Message, MessageContent, Role, Session};
    use crate::render::daily::render_daily;
    use chrono::{DateTime, Local, NaiveDate, TimeZone};
    use std::path::Path;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn at(day: &str) -> DateTime<Local> {
        let naive = day.parse::<NaiveDate>().unwrap().and_hms_opt(10, 0, 0).unwrap();
        Local.from_local_datetime(&naive).unwrap()
    }

    fn session(agent: AgentKind, id: &str, n: usize) -> Session {
        Session {
            agent,
            project: "p".to_string(),
            id: id.to_string(),
            started_at: at("2026-07-15"),
            messages: (0..n)
                .map(|i| Message {
                    role: Role::User,
                    timestamp: at("2026-07-15"),
                    content: MessageContent::Text(format!("m{i}")),
                })
                .collect(),
        }
    }

    // ---------- assemble_prompt ----------

    #[test]
    fn manifest_mode_lists_files_not_contents() {
        let files = vec![
            ("2026-07-15.md".to_string(), "SECRET_CONTENT_15".to_string()),
            ("2026-07-16.md".to_string(), "SECRET_CONTENT_16".to_string()),
        ];
        let prompt = assemble_prompt(
            "模板正文",
            PromptMode::FilesManifest,
            Path::new("/out/range"),
            &files,
        );
        assert!(prompt.text.contains("模板正文"));
        assert!(prompt.text.contains("2026-07-15.md"));
        assert!(prompt.text.contains("2026-07-16.md"));
        assert!(!prompt.text.contains("SECRET_CONTENT_15"));
        assert_eq!(prompt.dir, Path::new("/out/range"));
    }

    #[test]
    fn inline_mode_embeds_contents_under_budget() {
        let files = vec![
            ("a.md".to_string(), "第一天内容".to_string()),
            ("b.md".to_string(), "第二天内容".to_string()),
        ];
        let prompt = assemble_prompt("模板", PromptMode::Inline, Path::new("/x"), &files);
        assert!(prompt.text.contains("第一天内容"));
        assert!(prompt.text.contains("第二天内容"));
        assert!(prompt.text.contains("a.md"));
    }

    #[test]
    fn inline_mode_truncates_proportionally_over_budget() {
        // Three 40 KB files exceed the 48 KB budget, so the complete prompt must be truncated near the limit.
        let big = "字".repeat(40 * 1024);
        let files: Vec<(String, String)> = (0..3)
            .map(|i| (format!("f{i}.md"), big.clone()))
            .collect();
        let prompt = assemble_prompt("模板", PromptMode::Inline, Path::new("/x"), &files);
        assert!(prompt.text.chars().count() < INLINE_BUDGET + 1024);
        assert!(prompt.text.contains("截断"));
    }

    // ---------- build_stats ----------

    #[test]
    fn build_stats_summarizes_buckets() {
        let buckets = vec![
            DayBucket {
                date: d("2026-07-15"),
                sessions: vec![session(AgentKind::Codex, "c1", 3), session(AgentKind::Claude, "a1", 2)],
            },
            DayBucket {
                date: d("2026-07-16"),
                sessions: vec![session(AgentKind::Codex, "c2", 5)],
            },
        ];
        let stats = build_stats(&buckets);
        assert_eq!(
            stats,
            "有记录 2 天 · 会话 3 个（Codex × 2 · Claude Code × 1）· 消息 10 条"
        );
    }

    #[test]
    fn build_stats_empty() {
        assert_eq!(build_stats(&[]), "本周无对话记录");
    }

    // ---------- load_collected_dir ----------

    fn write_day_file(dir: &Path, name: &str, sessions: &[Session]) {
        let date = NaiveDate::parse_from_str(name.trim_end_matches(".md"), "%Y-%m-%d").unwrap();
        std::fs::write(dir.join(name), render_daily(date, sessions)).unwrap();
    }

    #[test]
    fn load_collected_dir_counts_sessions_and_messages_per_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_day_file(
            dir,
            "2026-07-15.md",
            &[session(AgentKind::Codex, "c1", 3), session(AgentKind::Claude, "a1", 2)],
        );
        write_day_file(dir, "2026-07-16.md", &[session(AgentKind::Codex, "c2", 5)]);
        std::fs::write(dir.join("index.md"), "# 索引（不应计入）\n").unwrap();

        let summary = load_collected_dir(dir).unwrap();
        assert_eq!(summary.files.len(), 2, "index.md should be excluded");
        assert_eq!(summary.files[0].0, "2026-07-15.md");
        assert!(summary.files[0].1.contains("AI 对话记录"));
        assert_eq!(
            summary.stats,
            "有记录 2 天 · 会话 3 个（Codex × 2 · Claude Code × 1）· 消息 10 条"
        );
    }

    #[test]
    fn load_collected_dir_ignores_headings_inside_message_bodies() {
        // Markdown headings inside assistant message bodies must not be counted as structural headings.
        // must not affect stats
        let tmp = tempfile::tempdir().unwrap();
        let mut s = session(AgentKind::Cursor, "c1", 0);
        s.messages = vec![
            Message {
                role: Role::User,
                timestamp: at("2026-07-15"),
                content: MessageContent::Text("问题".to_string()),
            },
            Message {
                role: Role::Assistant,
                timestamp: at("2026-07-15"),
                content: MessageContent::Text(
                    "## 综合分析\n\n### 一、架构对比\n\n**👤 用户** `99:99`\n\n正文".to_string(),
                ),
            },
        ];
        write_day_file(tmp.path(), "2026-07-15.md", &[s]);
        let summary = load_collected_dir(tmp.path()).unwrap();
        assert_eq!(
            summary.stats,
            "有记录 1 天 · 会话 1 个（Cursor × 1）· 消息 2 条"
        );
    }

    #[test]
    fn load_collected_dir_missing_dir_errors() {
        let err = load_collected_dir(Path::new("/nonexistent/dir")).unwrap_err();
        assert!(matches!(err, ReportError::DirNotFound(_)));
    }

    #[test]
    fn load_collected_dir_without_day_files_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("index.md"), "只有索引\n").unwrap();
        let err = load_collected_dir(tmp.path()).unwrap_err();
        assert!(matches!(err, ReportError::EmptyDir(_)));
    }
}
