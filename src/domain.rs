//! 核心领域类型：跨所有 agent 数据源共享的统一模型。

use chrono::{DateTime, Duration, Local, NaiveDate};

/// 支持的 AI 编程助手种类。渲染与 CLI 过滤均按 `ALL` 中的固定顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentKind {
    Codex,
    Cursor,
    Claude,
    Gemini,
}

impl AgentKind {
    pub const ALL: [AgentKind; 4] = [
        AgentKind::Codex,
        AgentKind::Cursor,
        AgentKind::Claude,
        AgentKind::Gemini,
    ];

    /// 面向用户的名称（用于 Markdown 标题）。
    pub fn display_name(&self) -> &'static str {
        match self {
            AgentKind::Codex => "Codex",
            AgentKind::Cursor => "Cursor",
            AgentKind::Claude => "Claude Code",
            AgentKind::Gemini => "Gemini",
        }
    }

    /// CLI/配置中使用的标识符。
    pub fn slug(&self) -> &'static str {
        match self {
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursor",
            AgentKind::Claude => "claude",
            AgentKind::Gemini => "gemini",
        }
    }

    pub fn from_slug(s: &str) -> Option<AgentKind> {
        AgentKind::ALL.into_iter().find(|k| k.slug() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Assistant,
    #[allow(dead_code)] // 预留给未来纳入 system 消息
    System,
}

/// 消息内容：纯文本，或一次工具调用（压缩为一行摘要）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageContent {
    Text(String),
    ToolUse { name: String, summary: String },
}

impl MessageContent {
    pub fn is_text(&self) -> bool {
        matches!(self, MessageContent::Text(_))
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(t) => Some(t),
            MessageContent::ToolUse { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    /// 本地时区时间（解析边界处已从 UTC/偏移转换）。
    pub timestamp: DateTime<Local>,
    pub content: MessageContent,
}

impl Message {
    pub fn is_text(&self) -> bool {
        self.content.is_text()
    }

    pub fn text(&self) -> Option<&str> {
        self.content.text()
    }
}

/// 一次完整会话（一个 rollout 文件 / 一个 transcript / 一个 chat）。
#[derive(Debug, Clone)]
pub struct Session {
    pub agent: AgentKind,
    /// 工作目录（codex/claude）或项目标识（cursor/gemini）。
    pub project: String,
    pub id: String,
    pub started_at: DateTime<Local>,
    /// 按时间升序。
    pub messages: Vec<Message>,
}

/// 本地日历日闭区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl DateRange {
    pub const MAX_DAYS: i64 = 366;

    pub fn new(start: NaiveDate, end: NaiveDate) -> Result<Self, DomainError> {
        if start > end {
            return Err(DomainError::InvertedRange { start, end });
        }
        let len = (end - start).num_days() + 1;
        if len > Self::MAX_DAYS {
            return Err(DomainError::RangeTooLong { days: len });
        }
        Ok(Self { start, end })
    }

    /// 以 `today` 为终点（含）向前数 n 天。
    pub fn last_n_days(n: u32, today: NaiveDate) -> Self {
        let n = n.max(1) as i64;
        Self {
            start: today - Duration::days(n - 1),
            end: today,
        }
    }

    pub fn contains_day(&self, d: NaiveDate) -> bool {
        self.start <= d && d <= self.end
    }

    pub fn contains_ts(&self, ts: &DateTime<Local>) -> bool {
        self.contains_day(ts.date_naive())
    }

    pub fn days(&self) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        let mut d = self.start;
        while d <= self.end {
            out.push(d);
            d = d.succ_opt().expect("NaiveDate overflow");
        }
        out
    }

    /// 输出目录名，如 `2026-07-12_2026-07-18`。
    pub fn dir_name(&self) -> String {
        format!("{}_{}", self.start, self.end)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("invalid date range: start {start} is after end {end}")]
    InvertedRange { start: NaiveDate, end: NaiveDate },
    #[error("date range too long: {days} days (max {})", DateRange::MAX_DAYS)]
    RangeTooLong { days: i64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, NaiveDate, TimeZone};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn local_dt(naive: chrono::NaiveDateTime) -> chrono::DateTime<Local> {
        Local.from_local_datetime(&naive).unwrap()
    }

