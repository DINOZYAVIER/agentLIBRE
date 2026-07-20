use std::fmt::{self, Debug, Formatter};

use agl_ids::TerminalSessionId;

use crate::{ApplicationError, ApplicationErrorCode, MAX_TERMINAL_PATH_BYTES};

pub const HUMAN_SHELL_HISTORY_MAX_ENTRIES: usize = 2_000;
pub const HUMAN_SHELL_HISTORY_MAX_COMMAND_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HumanShellHistoryPolicy {
    pub maximum_entries: usize,
    pub maximum_command_bytes: usize,
}

impl HumanShellHistoryPolicy {
    pub const fn selected() -> Self {
        Self {
            maximum_entries: HUMAN_SHELL_HISTORY_MAX_ENTRIES,
            maximum_command_bytes: HUMAN_SHELL_HISTORY_MAX_COMMAND_BYTES,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HumanShellHistoryCommand {
    terminal_id: TerminalSessionId,
    command_sequence: u64,
    workspace_root: String,
    command: String,
}

impl HumanShellHistoryCommand {
    pub fn new(
        terminal_id: TerminalSessionId,
        command_sequence: u64,
        workspace_root: impl Into<String>,
        command: impl Into<String>,
    ) -> Result<Self, ApplicationError> {
        let workspace_root = workspace_root.into();
        let command = command.into();
        if workspace_root.is_empty()
            || workspace_root.len() > MAX_TERMINAL_PATH_BYTES
            || workspace_root.contains('\0')
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "history workspace root must be nonempty bounded text without NUL",
            ));
        }
        if command.is_empty()
            || command.len() > HUMAN_SHELL_HISTORY_MAX_COMMAND_BYTES
            || command.contains('\0')
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "history command must be nonempty bounded UTF-8 without NUL",
            ));
        }
        Ok(Self {
            terminal_id,
            command_sequence,
            workspace_root,
            command,
        })
    }

    pub fn terminal_id(&self) -> &TerminalSessionId {
        &self.terminal_id
    }

    pub fn command_sequence(&self) -> u64 {
        self.command_sequence
    }

    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    pub fn command(&self) -> &str {
        &self.command
    }
}

impl Debug for HumanShellHistoryCommand {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanShellHistoryCommand")
            .field("terminal_id", &self.terminal_id)
            .field("command_sequence", &self.command_sequence)
            .field("workspace_root", &self.workspace_root)
            .field("command", &"<private>")
            .finish()
    }
}

pub trait HumanShellHistoryOwner: Send + Sync + 'static {
    fn load(
        &self,
        workspace_root: &str,
        policy: HumanShellHistoryPolicy,
    ) -> Result<Vec<String>, ApplicationError>;

    fn append(&self, command: HumanShellHistoryCommand) -> Result<(), ApplicationError>;
}
