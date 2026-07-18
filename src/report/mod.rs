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
    let mut sessions = 0usize;
    let mut messages = 0usize;
    for bucket in buckets {
        sessions += bucket.sessions.len();
        messages += bucket.message_count();
        for s in &bucket.sessions {
            *per_agent.entry(s.agent).or_default() += 1;
        }
    }
    let agent_part = per_agent
        .iter()
        .map(|(kind, n)| format!("{} × {}", kind.display_name(), n))
        .collect::<Vec<_>>()
        .join(" · ");
    format!(
        "有记录 {} 天 · 会话 {} 个（{}）· 消息 {} 条",
        buckets.len(),
        sessions,
        agent_part,
        messages
    )
}

#[cfg(test)]
mod tests {
    use super::{INLINE_BUDGET, PromptMode, assemble_prompt, build_stats};
    use crate::collect::DayBucket;
    use crate::domain::{AgentKind, Message, MessageContent, Role, Session};
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
}
