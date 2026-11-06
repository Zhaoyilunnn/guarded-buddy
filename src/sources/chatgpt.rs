//! Read the newest ChatGPT snapshot across live and archived device homes.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use serde_json::Value;

use super::{HistorySource, SourceError, cap_ingest};
use crate::domain::{AgentKind, DateRange, Message, MessageContent, Role, Session};
use crate::web::{Snapshot, history_root};

pub struct ChatgptSource {
    roots: Vec<PathBuf>,
}

impl ChatgptSource {
    pub fn new(home: &Path) -> Self {
        Self {
            roots: vec![history_root(home)],
        }
    }
    pub fn from_homes(homes: &[PathBuf]) -> Self {
        Self {
            roots: homes.iter().map(|h| history_root(h)).collect(),
        }
    }
    pub fn with_archive(mut self, archive: Option<&Path>) -> Self {
        if let Some(archive) = archive {
            self.roots.push(archive.join("chatgpt"));
        }
        self
    }
}

fn timestamp(value: &Value) -> Option<DateTime<Local>> {
    let seconds = value.as_f64()?;
    if !seconds.is_finite() {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis((seconds * 1000.0) as i64)
        .map(|t| t.with_timezone(&Local))
}

pub fn parse(snapshot: &Snapshot) -> Result<Session, String> {
    let body = &snapshot.conversation;
    let mapping = body["mapping"].as_object().ok_or("missing mapping")?;
    let mut current = body["current_node"]
        .as_str()
        .ok_or("missing current branch")?;
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    loop {
        if !seen.insert(current) {
            return Err("cycle in current branch".into());
        }
        let node = mapping.get(current).ok_or("missing branch node")?;
        nodes.push(node);
        match node["parent"].as_str() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    let mut messages = Vec::new();
    for node in nodes.into_iter().rev() {
        let msg = &node["message"];
        if msg["metadata"]["is_visually_hidden_from_conversation"].as_bool() == Some(true) {
            continue;
        }
        let role = match msg["author"]["role"].as_str() {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        // Internal reasoning/tool channels are not part of the visible dialogue.
        if msg["channel"].as_str().is_some_and(|c| c != "final") {
            continue;
        }
        let parts = msg["content"]["parts"].as_array();
        let text = parts
            .map(|p| {
                p.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let time =
            timestamp(&msg["create_time"]).ok_or("visible message has no valid creation time")?;
        messages.push(Message {
            role,
            timestamp: time,
            content: MessageContent::Text(cap_ingest(&text)),
        });
    }
    let started_at = messages
        .first()
        .map(|m| m.timestamp)
        .or_else(|| timestamp(&body["create_time"]))
        .ok_or("missing conversation time")?;
    // Select one complete snapshot before rendering its title and original link.
    Ok(Session {
        agent: AgentKind::Chatgpt,
        project: format!(
            "{} · https://chatgpt.com/c/{}",
            body["title"]
                .as_str()
                .unwrap_or("Untitled")
                .replace(['\n', '\r'], " "),
            snapshot.id()
        ),
        id: format!("{}:{}", snapshot.id(), snapshot.account),
        started_at,
        messages,
    })
}

impl HistorySource for ChatgptSource {
    fn kind(&self) -> AgentKind {
        AgentKind::Chatgpt
    }
    fn root(&self) -> &Path {
        &self.roots[0]
    }
    fn collect(&self, range: &DateRange, warnings: &mut Vec<SourceError>) -> Vec<Session> {
        let mut latest: BTreeMap<(String, String), (Snapshot, PathBuf)> = BTreeMap::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        warnings.push(SourceError::Parse {
                            path: root.clone(),
                            line: 0,
                            msg: e.to_string(),
                        });
                        continue;
                    }
                };
                if !entry.file_type().is_file()
                    || entry.path().extension().is_none_or(|x| x != "json")
                {
                    continue;
                }
                let path = entry.path();
                let result = (|| -> anyhow::Result<Snapshot> {
                    let file = std::fs::File::open(path)?;
                    let snapshot: Snapshot =
                        serde_json::from_reader(std::io::BufReader::new(file))?;
                    snapshot.validate()?;
                    Ok(snapshot)
                })();
                match result {
                    Ok(snapshot) => {
                        let key = (snapshot.account.clone(), snapshot.id().to_string());
                        let replace = latest
                            .get(&key)
                            .is_none_or(|(old, _)| snapshot.newer_than(old));
                        if replace {
                            latest.insert(key, (snapshot, path.to_path_buf()));
                        }
                    }
                    Err(e) => warnings.push(SourceError::Parse {
                        path: path.to_path_buf(),
                        line: 0,
                        msg: e.to_string(),
                    }),
                }
            }
        }
        latest
            .into_values()
            .filter_map(|(snapshot, path)| match parse(&snapshot) {
                Ok(mut session) => {
                    session.messages.retain(|m| range.contains_ts(&m.timestamp));
                    if session.messages.is_empty() {
                        None
                    } else {
                        Some(session)
                    }
                }
                Err(msg) => {
                    warnings.push(SourceError::Parse { path, line: 0, msg });
                    None
                }
            })
            .collect()
    }
}
