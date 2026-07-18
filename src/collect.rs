//! 采集编排：从所有数据源收集会话 → 按本地日分桶（跨午夜拆分）→
//! 写 `<out>/<range>/<date>.md` 与 `index.md`（幂等覆盖）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::NaiveDate;

use crate::domain::{DateRange, Message, Session};
use crate::render::daily::{render_daily, render_index};
use crate::sources::{HistorySource, SourceError};

/// 一天的桶：当日有消息的会话（每条 session 只含当日消息）。
#[derive(Debug)]
pub struct DayBucket {
    pub date: NaiveDate,
    pub sessions: Vec<Session>,
}

impl DayBucket {
    pub fn message_count(&self) -> usize {
        self.sessions.iter().map(|s| s.messages.len()).sum()
    }
}

/// 纯函数：会话按消息时间戳的本地日期拆桶；范围外消息丢弃；
/// 无范围内消息的会话不产生任何桶。
pub fn group_by_day(sessions: Vec<Session>, range: &DateRange) -> Vec<DayBucket> {
    let mut map: BTreeMap<NaiveDate, Vec<Session>> = BTreeMap::new();
    for session in sessions {
        let mut by_day: BTreeMap<NaiveDate, Vec<Message>> = BTreeMap::new();
        for m in session.messages {
            if range.contains_ts(&m.timestamp) {
                by_day.entry(m.timestamp.date_naive()).or_default().push(m);
            }
        }
        for (date, mut messages) in by_day {
            messages.sort_by_key(|m| m.timestamp);
            let started_at = messages[0].timestamp;
            map.entry(date).or_default().push(Session {
                agent: session.agent,
                project: session.project.clone(),
                id: session.id.clone(),
                started_at,
                messages,
            });
        }
    }
    map.into_iter()
        .map(|(date, mut sessions)| {
            sessions.sort_by_key(|s| s.started_at);
            DayBucket { date, sessions }
        })
        .collect()
}

#[derive(Debug)]
pub struct CollectOutcome {
    /// `out_root/<range.dir_name()>`
    pub dir: PathBuf,
    pub buckets: Vec<DayBucket>,
    pub warnings: Vec<SourceError>,
}

