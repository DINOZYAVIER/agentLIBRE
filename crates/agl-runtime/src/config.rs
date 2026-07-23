use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agl_process::{
    EnvironmentOverride, ExecutionContextSnapshot, ProcessSupervisorOptions, ShellProfileSnapshot,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

[inference.residency]
context_idle_seconds = 900
model_idle_seconds = 300

[execution]
max_active = 8
default_foreground_timeout_ms = 120000
maximum_foreground_timeout_ms = 1800000
termination_grace_ms = 2000
max_input_bytes = 65536
max_result_bytes = 65536
max_spool_bytes = 67108864
termination_output_headroom_bytes = 1048576
finished_retention_seconds = 604800

[execution.shell]
program = "bash"
command_args = ["-c"]
login_command_args = ["-l", "-c"]

[execution.environment]
inherit = ["PATH", "LANG", "LC_*", "TERM", "COLORTERM", "TZ"]
"#;

pub const DEFAULT_CONTEXT_IDLE_SECONDS: u64 = 900;
pub const DEFAULT_MODEL_IDLE_SECONDS: u64 = 300;
pub const MIN_INFERENCE_IDLE_SECONDS: u64 = 1;
pub const MAX_INFERENCE_IDLE_SECONDS: u64 = 86_400;

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
    pub inference: AgentLibreInferenceConfig,
    pub execution: AgentLibreExecutionConfig,
}

