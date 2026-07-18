//! 单日统一 Markdown 渲染：固定 agent 顺序（Codex→Cursor→Claude Code→Gemini）、
//! 会话按当日首条消息排序、正文超 2000 字符截断并标注原文字数。

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate, Weekday};

use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};

/// 渲染阶段单条消息正文的字符上限。
pub const DISPLAY_CAP: usize = 2000;

/// 字符边界安全截断：超过 `cap` 字符时截取并追加原文字数标注。
pub fn truncate(text: &str, cap: usize) -> String {
    let total = text.chars().count();
    if total <= cap {
        return text.to_string();
    }
    let body: String = text.chars().take(cap).collect();
    format!("{body}…（已截断，原文共 {total} 字符）")
}

fn weekday_zh(date: NaiveDate) -> &'static str {
    match date.weekday() {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

/// 会话 id 展示形式：前 8 字符，空 id 显示 unknown。
fn short_id(id: &str) -> String {
    if id.is_empty() {
        return "unknown".to_string();
    }
    id.chars().take(8).collect()
}

/// 渲染一天内所有会话。调用方保证传入的 session 只含当日消息。
pub fn render_daily(date: NaiveDate, sessions: &[Session]) -> String {
    let mut out = format!("# {date} {} · AI 对话记录\n\n", weekday_zh(date));

    // 统计行
    let message_count: usize = sessions.iter().map(|s| s.messages.len()).sum();
    let mut per_agent: BTreeMap<AgentKind, usize> = BTreeMap::new();
    for s in sessions {
        *per_agent.entry(s.agent).or_default() += 1;
    }
    let agent_part = per_agent
        .iter()
        .map(|(kind, n)| format!("{} × {}", kind.display_name(), n))
        .collect::<Vec<_>>()
        .join(" · ");
    if agent_part.is_empty() {
        out.push_str(&format!(
            "> 本会话日共 {} 个会话 · 消息 {} 条\n",
            sessions.len(),
            message_count
        ));
    } else {
        out.push_str(&format!(
            "> 本会话日共 {} 个会话 · {} · 消息 {} 条\n",
            sessions.len(),
            agent_part,
            message_count
        ));
    }

    // 按固定 agent 顺序输出；会话按当日首条消息排序
    for kind in AgentKind::ALL {
        let mut group: Vec<&Session> = sessions.iter().filter(|s| s.agent == kind).collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by_key(|s| s.started_at);
        out.push_str(&format!("\n## {}\n", kind.display_name()));
        for session in group {
            out.push_str(&render_session(session));
        }
    }
    out
}

fn render_session(session: &Session) -> String {
    let first = session.messages.first().map(|m| m.timestamp.format("%H:%M"));
    let last = session.messages.last().map(|m| m.timestamp.format("%H:%M"));
    let mut out = format!(
        "\n### 会话 `{}` · {}\n\n- 时间: {} – {} · {} 条消息\n",
        short_id(&session.id),
        session.project,
        first.map(|f| f.to_string()).unwrap_or_default(),
        last.map(|l| l.to_string()).unwrap_or_default(),
        session.messages.len(),
    );
    for message in &session.messages {
        out.push_str(&render_message(message));
    }
    out
}

fn render_message(message: &Message) -> String {
    let time = message.timestamp.format("%H:%M");
    match &message.content {
        MessageContent::Text(text) => {
            let icon = match message.role {
                Role::User => "👤 用户",
                Role::Assistant => "🤖 助手",
                Role::System => "⚙️ 系统",
            };
            format!("\n**{icon}** `{time}`\n\n{}\n", truncate(text, DISPLAY_CAP))
        }
        MessageContent::ToolUse { name, summary } => {
            format!("\n**🔧 工具** `{time}` · `{name}`\n\n`{summary}`\n")
        }
    }
}

/// 渲染索引页。`days` 只包含有数据的日期：(日期， 会话数， 消息数)。
pub fn render_index(
    range: &DateRange,
    days: &[(NaiveDate, usize, usize)],
    warning_count: usize,
) -> String {
    let mut out = format!(
        "# AI 对话记录索引（{} ~ {}）\n\n| 日期 | 会话数 | 消息数 |\n| --- | --- | --- |\n",
        range.start, range.end
    );
    let mut total_sessions = 0usize;
    let mut total_messages = 0usize;
    for date in range.days() {
        match days.iter().find(|(d, _, _)| *d == date) {
            Some((_, s, m)) => {
                total_sessions += s;
                total_messages += m;
                out.push_str(&format!("| [{date}]({date}.md) | {s} | {m} |\n"));
            }
            None => out.push_str(&format!("| {date} | 无记录 | - |\n")),
        }
    }
    out.push_str(&format!("| 合计 | {total_sessions} | {total_messages} |\n"));
    if warning_count > 0 {
        out.push_str(&format!(
            "\n> ⚠️ 采集过程中有 {warning_count} 条解析警告（详见 stderr 输出）\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{DISPLAY_CAP, render_daily, render_index, truncate};
    use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};
    use chrono::{DateTime, Local, NaiveDate, TimeZone};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn at(day: &str, hm: &str) -> DateTime<Local> {
        let naive = NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(
                hm[..2].parse().unwrap(),
                hm[3..5].parse().unwrap(),
                0,
            )
            .unwrap();
        Local.from_local_datetime(&naive).unwrap()
    }

    fn text_msg(role: Role, day: &str, hm: &str, text: &str) -> Message {
        Message {
            role,
            timestamp: at(day, hm),
            content: MessageContent::Text(text.to_string()),
        }
    }

    fn tool_msg(day: &str, hm: &str, name: &str, summary: &str) -> Message {
        Message {
            role: Role::Assistant,
            timestamp: at(day, hm),
            content: MessageContent::ToolUse {
                name: name.to_string(),
                summary: summary.to_string(),
            },
        }
    }

    fn codex_session() -> Session {
        Session {
            agent: AgentKind::Codex,
            project: "/home/zhaoyilun/shop-api".to_string(),
            id: "0f3a2b7c-aaaa-bbbb-cccc-dddddddddddd".to_string(),
            started_at: at("2026-07-15", "10:05"),
            messages: vec![
                text_msg(Role::User, "2026-07-15", "10:05", "加个健康检查接口"),
                text_msg(Role::Assistant, "2026-07-15", "10:06", "好的，我来添加 /healthz 路由。"),
                tool_msg("2026-07-15", "10:07", "Write", "{\"file_path\":\"/src/health.rs\"}"),
            ],
        }
    }

    fn claude_session() -> Session {
        Session {
            agent: AgentKind::Claude,
            project: "/home/zhaoyilun/notes-app".to_string(),
            id: "a1b2c3d4".to_string(),
            started_at: at("2026-07-15", "11:01"),
            messages: vec![
                text_msg(Role::User, "2026-07-15", "11:01", "帮我加全文搜索"),
                text_msg(Role::Assistant, "2026-07-15", "11:09", "已集成 FTS5。"),
            ],
        }
    }

    // ---------- truncate ----------

    #[test]
    fn truncate_short_text_unchanged() {
        assert_eq!(truncate("短文本", DISPLAY_CAP), "短文本");
    }

    #[test]
    fn truncate_long_text_char_boundary_safe_with_marker() {
        let long = "汉".repeat(2500);
        let out = truncate(&long, DISPLAY_CAP);
        assert!(out.ends_with("…（已截断，原文共 2500 字符）"));
        let body = out.trim_end_matches("…（已截断，原文共 2500 字符）");
        assert_eq!(body.chars().count(), DISPLAY_CAP);
    }

    // ---------- render_daily ----------

    #[test]
    fn render_daily_golden() {
        let sessions = vec![codex_session(), claude_session()];
        let md = render_daily(d("2026-07-15"), &sessions);
        let expected = "# 2026-07-15 周三 · AI 对话记录\n\
            \n\
            > 本会话日共 2 个会话 · Codex × 1 · Claude Code × 1 · 消息 5 条\n\
            \n\
            ## Codex\n\
            \n\
            ### 会话 `0f3a2b7c` · /home/zhaoyilun/shop-api\n\
            \n\
            - 时间: 10:05 – 10:07 · 3 条消息\n\
            \n\
            **👤 用户** `10:05`\n\
            \n\
            加个健康检查接口\n\
            \n\
            **🤖 助手** `10:06`\n\
            \n\
            好的，我来添加 /healthz 路由。\n\
            \n\
            **🔧 工具** `10:07` · `Write`\n\
            \n\
            `{\"file_path\":\"/src/health.rs\"}`\n\
            \n\
            ## Claude Code\n\
            \n\
            ### 会话 `a1b2c3d4` · /home/zhaoyilun/notes-app\n\
            \n\
            - 时间: 11:01 – 11:09 · 2 条消息\n\
            \n\
            **👤 用户** `11:01`\n\
            \n\
            帮我加全文搜索\n\
            \n\
            **🤖 助手** `11:09`\n\
            \n\
            已集成 FTS5。\n";
        assert_eq!(md, expected);
    }

    #[test]
    fn render_daily_skips_agents_without_sessions() {
        let sessions = vec![claude_session()];
        let md = render_daily(d("2026-07-15"), &sessions);
        assert!(!md.contains("## Codex"));
        assert!(!md.contains("## Cursor"));
        assert!(!md.contains("## Gemini"));
        assert!(md.contains("## Claude Code"));
        assert!(md.contains("> 本会话日共 1 个会话 · Claude Code × 1 · 消息 2 条"));
    }

    #[test]
    fn render_daily_orders_sessions_by_first_message() {
        let mut late = codex_session();
        late.id = "ffffffff-late".to_string();
        // 两个 codex 会话，故意乱序传入
        let mut early = codex_session();
        early.id = "00000000-early".to_string();
        early.started_at = at("2026-07-15", "08:00");
        early.messages = vec![text_msg(Role::User, "2026-07-15", "08:00", "早会")];
        let md = render_daily(d("2026-07-15"), &[late, early]);
        let pos_early = md.find("00000000").unwrap();
        let pos_late = md.find("ffffffff").unwrap();
        assert!(pos_early < pos_late);
    }

    #[test]
    fn render_daily_truncates_long_message_body() {
        let mut s = claude_session();
        s.messages = vec![text_msg(Role::User, "2026-07-15", "11:01", &"长".repeat(3000))];
        let md = render_daily(d("2026-07-15"), &[s]);
        assert!(md.contains("…（已截断，原文共 3000 字符）"));
    }

    #[test]
    fn weekday_rendered_in_chinese() {
        let sessions = vec![codex_session()];
        assert!(render_daily(d("2026-07-12"), &sessions).contains("周日"));
        assert!(render_daily(d("2026-07-13"), &sessions).contains("周一"));
        assert!(render_daily(d("2026-07-18"), &sessions).contains("周六"));
    }

    // ---------- render_index ----------

    #[test]
    fn render_index_marks_empty_days_and_totals() {
        let range = DateRange::new(d("2026-07-13"), d("2026-07-15")).unwrap();
        // 只有 7-14 有数据：3 会话 42 消息
        let days = vec![(d("2026-07-14"), 3usize, 42usize)];
        let md = render_index(&range, &days, 0);
        let expected = "# AI 对话记录索引（2026-07-13 ~ 2026-07-15）\n\
            \n\
            | 日期 | 会话数 | 消息数 |\n\
            | --- | --- | --- |\n\
            | 2026-07-13 | 无记录 | - |\n\
            | [2026-07-14](2026-07-14.md) | 3 | 42 |\n\
            | 2026-07-15 | 无记录 | - |\n\
            | 合计 | 3 | 42 |\n";
        assert_eq!(md, expected);
    }

    #[test]
    fn render_index_appends_warning_footnote() {
        let range = DateRange::new(d("2026-07-13"), d("2026-07-13")).unwrap();
        let md = render_index(&range, &[], 2);
        assert!(md.contains("> ⚠️ 采集过程中有 2 条解析警告"));
        let no_warn = render_index(&range, &[], 0);
        assert!(!no_warn.contains("⚠️"));
    }
}
