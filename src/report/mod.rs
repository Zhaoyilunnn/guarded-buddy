//! 周报总结：`Summarizer` trait、prompt 组装（FilesManifest / Inline 两种模式）、
//! 统计行构建。

pub mod api_backend;
pub mod cli_backend;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::collect::DayBucket;
use crate::domain::AgentKind;

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("外部 CLI `{cmd}` 执行失败（退出码 {code:?}）：{stderr}")]
    CliFailed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("外部 CLI `{cmd}` 超时（{secs} 秒）已被终止")]
    CliTimeout { cmd: String, secs: u64 },
    #[error("无法启动外部 CLI `{cmd}`：{source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("环境变量 {0} 未设置（用于读取 API key）")]
    MissingApiKey(String),
    #[error("API 请求失败：{0}")]
    Http(String),
    #[error("API 返回状态 {status}：{body}")]
    ApiStatus { status: u16, body: String },
    #[error("无法解析 API 响应：{0}")]
    ApiParse(String),
    #[error("采集目录不存在：{0}（请先运行 collect）")]
    DirNotFound(PathBuf),
    #[error("采集目录 {0} 下没有日记录文件（请先运行 collect）")]
    EmptyDir(PathBuf),
    #[error("未知的 CLI 预设：{0}（可选: codex, claude, agy, gemini；或用 --cmd 自定义）")]
    UnknownCliPreset(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// prompt 组装模式：CLI 后端用文件清单（让它自己读目录），API 后端内嵌全文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    FilesManifest,
    Inline,
}

#[derive(Debug)]
pub struct Prompt {
    pub text: String,
    /// CLI 后端的工作目录（range 目录）。
    pub dir: PathBuf,
}

/// 总结后端抽象（CLI / API 两种实现）。
pub trait Summarizer {
    fn summarize(&self, prompt: &Prompt) -> Result<String, ReportError>;
}

/// Inline 模式内嵌全文的总字符预算（200KB）。
pub const INLINE_BUDGET: usize = 200 * 1024;

/// 组装 prompt。`files` 为（文件名， 内容）对（通常是不含 index 的日文件）。
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
                    text.push_str(&format!("\n### 文件 {name}\n\n{content}\n"));
                }
            } else {
                // 超预算：按文件数均分预算逐个截断
                let share = INLINE_BUDGET / files.len().max(1);
                for (name, content) in files {
                    let count = content.chars().count();
                    let body: String = content.chars().take(share).collect();
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

/// 汇总统计行（模板 `{{stats}}` 占位符的内容）。
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

/// 已采集目录的内容 + 由文件反推的统计（尊重用户对日文件的手工编辑）。
#[derive(Debug)]
pub struct DirSummary {
    /// （文件名， 内容）按文件名排序，不含 index.md。
    pub files: Vec<(String, String)>,
    pub stats: String,
}

/// 读取 `out/<range>/` 下所有日文件并统计会话/消息数。
/// 通过扫描我们自己渲染的 markdown 结构（`## Agent` 段、`### 会话` 头、
/// `**👤/**🤖/**🔧` 消息行）计数。
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
                current_agent = agent_from_heading(heading.trim());
            } else if line.starts_with("### 会话") {
                if let Some(kind) = current_agent {
                    *per_agent.entry(kind).or_default() += 1;
                }
            } else if line.starts_with("**👤")
                || line.starts_with("**🤖")
                || line.starts_with("**🔧")
            {
                messages += 1;
            }
        }
        files.push((name, content));
    }
    let stats = format_stats(files.len(), &per_agent, messages);
    Ok(DirSummary { files, stats })
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
        // 3 个文件各 100KB，总预算 200KB → 每个约 66KB，全文必须低于预算上限
        let big = "字".repeat(100 * 1024);
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
        assert_eq!(summary.files.len(), 2, "index.md 应被排除");
        assert_eq!(summary.files[0].0, "2026-07-15.md");
        assert!(summary.files[0].1.contains("AI 对话记录"));
        assert_eq!(
            summary.stats,
            "有记录 2 天 · 会话 3 个（Codex × 2 · Claude Code × 1）· 消息 10 条"
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
