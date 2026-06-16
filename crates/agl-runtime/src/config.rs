use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::AgentLibrePaths;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLibreRuntimeConfig {
    pub paths: AgentLibrePaths,
    pub logging: AgentLibreLoggingConfig,
    pub history: AgentLibreHistoryConfig,
}

impl AgentLibreRuntimeConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_paths(AgentLibrePaths::from_env()?)
    }

    pub fn from_paths(paths: AgentLibrePaths) -> Result<Self> {
        let file_config = AgentLibreRuntimeConfigFile::read(&paths.runtime_config_path())?;
        Ok(Self {
            paths,
            logging: AgentLibreLoggingConfig::from_file_and_env(file_config.logging),
            history: file_config.history.unwrap_or_default(),
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentLibreRuntimeConfigFile {
    logging: Option<AgentLibreLoggingConfigFile>,
    history: Option<AgentLibreHistoryConfig>,
}

impl AgentLibreRuntimeConfigFile {
    fn read(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content)
                .with_context(|| format!("failed to parse runtime config {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err)
                .with_context(|| format!("failed to read runtime config {}", path.display())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLibreLoggingConfig {
    pub level: String,
    pub format: AgentLibreLogFormat,
    pub file: bool,
    pub stderr: AgentLibreStderrLogMode,
    pub include_message_text: bool,
}

impl AgentLibreLoggingConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        config.apply_env();
        config
    }

    fn from_file_and_env(file_config: Option<AgentLibreLoggingConfigFile>) -> Self {
        let mut config = Self::default();
        if let Some(file_config) = file_config {
            config.apply_file(file_config);
        }
        config.apply_env();
        config
    }

    fn apply_file(&mut self, file_config: AgentLibreLoggingConfigFile) {
        if let Some(level) = file_config.level {
            self.level = level;
        }
        if let Some(format) = file_config.format {
            self.format = format;
        }
        if let Some(file) = file_config.file {
            self.file = file;
        }
        if let Some(stderr) = file_config.stderr {
            self.stderr = stderr;
        }
        if let Some(include_message_text) = file_config.include_message_text {
            self.include_message_text = include_message_text;
        }
    }

    fn apply_env(&mut self) {
        if let Ok(format) = std::env::var("AGL_LOG_FORMAT") {
            self.format = AgentLibreLogFormat::from_env_value(&format).unwrap_or(self.format);
        }
        if let Ok(stderr) = std::env::var("AGL_LOG_STDERR") {
            self.stderr = AgentLibreStderrLogMode::from_env_value(&stderr).unwrap_or(self.stderr);
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentLibreLoggingConfigFile {
    level: Option<String>,
    format: Option<AgentLibreLogFormat>,
    file: Option<bool>,
    stderr: Option<AgentLibreStderrLogMode>,
    include_message_text: Option<bool>,
}

impl Default for AgentLibreLoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: AgentLibreLogFormat::Compact,
            file: true,
            stderr: AgentLibreStderrLogMode::Auto,
            include_message_text: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLibreLogFormat {
    Compact,
    Json,
}

impl AgentLibreLogFormat {
    fn from_env_value(value: &str) -> Option<Self> {
        match value {
            "compact" => Some(Self::Compact),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLibreStderrLogMode {
    Auto,
    Always,
    Never,
}

impl AgentLibreStderrLogMode {
    fn from_env_value(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLibreHistoryConfig {
    pub enabled: bool,
}

impl Default for AgentLibreHistoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_file_overrides_logging_and_history() {
        let root = std::env::temp_dir().join(format!("agl-runtime-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = AgentLibrePaths::from_agl_home(&root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.runtime_config_path(),
            r#"
[logging]
level = "debug"
format = "json"
stderr = "always"
include_message_text = true

[history]
enabled = false
"#,
        )
        .unwrap();

        let config = AgentLibreRuntimeConfig::from_paths(paths).unwrap();

        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, AgentLibreLogFormat::Json);
        assert_eq!(config.logging.stderr, AgentLibreStderrLogMode::Always);
        assert!(config.logging.include_message_text);
        assert!(!config.history.enabled);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_logging_keeps_message_text_out() {
        let config = AgentLibreLoggingConfig::default();

        assert!(!config.include_message_text);
        assert_eq!(config.level, "info");
        assert_eq!(config.format, AgentLibreLogFormat::Compact);
    }
}