impl AgentLibreRuntimeConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_paths(AgentLibrePaths::from_env()?)
    }

    pub fn from_paths(paths: AgentLibrePaths) -> Result<Self> {
        let file_config = AgentLibreRuntimeConfigFile::read(&paths.runtime_config_path())?;
        let workspace = AgentLibreWorkspaceConfig::from_file_and_env(file_config.workspace)?;
        let inference = file_config.inference.unwrap_or_default();
        inference.validate()?;
        Ok(Self {
            paths,
            logging: AgentLibreLoggingConfig::from_file_and_env(file_config.logging),
            history: file_config.history.unwrap_or_default(),
            workspace,
            inference,
            execution: file_config.execution.unwrap_or_default().validate()?,
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
    inference: Option<AgentLibreInferenceConfig>,
    execution: Option<AgentLibreExecutionConfig>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentLibreInferenceConfig {
    pub residency: AgentLibreInferenceResidencyConfig,
}

impl AgentLibreInferenceConfig {
    fn validate(&self) -> Result<()> {
        self.residency.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentLibreInferenceResidencyConfig {
    pub context_idle_seconds: u64,
    pub model_idle_seconds: u64,
}

impl Default for AgentLibreInferenceResidencyConfig {
    fn default() -> Self {
        Self {
            context_idle_seconds: DEFAULT_CONTEXT_IDLE_SECONDS,
            model_idle_seconds: DEFAULT_MODEL_IDLE_SECONDS,
        }
    }
}

impl AgentLibreInferenceResidencyConfig {
    fn validate(&self) -> Result<()> {
        validate_inference_idle_seconds("context_idle_seconds", self.context_idle_seconds)?;
        validate_inference_idle_seconds("model_idle_seconds", self.model_idle_seconds)
    }
}

fn validate_inference_idle_seconds(name: &str, value: u64) -> Result<()> {
    if !(MIN_INFERENCE_IDLE_SECONDS..=MAX_INFERENCE_IDLE_SECONDS).contains(&value) {
        bail!(
            "inference.residency.{name} must be between {MIN_INFERENCE_IDLE_SECONDS} and {MAX_INFERENCE_IDLE_SECONDS} seconds"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentLibreExecutionConfig {
    pub max_active: usize,
    pub command_capacity: usize,
    pub default_foreground_timeout_ms: u64,
    pub maximum_foreground_timeout_ms: u64,
    pub termination_grace_ms: u64,
    pub setup_timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
    pub max_spool_bytes: u64,
    pub termination_output_headroom_bytes: u64,
    pub finished_retention_seconds: u64,
    pub default_terminal_columns: u16,
    pub default_terminal_rows: u16,
    pub runtime_read_only_roots: Vec<PathBuf>,
    pub shell: AgentLibreShellExecutionConfig,
    pub environment: AgentLibreExecutionEnvironmentConfig,
}

impl Default for AgentLibreExecutionConfig {
    fn default() -> Self {
        Self {
            max_active: 8,
            command_capacity: 64,
            default_foreground_timeout_ms: 120_000,
            maximum_foreground_timeout_ms: 1_800_000,
            termination_grace_ms: 2_000,
            setup_timeout_ms: 10_000,
            poll_interval_ms: 10,
            max_input_bytes: 65_536,
            max_result_bytes: 65_536,
            max_spool_bytes: 67_108_864,
            termination_output_headroom_bytes: 1_048_576,
            finished_retention_seconds: 604_800,
            default_terminal_columns: 80,
            default_terminal_rows: 24,
            runtime_read_only_roots: Vec::new(),
            shell: AgentLibreShellExecutionConfig::default(),
            environment: AgentLibreExecutionEnvironmentConfig::default(),
        }
    }
}

impl AgentLibreExecutionConfig {
    fn validate(mut self) -> Result<Self> {
        if self.max_active == 0
            || self.command_capacity == 0
            || self.default_foreground_timeout_ms == 0
            || self.maximum_foreground_timeout_ms < self.default_foreground_timeout_ms
            || self.termination_grace_ms == 0
            || self.setup_timeout_ms == 0
            || self.poll_interval_ms == 0
            || self.max_input_bytes == 0
            || self.max_input_bytes > 65_536
            || self.max_result_bytes == 0
            || self.max_result_bytes > 65_536
            || self.max_spool_bytes == 0
            || self.termination_output_headroom_bytes == 0
            || self.finished_retention_seconds == 0
            || self.default_terminal_columns == 0
            || self.default_terminal_rows == 0
        {
            bail!("execution limits, timeouts, retention, and terminal dimensions are invalid");
        }
        if self.termination_grace_ms >= self.maximum_foreground_timeout_ms {
            bail!("execution termination grace must be below the maximum foreground timeout");
        }
        self.shell.validate()?;
        self.environment.validate()?;
        let mut canonical_roots = Vec::with_capacity(self.runtime_read_only_roots.len());
        for root in &self.runtime_read_only_roots {
            let canonical = root.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize execution runtime root {}",
                    root.display()
                )
            })?;
            if !canonical.is_dir() {
                bail!(
                    "execution runtime root is not a directory: {}",
                    root.display()
                );
            }
            canonical_roots.push(canonical);
        }
        canonical_roots.sort();
        canonical_roots.dedup();
        self.runtime_read_only_roots = canonical_roots;
        Ok(self)
    }

    pub fn shell_snapshot(&self) -> Result<ShellProfileSnapshot> {
        let program =
            resolve_executable(&self.shell.program, &self.admitted_environment()?.values)?;
        let metadata = std::fs::metadata(&program)
            .with_context(|| format!("failed to inspect execution shell {}", program.display()))?;
        if !metadata.is_file() || !is_executable(&metadata) {
            bail!(
                "configured execution shell is not a regular executable: {}",
                program.display()
            );
        }
        let executable = std::fs::read(&program)
            .with_context(|| format!("failed to read execution shell {}", program.display()))?;
        let config_json = serde_json::to_vec(&(
            &program,
            &self.shell.command_args,
            &self.shell.login_command_args,
            &self.environment.inherit,
        ))?;
        Ok(ShellProfileSnapshot {
            program,
            command_args: self.shell.command_args.clone(),
            login_command_args: self.shell.login_command_args.clone(),
            environment_names: self.admitted_environment()?.values.into_keys().collect(),
            executable_digest: sha256_digest(&executable),
            config_digest: sha256_digest(&config_json),
        })
    }

    pub fn admitted_environment(&self) -> Result<EnvironmentOverride> {
        let mut values = std::collections::BTreeMap::new();
        let mut total_bytes = 0usize;
        for (name, value) in std::env::vars() {
            if self.environment.admits(&name) {
                total_bytes = total_bytes
                    .saturating_add(name.len())
                    .saturating_add(value.len());
                if total_bytes > self.environment.maximum_bytes {
                    bail!("admitted execution environment exceeds its configured byte limit");
                }
                values.insert(name, value);
            }
        }
        let environment = EnvironmentOverride { values };
        environment
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(environment)
    }

    pub fn context_snapshot(&self, workspace_root: &Path) -> Result<ExecutionContextSnapshot> {
        let workspace_root = workspace_root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize workspace root {}",
                workspace_root.display()
            )
        })?;
        let snapshot = ExecutionContextSnapshot {
            workspace_root: workspace_root.clone(),
            working_directory: workspace_root,
            private_execution_roots: Vec::new(),
            shell: self.shell_snapshot()?,
            revision: 1,
            profile_metadata: "workspace".to_owned(),
        };
        snapshot
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(snapshot)
    }

    pub fn supervisor_options(
        &self,
        paths: &AgentLibrePaths,
        launcher_path: PathBuf,
    ) -> Result<ProcessSupervisorOptions> {
        let options = ProcessSupervisorOptions {
            launcher_path,
            data_root: paths.data_dir.join("executions"),
            state_root: paths.state_dir.join("executions"),
            max_active: self.max_active,
            command_capacity: self.command_capacity,
            poll_interval: Duration::from_millis(self.poll_interval_ms),
            setup_timeout: Duration::from_millis(self.setup_timeout_ms),
            termination_grace: Duration::from_millis(self.termination_grace_ms),
            max_input_bytes: self.max_input_bytes,
            max_result_bytes: self.max_result_bytes,
            max_spool_bytes: self.max_spool_bytes,
            termination_output_headroom_bytes: self.termination_output_headroom_bytes,
            finished_retention: Duration::from_secs(self.finished_retention_seconds),
            runtime_read_only_roots: self.runtime_read_only_roots.clone(),
        };
        options
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(options)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLibreShellExecutionConfig {
    pub program: PathBuf,
    pub command_args: Vec<String>,
    pub login_command_args: Option<Vec<String>>,
}

impl Default for AgentLibreShellExecutionConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("bash"),
            command_args: vec!["-c".to_owned()],
            login_command_args: Some(vec!["-l".to_owned(), "-c".to_owned()]),
        }
    }
}

impl AgentLibreShellExecutionConfig {
    fn validate(&self) -> Result<()> {
        if self.program.as_os_str().is_empty()
            || self.command_args.iter().any(|value| value.contains('\0'))
            || self
                .login_command_args
                .iter()
                .flatten()
                .any(|value| value.contains('\0'))
        {
            bail!("execution shell program and argument vectors are invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentLibreExecutionEnvironmentConfig {
    pub inherit: Vec<String>,
    pub maximum_bytes: usize,
}

impl Default for AgentLibreExecutionEnvironmentConfig {
    fn default() -> Self {
        Self {
            inherit: vec![
                "PATH".to_owned(),
                "LANG".to_owned(),
                "LC_*".to_owned(),
                "TERM".to_owned(),
                "COLORTERM".to_owned(),
                "TZ".to_owned(),
            ],
            maximum_bytes: 65_536,
        }
    }
}

impl AgentLibreExecutionEnvironmentConfig {
    fn validate(&self) -> Result<()> {
        if self.maximum_bytes == 0 || self.maximum_bytes > 1_048_576 {
            bail!("execution environment byte limit is invalid");
        }
        for pattern in &self.inherit {
            let prefix = pattern.strip_suffix('*').unwrap_or(pattern);
            if prefix.is_empty()
                || prefix.contains(['=', '\0'])
                || (pattern.contains('*') && !pattern.ends_with('*'))
            {
                bail!("execution environment allowlist pattern is invalid: {pattern:?}");
            }
        }
        Ok(())
    }

    fn admits(&self, name: &str) -> bool {
        self.inherit.iter().any(|pattern| {
            pattern
                .strip_suffix('*')
                .map_or(name == pattern, |prefix| name.starts_with(prefix))
        })
    }
}

fn resolve_executable(
    configured: &Path,
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<PathBuf> {
    if configured.is_absolute() {
        return configured.canonicalize().with_context(|| {
            format!("failed to resolve execution shell {}", configured.display())
        });
    }
    let path = environment
        .get("PATH")
        .ok_or_else(|| anyhow::anyhow!("relative execution shell requires admitted PATH"))?;
    for root in std::env::split_paths(path) {
        let candidate = root.join(configured);
        if candidate.is_file() {
            return candidate.canonicalize().with_context(|| {
                format!("failed to resolve execution shell {}", candidate.display())
            });
        }
    }
    bail!(
        "configured execution shell was not found in admitted PATH: {}",
        configured.display()
    )
}

fn sha256_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in digest {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
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

[inference.residency]
context_idle_seconds = 17
model_idle_seconds = 29
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
        assert_eq!(config.inference.residency.context_idle_seconds, 17);
        assert_eq!(config.inference.residency.model_idle_seconds, 29);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inference_residency_defaults_without_runtime_config() {
        let root = temp_root("inference-residency-defaults");
        let paths = AgentLibrePaths::from_agl_home(&root);

        let config = AgentLibreRuntimeConfig::from_paths(paths).unwrap();

        assert_eq!(
            config.inference.residency.context_idle_seconds,
            DEFAULT_CONTEXT_IDLE_SECONDS
        );
        assert_eq!(
            config.inference.residency.model_idle_seconds,
            DEFAULT_MODEL_IDLE_SECONDS
        );
    }

    #[test]
    fn inference_residency_accepts_inclusive_bounds() {
        let root = temp_root("inference-residency-bounds");
        let paths = AgentLibrePaths::from_agl_home(&root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            paths.runtime_config_path(),
            format!(
                "[inference.residency]\ncontext_idle_seconds = {MIN_INFERENCE_IDLE_SECONDS}\nmodel_idle_seconds = {MAX_INFERENCE_IDLE_SECONDS}\n"
            ),
        )
        .unwrap();

        let config = AgentLibreRuntimeConfig::from_paths(paths).unwrap();

        assert_eq!(
            config.inference.residency.context_idle_seconds,
            MIN_INFERENCE_IDLE_SECONDS
        );
        assert_eq!(
            config.inference.residency.model_idle_seconds,
            MAX_INFERENCE_IDLE_SECONDS
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inference_residency_rejects_values_outside_bounds() {
        for (name, context_idle_seconds, model_idle_seconds) in [
            ("context-zero", 0, DEFAULT_MODEL_IDLE_SECONDS),
            (
                "context-over",
                MAX_INFERENCE_IDLE_SECONDS + 1,
                DEFAULT_MODEL_IDLE_SECONDS,
            ),
            ("model-zero", DEFAULT_CONTEXT_IDLE_SECONDS, 0),
            (
                "model-over",
                DEFAULT_CONTEXT_IDLE_SECONDS,
                MAX_INFERENCE_IDLE_SECONDS + 1,
            ),
        ] {
            let root = temp_root(name);
            let paths = AgentLibrePaths::from_agl_home(&root);
            std::fs::create_dir_all(&paths.config_dir).unwrap();
            std::fs::write(
                paths.runtime_config_path(),
                format!(
                    "[inference.residency]\ncontext_idle_seconds = {context_idle_seconds}\nmodel_idle_seconds = {model_idle_seconds}\n"
                ),
            )
            .unwrap();

            let error = AgentLibreRuntimeConfig::from_paths(paths).unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("must be between 1 and 86400 seconds"),
                "unexpected validation error: {error:#}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn inference_residency_rejects_unknown_and_non_integer_fields() {
        for (name, content) in [
            (
                "unknown",
                "[inference.residency]\ncontext_idle_seconds = 900\nmodel_idle_seconds = 300\nlegacy_idle_seconds = 1\n",
            ),
            (
                "non-integer",
                "[inference.residency]\ncontext_idle_seconds = 1.5\nmodel_idle_seconds = 300\n",
            ),
        ] {
            let root = temp_root(name);
            let paths = AgentLibrePaths::from_agl_home(&root);
            std::fs::create_dir_all(&paths.config_dir).unwrap();
            std::fs::write(paths.runtime_config_path(), content).unwrap();

            let error = AgentLibreRuntimeConfig::from_paths(paths).unwrap_err();

            assert!(
                error.to_string().contains("failed to parse runtime config"),
                "unexpected parse error: {error:#}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
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
