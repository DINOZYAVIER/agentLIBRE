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
