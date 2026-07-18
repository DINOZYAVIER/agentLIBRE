use agl_capabilities::ToolAccessMode;
use agl_content::Content;
use agl_ids::{
    DaemonInstanceId, EventId, ExecutionId, MessageId, RunId, SessionId, StepId, TurnId,
};
use agl_process::{ExecutionExit, ExecutionOutputChunk, ExecutionProfile, ExecutionState};
use serde::{Deserialize, Serialize};

use crate::CommandContext;

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
    UserExecution {
        execution_id: ExecutionId,
        command: String,
        profile: ExecutionProfile,
        cwd: String,
        state: ExecutionState,
        exit: Option<ExecutionExit>,
        output: Vec<ExecutionOutputChunk>,
        output_truncated: bool,
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
            Self::UserExecution { execution_id, .. } => execution_id.to_string(),
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
pub struct UserExecutionView {
    pub execution_id: ExecutionId,
    pub state: ExecutionState,
    pub profile: ExecutionProfile,
    pub last_sequence: u64,
    pub output_truncated: bool,
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
    pub executions: Vec<UserExecutionView>,
    pub command_context: CommandContext,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPresentationEvent {
    SnapshotReplaced {
        snapshot: Box<SessionPresentationSnapshot>,
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
    ExecutionOutput {
        execution_id: ExecutionId,
        chunk: ExecutionOutputChunk,
    },
    ExecutionStateChanged {
        execution: UserExecutionView,
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
