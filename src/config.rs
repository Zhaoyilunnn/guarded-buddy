//! Configuration file: `~/.config/ai-weekly-report/config.toml` (optional).
//! Priority: CLI flag > config > built-in defaults. Unknown fields are tolerated (forward compatible).

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Output root directory (relative to cwd or absolute), default "out".
    pub out_dir: Option<String>,
    /// Default lookback days, default 7.
    pub days: Option<u32>,
    /// Summarization backend: "cli" | "api".
    pub backend: Option<String>,
    /// CLI backend preset name: codex | claude | agy | gemini.
    pub cli_name: Option<String>,
    /// Custom CLI command template (overrides cli_name preset).
    pub cli_cmd: Option<String>,
    /// External CLI timeout in seconds, default 600.
    pub timeout_secs: Option<u64>,
    pub api_base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_model: Option<String>,
    /// Weekly report template file path.
    pub template: Option<PathBuf>,
    /// Gemini/antigravity: whether to include history.jsonl.
    pub include_prompt_history: Option<bool>,
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
    /// Default config path: `<home>/.config/ai-weekly-report/config.toml`.
    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".config")
            .join("ai-weekly-report")
            .join("config.toml")
    }

    /// Load config; returns all defaults (every field None) when the file is missing.
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
    use super::{Config, ConfigError};

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::load(&tmp.path().join("nonexistent.toml")).unwrap();
        assert!(config.out_dir.is_none());
        assert!(config.backend.is_none());
        assert!(config.include_prompt_history.is_none());
    }

    #[test]
    fn parses_full_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
out_dir = "artifacts"
days = 14
backend = "api"
cli_name = "codex"
cli_cmd = "my-wrapper --fast"
timeout_secs = 300
api_base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
api_model = "deepseek-chat"
template = "/home/me/weekly-template.md"
include_prompt_history = true
"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.out_dir.as_deref(), Some("artifacts"));
        assert_eq!(config.days, Some(14));
        assert_eq!(config.backend.as_deref(), Some("api"));
        assert_eq!(config.cli_name.as_deref(), Some("codex"));
        assert_eq!(config.cli_cmd.as_deref(), Some("my-wrapper --fast"));
        assert_eq!(config.timeout_secs, Some(300));
        assert_eq!(config.api_base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(config.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(config.api_model.as_deref(), Some("deepseek-chat"));
        assert_eq!(
            config.template.as_deref(),
            Some(std::path::Path::new("/home/me/weekly-template.md"))
        );
        assert_eq!(config.include_prompt_history, Some(true));
    }

    #[test]
    fn partial_config_leaves_rest_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "backend = \"api\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.backend.as_deref(), Some("api"));
        assert!(config.days.is_none());
        assert!(config.api_model.is_none());
    }

    #[test]
    fn bad_toml_returns_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "backend = [unclosed\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unknown_fields_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "future_field = 1\nbackend = \"cli\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.backend.as_deref(), Some("cli"));
    }

    #[test]
    fn default_path_under_config_dir() {
        let home = std::path::Path::new("/fake/home");
        assert_eq!(
            Config::default_path(home),
            std::path::Path::new("/fake/home/.config/ai-weekly-report/config.toml")
        );
    }
}