/// 全量采集并落盘（幂等：同名文件覆盖）。
pub fn collect(
    sources: &[Box<dyn HistorySource>],
    range: &DateRange,
    out_root: &std::path::Path,
) -> std::io::Result<CollectOutcome> {
    let mut warnings = Vec::new();
    let mut all = Vec::new();
    for source in sources {
        all.extend(source.collect(range, &mut warnings));
    }
    let buckets = group_by_day(all, range);

    let dir = out_root.join(range.dir_name());
    std::fs::create_dir_all(&dir)?;
    for bucket in &buckets {
        let md = render_daily(bucket.date, &bucket.sessions);
        std::fs::write(dir.join(format!("{}.md", bucket.date)), md)?;
    }
    let days: Vec<(NaiveDate, usize, usize)> = buckets
        .iter()
        .map(|b| (b.date, b.sessions.len(), b.message_count()))
        .collect();
    std::fs::write(dir.join("index.md"), render_index(range, &days, warnings.len()))?;

    Ok(CollectOutcome {
        dir,
        buckets,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::{CollectOutcome, collect, group_by_day};
    use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};
    use crate::sources::{HistorySource, SourceError};
    use chrono::{DateTime, Local, NaiveDate, TimeZone};
    use std::path::{Path, PathBuf};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn at(day: &str, hm: &str) -> DateTime<Local> {
        let naive = NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(hm[..2].parse().unwrap(), hm[3..5].parse().unwrap(), 0)
            .unwrap();
        Local.from_local_datetime(&naive).unwrap()
    }

    fn msg(day: &str, hm: &str, text: &str) -> Message {
        Message {
            role: Role::User,
            timestamp: at(day, hm),
            content: MessageContent::Text(text.to_string()),
        }
    }

    fn session(id: &str, messages: Vec<Message>) -> Session {
        Session {
            agent: AgentKind::Codex,
            project: "proj".to_string(),
            id: id.to_string(),
            started_at: messages[0].timestamp,
            messages,
        }
    }

    fn range() -> DateRange {
        DateRange::new(d("2026-07-12"), d("2026-07-18")).unwrap()
    }

    // ---------- group_by_day ----------

    #[test]
    fn group_by_day_splits_session_spanning_midnight() {
        let s = session(
            "s1",
            vec![
                msg("2026-07-15", "23:50", "跨午夜的开始"),
                msg("2026-07-16", "00:10", "跨午夜的继续"),
            ],
        );
        let buckets = group_by_day(vec![s], &range());
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].date, d("2026-07-15"));
        assert_eq!(buckets[0].sessions[0].messages.len(), 1);
        assert_eq!(
            buckets[0].sessions[0].messages[0].text().unwrap(),
            "跨午夜的开始"
        );
        assert_eq!(buckets[1].date, d("2026-07-16"));
        assert_eq!(
            buckets[1].sessions[0].messages[0].text().unwrap(),
            "跨午夜的继续"
        );
        // 每日桶内 started_at = 当日首条消息
        assert_eq!(buckets[1].sessions[0].started_at, at("2026-07-16", "00:10"));
    }

    #[test]
    fn group_by_day_drops_out_of_range_messages() {
        let s = session(
            "s1",
            vec![
                msg("2026-07-01", "10:00", "范围外"),
                msg("2026-07-15", "10:00", "范围内"),
            ],
        );
        let buckets = group_by_day(vec![s], &range());
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].date, d("2026-07-15"));
        assert_eq!(buckets[0].sessions[0].messages.len(), 1);
    }

    #[test]
    fn group_by_day_skips_sessions_without_in_range_messages() {
        let s = session("s1", vec![msg("2026-07-01", "10:00", "范围外")]);
        let buckets = group_by_day(vec![s], &range());
        assert!(buckets.is_empty());
    }

    #[test]
    fn group_by_day_sorts_sessions_within_day() {
        let late = session("late", vec![msg("2026-07-15", "15:00", "晚")]);
        let early = session("early", vec![msg("2026-07-15", "09:00", "早")]);
        let buckets = group_by_day(vec![late, early], &range());
        assert_eq!(buckets[0].sessions[0].id, "early");
        assert_eq!(buckets[0].sessions[1].id, "late");
    }

    // ---------- collect（stub 数据源） ----------

    struct StubSource {
        kind: AgentKind,
        sessions: Vec<Session>,
        stub_warnings: usize,
    }

    impl HistorySource for StubSource {
        fn kind(&self) -> AgentKind {
            self.kind
        }
        fn root(&self) -> &Path {
            Path::new("/stub")
        }
        fn collect(&self, _range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
            for i in 0..self.stub_warnings {
                warnings.push(SourceError::Parse {
                    path: PathBuf::from("/stub/x.jsonl"),
                    line: i + 1,
                    msg: "bad line".to_string(),
                });
            }
            self.sessions.clone()
        }
    }

    fn boxed(s: StubSource) -> Box<dyn HistorySource> {
        Box::new(s)
    }

    fn two_day_stubs() -> Vec<Box<dyn HistorySource>> {
        vec![
            boxed(StubSource {
                kind: AgentKind::Codex,
                sessions: vec![session("c1", vec![msg("2026-07-15", "10:00", "codex 消息")])],
                stub_warnings: 0,
            }),
            boxed(StubSource {
                kind: AgentKind::Claude,
                sessions: vec![session(
                    "a1",
                    vec![
                        msg("2026-07-15", "11:00", "claude 第一天"),
                        msg("2026-07-16", "09:00", "claude 第二天"),
                    ],
                )],
                stub_warnings: 1,
            }),
        ]
    }

    #[test]
    fn collect_writes_day_files_index_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome: CollectOutcome =
            collect(&two_day_stubs(), &range(), tmp.path()).expect("collect ok");

        let dir = tmp.path().join("2026-07-12_2026-07-18");
        assert_eq!(outcome.dir, dir);
        // 只有有数据的日期写文件
        let d15 = std::fs::read_to_string(dir.join("2026-07-15.md")).unwrap();
        assert!(d15.contains("# 2026-07-15 周三 · AI 对话记录"));
        assert!(d15.contains("codex 消息"));
        assert!(d15.contains("claude 第一天"));
        assert!(!d15.contains("claude 第二天"));
        let d16 = std::fs::read_to_string(dir.join("2026-07-16.md")).unwrap();
        assert!(d16.contains("claude 第二天"));
        assert!(!dir.join("2026-07-12.md").exists(), "空日不写文件");
        // index.md：合计行 + 警告脚注
        let index = std::fs::read_to_string(dir.join("index.md")).unwrap();
        assert!(index.contains("| 合计 | 3 | 3 |"));
        assert!(index.contains("⚠️"));
        assert!(index.contains("[2026-07-15](2026-07-15.md)"));
        // 警告被带出
        assert_eq!(outcome.warnings.len(), 1);
        // 幂等：再跑一遍内容一致
        let outcome2 = collect(&two_day_stubs(), &range(), tmp.path()).expect("collect ok");
        assert_eq!(
            std::fs::read_to_string(outcome2.dir.join("2026-07-15.md")).unwrap(),
            d15
        );
        assert_eq!(outcome2.buckets.len(), 2);
    }

    #[test]
    fn collect_with_no_data_writes_only_index() {
        let tmp = tempfile::tempdir().unwrap();
        let sources: Vec<Box<dyn HistorySource>> = vec![boxed(StubSource {
            kind: AgentKind::Codex,
            sessions: vec![],
            stub_warnings: 0,
        })];
        let outcome = collect(&sources, &range(), tmp.path()).unwrap();
        assert!(outcome.buckets.is_empty());
        let index = std::fs::read_to_string(outcome.dir.join("index.md")).unwrap();
        assert!(index.contains("| 合计 | 0 | 0 |"));
        assert!(!index.contains("⚠️"));
    }
}
