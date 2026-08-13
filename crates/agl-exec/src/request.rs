use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ProcessError, ProcessErrorCode, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionIo {
    Pipes,
    Pty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfile {
    Workspace,
    Host,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Argv,
    Shell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
        }
    }
}

impl TerminalSize {
    pub fn validate(self) -> Result<Self> {
        if self.columns == 0 || self.rows == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidTerminalSize,
                "terminal columns and rows must be nonzero",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAuthorization {
    pub host_process_execution: bool,
    pub shell_login_startup: bool,
    pub workspace_write: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLimits {
    pub timeout_ms: Option<u64>,
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionGrantLease {
    pub origin: ExecutionLeaseOrigin,
    pub grant_id: String,
    pub duration: String,
    pub scope_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLeaseOrigin {
    ToolGrant,
    LocalOperatorTerminal,
}

pub const LOCAL_OPERATOR_TERMINAL_LEASE_DURATION: &str = "daemon_lifetime";

impl ExecutionGrantLease {
    pub fn validate(&self) -> Result<()> {
        if self.grant_id.trim().is_empty()
            || self.duration.trim().is_empty()
            || self.scope_digest.trim().is_empty()
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "execution grant lease fields must be nonempty",
            ));
        }
        if self.origin == ExecutionLeaseOrigin::LocalOperatorTerminal
            && self.duration != LOCAL_OPERATOR_TERMINAL_LEASE_DURATION
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "local-operator terminal authority must have daemon-lifetime duration",
            ));
        }
        Ok(())
    }

    pub fn is_capability_grant(&self) -> bool {
        self.origin == ExecutionLeaseOrigin::ToolGrant
    }
}

impl ExecutionLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_input_bytes == 0 || self.max_output_bytes == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "process input and output limits must be nonzero",
            ));
        }
        if self.timeout_ms == Some(0) {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "process timeout must be nonzero when present",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentOverride {
    pub values: std::collections::BTreeMap<String, String>,
}

impl EnvironmentOverride {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in &self.values {
            if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "environment names and values must be nonempty and contain no NUL or '=' in names",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellProfileSnapshot {
    pub program: PathBuf,
    pub command_args: Vec<String>,
    pub login_command_args: Option<Vec<String>>,
    pub environment_names: Vec<String>,
    pub executable_digest: String,
    pub config_digest: String,
}

impl ShellProfileSnapshot {
    pub fn validate(&self) -> Result<()> {
        validate_program(&self.program)?;
        validate_argv(&self.command_args)?;
        if let Some(args) = &self.login_command_args {
            validate_argv(args)?;
        }
        if self
            .environment_names
            .iter()
            .any(|name| name.is_empty() || name.contains(['=', '\0']))
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "shell environment names must be nonempty and contain no NUL or '='",
            ));
        }
        if self.executable_digest.is_empty() || self.config_digest.is_empty() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "shell admission digests must be nonempty",
            ));
        }
        Ok(())
    }

    pub fn verify_executable(&self) -> Result<()> {
        use sha2::{Digest as _, Sha256};

        self.validate()?;
        let canonical = self.program.canonicalize().map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::SandboxExecutableUnavailable,
                format!("admitted shell executable cannot be resolved: {error}"),
            )
        })?;
        if canonical != self.program {
            return Err(ProcessError::new(
                ProcessErrorCode::SandboxExecutableUnavailable,
                "admitted shell executable path changed after admission",
            ));
        }
        let metadata = std::fs::metadata(&canonical).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::SandboxExecutableUnavailable,
                format!("admitted shell executable cannot be inspected: {error}"),
            )
        })?;
        if !metadata.is_file() || !is_executable(&metadata) {
            return Err(ProcessError::new(
                ProcessErrorCode::SandboxExecutableUnavailable,
                "admitted shell executable is not a regular executable",
            ));
        }
        let bytes = std::fs::read(&canonical).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::SandboxExecutableUnavailable,
                format!("admitted shell executable cannot be read: {error}"),
            )
        })?;
        let mut digest = String::with_capacity(71);
        digest.push_str("sha256:");
        use std::fmt::Write as _;
        for byte in Sha256::digest(bytes) {
            write!(&mut digest, "{byte:02x}").expect("writing to a String cannot fail");
        }
        if digest != self.executable_digest {
            return Err(ProcessError::new(
                ProcessErrorCode::SandboxExecutableUnavailable,
                "admitted shell executable identity changed after admission",
            ));
        }
        Ok(())
    }
}

fn validate_program(program: &Path) -> Result<()> {
    let value = program.as_os_str().to_string_lossy();
    if value.is_empty() || value.contains('\0') || !program.is_absolute() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "admitted program must be an absolute path containing no NUL",
        ));
    }
    Ok(())
}

fn validate_argv(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "process arguments must contain no NUL",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}
