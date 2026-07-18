use agl_ids::{ExecutionId, RunId, SessionId, StepId};
use agl_process::{ExecutionProfile, ExecutionStatus, TerminalSize};
use serde::{Deserialize, Serialize};

use crate::{ApplicationError, ApplicationErrorCode};

pub const MAX_USER_SHELL_COMMAND_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalOperatorPrincipal {
    pub uid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserShellSubmission {
    pub session_id: SessionId,
    pub client_submission_id: String,
    pub command: String,
    pub execution_context_revision: u64,
    pub profile: ExecutionProfile,
    pub terminal_size: TerminalSize,
    pub background: bool,
    pub operator: LocalOperatorPrincipal,
}

impl UserShellSubmission {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.client_submission_id.is_empty() || self.client_submission_id.len() > 256 {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "client submission ID must be nonempty and bounded",
            ));
        }
        if self.command.contains('\0')
            || self.command.len() > MAX_USER_SHELL_COMMAND_BYTES
            || self
                .command
                .chars()
                .all(|character| character == '\r' || character == '\n')
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "user shell command must be nonempty bounded UTF-8 without NUL",
            ));
        }
        self.terminal_size.validate().map_err(|error| {
            ApplicationError::new(ApplicationErrorCode::InvalidArguments, error.to_string())
        })?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserShellAdmission {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub execution_id: ExecutionId,
    pub resolved_cwd: String,
    pub profile: ExecutionProfile,
    pub status: ExecutionStatus,
    pub background: bool,
    pub replayed: bool,
}