    #[test]
    fn date_range_last_7_days_endpoints() {
        let range = DateRange::last_n_days(7, d("2026-07-18"));
        assert_eq!(range.start, d("2026-07-12"));
        assert_eq!(range.end, d("2026-07-18"));
    }

    #[test]
    fn date_range_last_1_day_is_today_only() {
        let range = DateRange::last_n_days(1, d("2026-07-18"));
        assert_eq!(range.start, d("2026-07-18"));
        assert_eq!(range.end, d("2026-07-18"));
    }

    #[test]
    fn date_range_rejects_inverted() {
        let err = DateRange::new(d("2026-07-18"), d("2026-07-12")).unwrap_err();
        assert!(matches!(err, DomainError::InvertedRange { .. }));
    }

    #[test]
    fn date_range_rejects_overlong() {
        let err = DateRange::new(d("2026-01-01"), d("2027-06-01")).unwrap_err();
        assert!(matches!(err, DomainError::RangeTooLong { .. }));
    }

    #[test]
    fn date_range_days_yields_each_day() {
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let days = range.days();
        assert_eq!(days.len(), 7);
        assert_eq!(days[0], d("2026-07-12"));
        assert_eq!(days[6], d("2026-07-18"));
    }

    #[test]
    fn date_range_dir_name_format() {
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        assert_eq!(range.dir_name(), "2026-07-12_2026-07-18");
    }

    #[test]
    fn date_range_contains_day_inclusive() {
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        assert!(range.contains_day(d("2026-07-12")));
        assert!(range.contains_day(d("2026-07-18")));
        assert!(!range.contains_day(d("2026-07-11")));
        assert!(!range.contains_day(d("2026-07-19")));
    }

    #[test]
    fn date_range_contains_ts_uses_local_date() {
        let range = DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap();
        let inside = local_dt(d("2026-07-15").and_hms_opt(23, 59, 59).unwrap());
        let outside = local_dt(d("2026-07-19").and_hms_opt(0, 0, 0).unwrap());
        assert!(range.contains_ts(&inside));
        assert!(!range.contains_ts(&outside));
    }

    #[test]
    fn agent_kind_slug_roundtrip() {
        for kind in AgentKind::ALL {
            assert_eq!(AgentKind::from_slug(kind.slug()), Some(kind));
        }
        assert_eq!(AgentKind::from_slug("unknown"), None);
    }

    #[test]
    fn agent_kind_display_names() {
        assert_eq!(AgentKind::Codex.display_name(), "Codex");
        assert_eq!(AgentKind::Cursor.display_name(), "Cursor");
        assert_eq!(AgentKind::Claude.display_name(), "Claude Code");
        assert_eq!(AgentKind::Gemini.display_name(), "Gemini");
    }

    #[test]
    fn agent_kind_order_is_codex_cursor_claude_gemini() {
        assert_eq!(
            AgentKind::ALL,
            [
                AgentKind::Codex,
                AgentKind::Cursor,
                AgentKind::Claude,
                AgentKind::Gemini
            ]
        );
    }

    #[test]
    fn message_content_accessors() {
        let text = MessageContent::Text("hello".to_string());
        assert!(text.is_text());
        assert_eq!(text.text(), Some("hello"));

        let tool = MessageContent::ToolUse {
            name: "Read".to_string(),
            summary: "{}".to_string(),
        };
        assert!(!tool.is_text());
        assert_eq!(tool.text(), None);
    }

    #[test]
    fn message_is_text_delegates_to_content() {
        let msg = Message {
            role: Role::User,
            timestamp: local_dt(d("2026-07-15").and_hms_opt(10, 0, 0).unwrap()),
            content: MessageContent::Text("hi".to_string()),
        };
        assert!(msg.is_text());
        assert_eq!(msg.text(), Some("hi"));
    }
}
