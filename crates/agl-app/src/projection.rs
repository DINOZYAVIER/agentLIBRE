use std::collections::BTreeSet;

use agl_capabilities::ToolAccessMode;
use agl_content::Content;
use agl_ids::{
    DaemonInstanceId, EventId, ExecutionId, MessageId, RunId, SessionId, StepId, TerminalSessionId,
    TurnId,
};
use agl_process::{ExecutionExit, ExecutionProfile, ExecutionState};
use serde::{Deserialize, Serialize};

use crate::{
    ApplicationError, ApplicationErrorCode, CommandContext, MAX_QUEUED_PROMPTS_PER_SESSION,
    MAX_TERMINAL_PATH_BYTES, MAX_TERMINALS_PER_SESSION, TerminalSessionView,
};

pub const MAX_PRESENTATION_ITEMS: usize = 2_000;
pub const MAX_EXECUTION_VIEWS: usize = 2_000;
pub const MAX_PRESENTATION_CONTENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationCursor {
    pub daemon_instance_id: DaemonInstanceId,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPresentationStatus {
    Active,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeader {
    pub session_id: SessionId,
    pub status: SessionPresentationStatus,
    pub durable: bool,
    pub resumed: bool,
    pub title: Option<String>,
    pub function_name: String,
    pub model_id: Option<String>,
    pub operation_mode: ToolAccessMode,
    pub selected_skills: Vec<String>,
    pub runtime_context_revision: u64,
    pub workspace_root: String,
    pub cwd: String,
    pub execution_context_revision: u64,
    pub context_used_tokens: Option<u64>,
    pub context_limit_tokens: Option<u64>,
    pub active_run_count: u32,
    pub queued_prompt_count: u32,
    pub active_execution_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantItemState {
    Streaming,
    Final,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionItemState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPresentationItem {
    UserMessage {
        message_id: MessageId,
        content: Content,
    },
    AssistantMessage {
        message_id: MessageId,
        content: Content,
        state: AssistantItemState,
    },
    AgentAction {
        run_id: RunId,
        step_id: StepId,
        capability_id: Option<String>,
        summary: String,
        state: ActionItemState,
    },
    ContextBoundary {
        event_id: EventId,
        reason: String,
    },
    Notice {
        event_id: EventId,
        severity: Severity,
        code: String,
        message: String,
    },
}

impl SessionPresentationItem {
    pub fn key(&self) -> String {
        match self {
            Self::UserMessage { message_id, .. } | Self::AssistantMessage { message_id, .. } => {
                message_id.to_string()
            }
            Self::AgentAction {
                run_id, step_id, ..
            } => format!("{run_id}:{step_id}"),
            Self::ContextBoundary { event_id, .. } | Self::Notice { event_id, .. } => {
                event_id.to_string()
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveRunView {
    pub run_id: RunId,
    pub turn_id: Option<TurnId>,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedPromptView {
    pub run_id: RunId,
    pub ordinal: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionView {
    pub execution_id: ExecutionId,
    pub state: ExecutionState,
    pub profile: ExecutionProfile,
    pub cwd: String,
    pub exit: Option<ExecutionExit>,
    pub last_sequence: u64,
    pub output_truncated: bool,
}

impl ExecutionView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.cwd.is_empty()
            || self.cwd.len() > MAX_TERMINAL_PATH_BYTES
            || self.cwd.contains('\0')
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "execution cwd must be nonempty bounded text without NUL",
            ));
        }
        if self.state.is_live() && self.exit.is_some() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "a live execution cannot carry an exit outcome",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSnapshot {
    pub session_id: SessionId,
    pub cursor: PresentationCursor,
    pub header: SessionHeader,
    pub items: Vec<SessionPresentationItem>,
    pub active_run: Option<ActiveRunView>,
    pub queued_prompts: Vec<QueuedPromptView>,
    pub terminals: Vec<TerminalSessionView>,
    pub executions: Vec<ExecutionView>,
    pub command_context: CommandContext,
}

impl SessionPresentationSnapshot {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.header.session_id != self.session_id
            || self
                .command_context
                .session_id
                .as_ref()
                .is_some_and(|id| id != &self.session_id)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "snapshot session identities must agree",
            ));
        }
        if self.items.len() > MAX_PRESENTATION_ITEMS
            || self.queued_prompts.len() > MAX_QUEUED_PROMPTS_PER_SESSION
            || self.terminals.len() > MAX_TERMINALS_PER_SESSION
            || self.executions.len() > MAX_EXECUTION_VIEWS
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "snapshot exceeds a bounded collection limit",
            ));
        }

        let mut terminal_ids = BTreeSet::new();
        let mut terminal_execution_ids = BTreeSet::new();
        for terminal in &self.terminals {
            terminal.validate_for_session(&self.session_id)?;
            if !terminal_ids.insert(&terminal.terminal_id)
                || !terminal_execution_ids.insert(&terminal.execution_id)
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "snapshot terminal identities must be unique",
                ));
            }
        }
        let mut execution_ids = BTreeSet::new();
        for execution in &self.executions {
            execution.validate()?;
            if !execution_ids.insert(&execution.execution_id) {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "snapshot execution identities must be unique",
                ));
            }
        }
        let encoded_bytes = serde_json::to_vec(self)
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "session presentation snapshot could not be encoded",
                )
            })?
            .len();
        if encoded_bytes > MAX_PRESENTATION_CONTENT_BYTES {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "session presentation snapshot exceeds the 8 MiB content limit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPresentationEvent {
    SnapshotReplaced {
        snapshot: Box<SessionPresentationSnapshot>,
        older_page_cursor: Option<String>,
    },
    HeaderChanged {
        header: SessionHeader,
    },
    ItemUpsert {
        item: SessionPresentationItem,
    },
    ItemRemoved {
        item_key: String,
    },
    AssistantTextDelta {
        run_id: RunId,
        turn_id: TurnId,
        provisional_message_id: MessageId,
        sequence: u64,
        text: String,
    },
    PromptQueued {
        prompt: QueuedPromptView,
    },
    PromptActivated {
        run_id: RunId,
    },
    PromptFinished {
        run_id: RunId,
        state: String,
    },
    TerminalAdded {
        terminal: TerminalSessionView,
    },
    TerminalChanged {
        terminal: TerminalSessionView,
    },
    TerminalRemoved {
        terminal_id: TerminalSessionId,
    },
    TerminalCommandStarted {
        terminal_id: TerminalSessionId,
        sequence: u64,
    },
    TerminalCommandFinished {
        terminal_id: TerminalSessionId,
        sequence: u64,
        exit_status: i32,
        cwd: String,
    },
    ExecutionStateChanged {
        execution: ExecutionView,
    },
    CommandAvailabilityChanged,
    SessionFinished,
    Notice {
        severity: Severity,
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationEventEnvelope {
    pub event_id: EventId,
    pub session_id: SessionId,
    pub cursor: PresentationCursor,
    pub event: SessionPresentationEvent,
}
