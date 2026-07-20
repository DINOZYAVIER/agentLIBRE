use std::path::Path;
use std::path::PathBuf;

use agl_ids::{ExecutionId, RequestId, RunId, SessionId};
use serde::{Deserialize, Serialize};

use crate::{ExecutionIo, ExecutionOwner, ExecutionProfile, ProcessBytes, TerminalSize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Admitting,
    Starting,
    Running,
    Exited,
    Signalled,
    Cancelled,
    TimedOut,
    Failed,
    OutcomeUnknown,
}

impl ExecutionState {
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }

    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Admitting | Self::Starting | Self::Running)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionChannel {
    Stdout,
    Stderr,
    Terminal,
    Lifecycle,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCursor {
    pub after_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOutputChunk {
    pub sequence: u64,
    pub channel: ExecutionChannel,
    pub bytes: ProcessBytes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExecutionExit {
    Code { code: i32 },
    Signal { signal: i32 },
    Error { code: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStatus {
    pub execution_id: ExecutionId,
    pub owner: ExecutionOwner,
    pub state: ExecutionState,
    pub profile: ExecutionProfile,
    pub io: ExecutionIo,
    pub cwd: PathBuf,
    pub terminal_size: Option<TerminalSize>,
    pub exit: Option<ExecutionExit>,
    pub first_retained_sequence: Option<u64>,
    pub last_sequence: u64,
    pub retained_bytes: u64,
    pub discarded_output_bytes: u64,
    pub output_truncated: bool,
    pub output_expired: bool,
    pub started_at_unix_ms: Option<i64>,
    pub finished_at_unix_ms: Option<i64>,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPrivateCommand {
    pub display: String,
    pub truncated: bool,
}

impl ExecutionPrivateCommand {
    pub fn from_argv(program: &Path, args: &[String], maximum_bytes: usize) -> crate::Result<Self> {
        if maximum_bytes < 4 {
            return Err(crate::ProcessError::new(
                crate::ProcessErrorCode::InvalidRequest,
                "private command display bound must be at least four bytes",
            ));
        }
        let argv = std::iter::once(program.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>();
        let mut display = serde_json::to_string(&argv).map_err(|error| {
            crate::ProcessError::new(
                crate::ProcessErrorCode::Internal,
                format!("failed to encode private command display: {error}"),
            )
        })?;
        if display.len() <= maximum_bytes {
            return Ok(Self {
                display,
                truncated: false,
            });
        }
        let end_limit = maximum_bytes - '…'.len_utf8();
        let mut end = end_limit.min(display.len());
        while !display.is_char_boundary(end) {
            end -= 1;
        }
        display.truncate(end);
        display.push('…');
        Ok(Self {
            display,
            truncated: true,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionListFilter {
    pub session_id: Option<SessionId>,
    pub root_run_id: Option<RunId>,
    pub include_finished: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReadResult {
    pub execution_id: ExecutionId,
    pub chunks: Vec<ExecutionOutputChunk>,
    pub next_sequence: u64,
    pub state: ExecutionState,
    pub output_truncated: bool,
    pub output_expired: bool,
}

/// Destructive bounded drain of the private managed-shell side channel. These
/// bytes are never sourced from the PTY spool and must be passed only to the
/// terminal registry's authenticated integration parser.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellIntegrationReadResult {
    pub execution_id: ExecutionId,
    pub bytes: ProcessBytes,
    pub output_through_sequence: u64,
    /// Kernel-observed foreground process group for the managed PTY. `None`
    /// means the shell itself currently owns the foreground terminal.
    pub foreground_process_group: Option<i32>,
    pub channel_closed: bool,
    pub degraded: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputLease {
    pub attachment_id: RequestId,
    pub writable: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillMode {
    #[default]
    Graceful,
    Immediate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_starting_and_running_are_live() {
        assert!(ExecutionState::Starting.is_live());
        assert!(ExecutionState::Running.is_live());
        assert!(!ExecutionState::Admitting.is_live());
        assert!(ExecutionState::OutcomeUnknown.is_terminal());
    }

    #[test]
    fn private_command_display_is_utf8_bounded_and_contains_only_argv() {
        let command = ExecutionPrivateCommand::from_argv(
            Path::new("/bin/echo"),
            &["short".to_owned(), "секретный-длинный-аргумент".to_owned()],
            24,
        )
        .unwrap();

        assert!(command.truncated);
        assert!(command.display.len() <= 24);
        assert!(command.display.ends_with('…'));
        assert!(std::str::from_utf8(command.display.as_bytes()).is_ok());

        let complete = ExecutionPrivateCommand::from_argv(Path::new("/bin/true"), &[], 64).unwrap();
        assert_eq!(complete.display, r#"["/bin/true"]"#);
        assert!(!complete.truncated);
    }
}
