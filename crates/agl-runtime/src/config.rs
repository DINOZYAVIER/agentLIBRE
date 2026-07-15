use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::AgentLibrePaths;

pub const DEFAULT_RUNTIME_CONFIG_TOML: &str = r#"[logging]
level = "info"
format = "compact"
file = true
stderr = "never"
include_message_text = false

[history]
enabled = true

[workspace]
# root = "/path/to/workspace"
"#;

pub fn write_default_runtime_config(path: impl AsRef<Path>, force: bool) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create runtime config directory {}",
                parent.display()
            )
        })?;
    }

    if force {
        std::fs::write(path, DEFAULT_RUNTIME_CONFIG_TOML)
            .with_context(|| format!("failed to write runtime config {}", path.display()))?;
        return Ok(());
    }

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("runtime config already exists: {}", path.display())
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to write runtime config {}", path.display()));
        }
    };
    file.write_all(DEFAULT_RUNTIME_CONFIG_TOML.as_bytes())
        .with_context(|| format!("failed to write runtime config {}", path.display()))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLibreRuntimeConfig {
    pub paths: AgentLibrePaths,
    pub logging: AgentLibreLoggingConfig,
    pub history: AgentLibreHistoryConfig,
    pub workspace: AgentLibreWorkspaceConfig,
}

impl AgentLibreRuntimeConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_paths(AgentLibrePaths::from_env()?)
    }

    pub fn from_paths(paths: AgentLibrePaths) -> Result<Self> {
        let file_config = AgentLibreRuntimeConfigFile::read(&paths.runtime_config_path())?;
        let workspace = AgentLibreWorkspaceConfig::from_file_and_env(file_config.workspace)?;
        Ok(Self {
            paths,
            logging: AgentLibreLoggingConfig::from_file_and_env(file_config.logging),
            history: file_config.history.unwrap_or_default(),
            workspace,
        })
    }

    pub fn resolve_workspace_root(&self, override_root: Option<&Path>) -> Result<PathBuf> {
        self.workspace.resolve_root(override_root)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentLibreRuntimeConfigFile {
    logging: Option<AgentLibreLoggingConfigFile>,
    history: Option<AgentLibreHistoryConfig>,
    workspace: Option<AgentLibreWorkspaceConfig>,
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
            stderr: AgentLibreStderrLogMode::Never,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLibreWorkspaceConfig {
    pub root: Option<PathBuf>,
}

impl AgentLibreWorkspaceConfig {
    fn from_file_and_env(file_config: Option<Self>) -> Result<Self> {
        let mut config = file_config.unwrap_or_default();
        if let Some(root) = env_workspace_root() {
            config.root = Some(root);
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if let Some(root) = &self.root {
            validate_non_empty_path("workspace.root", root)?;
        }
        Ok(())
    }

    pub fn resolve_root(&self, override_root: Option<&Path>) -> Result<PathBuf> {
        let explicit = override_root.or(self.root.as_deref());
        resolve_workspace_root_from(std::env::current_dir()?, explicit)
    }
}

pub fn resolve_workspace_root_from(
    start: impl AsRef<Path>,
    explicit_root: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(root) = explicit_root {
        validate_non_empty_path("workspace root", root)?;
        return canonical_workspace_root(root);
    }

    let start = canonical_workspace_root(start.as_ref())?;
    Ok(find_git_top(&start).unwrap_or(start))
}

fn canonical_workspace_root(root: &Path) -> Result<PathBuf> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize workspace root {}", root.display()))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        bail!("workspace root is not a directory: {}", root.display())
    }
}

fn find_git_top(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn env_workspace_root() -> Option<PathBuf> {
    std::env::var_os("AGL_WORKSPACE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn validate_non_empty_path(name: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{name} cannot be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-runtime-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn runtime_config_file_overrides_logging_and_history() {
        let root = temp_root("config-file");
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

[workspace]
root = "/tmp/workspace-root"
"#,
        )
        .unwrap();

        let config = AgentLibreRuntimeConfig::from_paths(paths).unwrap();

        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, AgentLibreLogFormat::Json);
        assert_eq!(config.logging.stderr, AgentLibreStderrLogMode::Always);
        assert!(config.logging.include_message_text);
        assert!(!config.history.enabled);
        assert_eq!(
            config.workspace.root,
            Some(PathBuf::from("/tmp/workspace-root"))
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_logging_keeps_message_text_out() {
        let config = AgentLibreLoggingConfig::default();

        assert!(!config.include_message_text);
        assert_eq!(config.level, "info");
        assert_eq!(config.format, AgentLibreLogFormat::Compact);
        assert_eq!(config.stderr, AgentLibreStderrLogMode::Never);
    }

    #[test]
    fn workspace_root_resolves_git_top_before_cwd() {
        let root = temp_root("workspace-git");
        let nested = root.join("crates/agl-cli");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        let resolved = resolve_workspace_root_from(&nested, None).unwrap();

        assert_eq!(resolved, root.canonicalize().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_root_falls_back_to_current_directory_without_git() {
        let root = temp_root("workspace-cwd");
        std::fs::create_dir_all(&root).unwrap();
        if root.ancestors().any(|path| path.join(".git").exists()) {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }

        let resolved = resolve_workspace_root_from(&root, None).unwrap();

        assert_eq!(resolved, root.canonicalize().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_root_explicit_override_wins() {
        let root = temp_root("workspace-explicit");
        let start = root.join("repo/subdir");
        let explicit = root.join("workspace");
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(&start).unwrap();
        std::fs::create_dir_all(&explicit).unwrap();

        let resolved = resolve_workspace_root_from(&start, Some(&explicit)).unwrap();

        assert_eq!(resolved, explicit.canonicalize().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writes_default_runtime_config_file() {
        let root = temp_root("config-write");
        let path = root.join("config").join("agentlibre.toml");

        write_default_runtime_config(&path, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DEFAULT_RUNTIME_CONFIG_TOML
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_runtime_config_refuses_overwrite() {
        let root = temp_root("config-refuse");
        let path = root.join("config").join("agentlibre.toml");
        write_default_runtime_config(&path, false).unwrap();

        let err = write_default_runtime_config(&path, false).unwrap_err();

        assert!(err.to_string().contains("runtime config already exists"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_runtime_config_force_overwrites() {
        let root = temp_root("config-force");
        let path = root.join("config").join("agentlibre.toml");
        write_default_runtime_config(&path, false).unwrap();
        std::fs::write(&path, "old").unwrap();

        write_default_runtime_config(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DEFAULT_RUNTIME_CONFIG_TOML
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
