//! Configuration: `~/.config/buddy/config.toml` (optional).
//! Priority: CLI flag > config > built-in defaults.
//! Falls back to legacy `~/.config/ai-weekly-report/config.toml` when the new path is missing.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub out_dir: Option<String>,
    pub llm: LlmConfig,
    pub wr: WrConfig,
    pub signoff: SignoffConfig,
    // ---- legacy flat fields (ai-weekly-report) ----
    pub days: Option<u32>,
    pub backend: Option<String>,
    pub cli_name: Option<String>,
    pub cli_cmd: Option<String>,
    pub timeout_secs: Option<u64>,
    pub api_base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_model: Option<String>,
    pub template: Option<PathBuf>,
    pub include_prompt_history: Option<bool>,
    pub mail_to: Option<Vec<String>>,
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
    pub allowed_workspaces: Option<Vec<String>>,
    pub max_auto_todos: Option<usize>,
    pub act_timeout_secs: Option<u64>,
    pub dry_run: Option<bool>,
    pub min_confidence: Option<String>,
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

    pub fn legacy_path(home: &Path) -> PathBuf {
        home.join(".config")
            .join("ai-weekly-report")
            .join("config.toml")
    }

    /// Load new path first; if missing, try legacy path.
    pub fn load_for_home(home: &Path) -> Result<Config, ConfigError> {
        let primary = Self::default_path(home);
        if primary.exists() {
            return Self::load(&primary);
        }
        let legacy = Self::legacy_path(home);
        if legacy.exists() {
            return Self::load(&legacy);
        }
        Ok(Config::default())
    }

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

    pub fn effective_llm_backend(&self) -> Option<&str> {
        self.llm.backend.as_deref().or(self.backend.as_deref())
    }

    pub fn effective_cli_name(&self) -> Option<&str> {
        self.llm.cli_name.as_deref().or(self.cli_name.as_deref())
    }

    pub fn effective_cli_cmd(&self) -> Option<&str> {
        self.llm.cli_cmd.as_deref().or(self.cli_cmd.as_deref())
    }

    pub fn effective_timeout_secs(&self) -> Option<u64> {
        self.llm.timeout_secs.or(self.timeout_secs)
    }

    pub fn effective_api_base_url(&self) -> Option<&str> {
        self.llm
            .api_base_url
            .as_deref()
            .or(self.api_base_url.as_deref())
    }

    pub fn effective_api_key_env(&self) -> Option<&str> {
        self.llm
            .api_key_env
            .as_deref()
            .or(self.api_key_env.as_deref())
    }

    pub fn effective_api_model(&self) -> Option<&str> {
        self.llm
            .api_model
            .as_deref()
            .or(self.api_model.as_deref())
    }

    pub fn effective_wr_days(&self) -> Option<u32> {
        self.wr.days.or(self.days)
    }

    pub fn effective_wr_mail_to(&self) -> Option<&[String]> {
        self.wr
            .mail_to
            .as_deref()
            .or(self.mail_to.as_deref())
    }

    pub fn effective_wr_include_prompt_history(&self) -> bool {
        self.wr
            .include_prompt_history
            .or(self.include_prompt_history)
            .unwrap_or(false)
    }

    pub fn effective_wr_template(&self) -> Option<&Path> {
        self.wr
            .template
            .as_deref()
            .or(self.llm.template.as_deref())
            .or(self.template.as_deref())
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
allowed_workspaces = ["/tmp/proj"]
max_auto_todos = 2
dry_run = true
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
        assert_eq!(config.signoff.window_hours, Some(24));
        assert_eq!(config.signoff.max_auto_todos, Some(2));
        assert_eq!(config.signoff.dry_run, Some(true));
    }

    #[test]
    fn legacy_flat_fields_still_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
backend = "cli"
days = 7
mail_to = ["old@example.com"]
"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.effective_llm_backend(), Some("cli"));
        assert_eq!(config.effective_wr_days(), Some(7));
        assert_eq!(
            config.effective_wr_mail_to(),
            Some(["old@example.com".to_string()].as_slice())
        );
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
