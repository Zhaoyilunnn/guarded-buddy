//! Configuration: `~/.config/buddy/config.toml` (optional).
//! Priority: CLI flag > config > built-in defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub out_dir: Option<String>,
    pub llm: LlmConfig,
    pub wr: WrConfig,
    pub signoff: SignoffConfig,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LlmConfig {
    pub backend: Option<String>,
    pub cli_name: Option<String>,
    pub cli_cmd: Option<String>,
    pub timeout_secs: Option<u64>,
    pub api_base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_model: Option<String>,
    pub template: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WrConfig {
    pub days: Option<u32>,
    pub mail_to: Option<Vec<String>>,
    pub include_prompt_history: Option<bool>,
    pub template: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SignoffConfig {
    pub window_hours: Option<u64>,
    pub mail_to: Option<Vec<String>>,
    /// Per-workspace trust overrides; unlisted paths default to `yolo`.
    pub workspaces: Option<Vec<WorkspaceTrustConfig>>,
    pub max_auto_todos: Option<usize>,
    pub act_timeout_secs: Option<u64>,
    pub dry_run: Option<bool>,
    pub min_confidence: Option<String>,
}

/// One `[[signoff.workspaces]]` entry.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTrustConfig {
    pub path: String,
    #[serde(default)]
    pub trust: crate::signoff::WorkspaceTrust,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse error at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl Config {
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".config").join("buddy").join("config.toml")
    }

    /// Load config; missing file → all defaults.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::load(&tmp.path().join("nonexistent.toml")).unwrap();
        assert!(config.out_dir.is_none());
        assert!(config.llm.backend.is_none());
        assert!(config.wr.mail_to.is_none());
        assert!(config.signoff.mail_to.is_none());
    }

    #[test]
    fn parses_layered_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
out_dir = "artifacts"
[llm]
backend = "api"
cli_name = "codex"
[wr]
days = 14
mail_to = ["weekly@example.com"]
[signoff]
window_hours = 24
mail_to = ["me@example.com"]
max_auto_todos = 2
act_timeout_secs = 7200
dry_run = true

[[signoff.workspaces]]
path = "/tmp/proj"
trust = "workspace-write"
"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.out_dir.as_deref(), Some("artifacts"));
        assert_eq!(config.llm.backend.as_deref(), Some("api"));
        assert_eq!(config.wr.days, Some(14));
        assert_eq!(
            config.wr.mail_to.as_deref(),
            Some(["weekly@example.com".to_string()].as_slice())
        );
        assert_eq!(
            config.signoff.mail_to.as_deref(),
            Some(["me@example.com".to_string()].as_slice())
        );
        let ws = config.signoff.workspaces.as_ref().unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].path, "/tmp/proj");
        assert_eq!(
            ws[0].trust,
            crate::signoff::WorkspaceTrust::WorkspaceWrite
        );
        assert_eq!(config.signoff.act_timeout_secs, Some(7200));
        assert_eq!(config.signoff.dry_run, Some(true));
    }

    #[test]
    fn default_path_under_buddy_config_dir() {
        let home = std::path::Path::new("/fake/home");
        assert_eq!(
            Config::default_path(home),
            std::path::Path::new("/fake/home/.config/buddy/config.toml")
        );
    }
}
