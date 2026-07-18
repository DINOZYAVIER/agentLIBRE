use agl_content::Content;
use agl_ids::{
    DaemonInstanceId, EventId, ExecutionId, MessageId, RequestId, RunId, SessionId, StepId, TurnId,
};
use serde::{Deserialize, Serialize};

use crate::{
    ExecutionExit, ExecutionOutputChunk, ExecutionProfile, ExecutionState, ExecutionStatus,
    KillMode, ProtocolToolMode, TerminalSize,
};

pub const MAX_JSONL_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_ASSISTANT_DELTA_BYTES: usize = 16 * 1024;
pub const MAX_PRESENTATION_ITEMS: usize = 2_000;
pub const MAX_PRESENTATION_CONTENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_COMMAND_DESCRIPTORS: usize = 256;
pub const MAX_COMMAND_ARGUMENTS: usize = 64;
pub const MAX_SUGGESTIONS: usize = 50;
pub const MAX_COMMAND_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_DISPLAY_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientEffectKind {
    Help,
    Disconnect,
    InputHistory,
    RawExecutionAttach,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCatalogRequest {
    pub session_id: Option<SessionId>,
    pub client_effects: Vec<ClientEffectKind>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSuggestionsRequest {
    pub session_id: Option<SessionId>,
    pub command_id: String,
    pub argument_id: String,
    pub query: String,
    pub cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    Session,
    Runtime,
    Workspace,
    Execution,
    Client,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandArgumentKind {
    String,
    Boolean,
    Unsigned,
    Path,
    SessionId,
    ExecutionId,
    ModelId,
    OperationMode,
    SkillId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgumentDescriptor {
    pub id: String,
    pub label: String,
    pub kind: CommandArgumentKind,
    pub required: bool,
    pub repeated: bool,
    pub suggestion_source: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationActionKind {
    SessionNew,
    SessionResume,
    SessionStatus,
    ModelSelect,
    OperationModeSelect,
    SkillsSelect,
    WorkspaceGet,
    WorkspaceSet,
    WorkingDirectoryGet,
    WorkingDirectorySet,
    ExecutionList,
    ExecutionAttach,
    ExecutionKill,
    RuntimeContextReload,
    SessionClear,
    SessionExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandConcurrency {
    ReadOnly,
    TurnBoundaryMutation,
    SessionDestructive,
    StartsExecution,
    SurfaceLocal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAvailability {
    Enabled,
    Disabled {
        reason_code: String,
        message: String,
    },
    Hidden,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDescriptor {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub summary: String,
    pub category: CommandCategory,
    pub arguments: Vec<CommandArgumentDescriptor>,
    pub action_kind: ApplicationActionKind,
    pub concurrency: CommandConcurrency,
    pub availability: CommandAvailability,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCatalogEvent {
    pub descriptors: Vec<CommandDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSuggestion {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSuggestionsEvent {
    pub entries: Vec<CommandSuggestion>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLaunchOptions {
    pub workspace_root: Option<String>,
    pub function_ref: Option<String>,
    pub model_id: Option<String>,
    pub operation_mode: Option<ProtocolToolMode>,
    pub skill_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionSelector {
    Latest,
    Id { session_id: SessionId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationAction {
    SessionNew {
        launch: SessionLaunchOptions,
    },
    SessionResume {
        selector: SessionSelector,
    },
    SessionStatus,
    ModelSelect {
        model_id: String,
    },
    OperationModeSelect {
        mode: ProtocolToolMode,
    },
    SkillsSelect {
        skill_ids: Vec<String>,
    },
    WorkspaceGet,
    WorkspaceSet {
        path: String,
    },
    WorkingDirectoryGet,
    WorkingDirectorySet {
        path: String,
        profile: ExecutionProfile,
    },
    ExecutionList {
        include_finished: bool,
    },
    ExecutionAttach {
        execution_id: ExecutionId,
        read_only: bool,
    },
    ExecutionKill {
        execution_id: ExecutionId,
        mode: KillMode,
    },
    RuntimeContextReload,
    SessionClear,
    SessionExit {
        confirm_active: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationActionRequest {
    pub session_id: Option<SessionId>,
    pub client_submission_id: String,
    pub action: ApplicationAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSubscribeRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionCancelRequest {
    pub subscription_request_id: RequestId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserShellStartRequest {
    pub session_id: SessionId,
    pub client_submission_id: String,
    pub command: String,
    pub execution_context_revision: u64,
    pub profile: ExecutionProfile,
    pub terminal_size: TerminalSize,
    pub background: bool,
}

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
    pub operation_mode: ProtocolToolMode,
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
pub struct CommandContext {
    pub session_id: Option<SessionId>,
    pub session_active: bool,
    pub active_or_queued_turns: u32,
    pub active_executions: u32,
    pub host_shell_available: bool,
    pub operation_mode: ProtocolToolMode,
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
#[serde(deny_unknown_fields)]
pub struct SessionPresentationEvent {
    pub snapshot: SessionPresentationSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSubscriptionStartedEvent {
    pub snapshot: SessionPresentationSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPresentationEventPayload {
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
    pub event: SessionPresentationEventPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationSubscriptionFinishReason {
    ClientCancelled,
    SessionFinished,
    DaemonShutdown,
    ResyncRequired,
    ProtocolError,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSubscriptionFinishedEvent {
    pub session_id: SessionId,
    pub last_delivered_cursor: PresentationCursor,
    pub reason: PresentationSubscriptionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionCancelledEvent {
    pub subscription_request_id: RequestId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserShellAcceptedEvent {
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationActionResult {
    SessionOpened {
        session_id: SessionId,
        resumed: bool,
        snapshot: Box<SessionPresentationSnapshot>,
    },
    Status {
        header: SessionHeader,
    },
    ModelChanged {
        header: SessionHeader,
    },
    ModeChanged {
        header: SessionHeader,
    },
    SkillsChanged {
        header: SessionHeader,
    },
    WorkspaceChanged {
        header: SessionHeader,
    },
    WorkingDirectoryChanged {
        header: SessionHeader,
    },
    Executions {
        executions: Vec<ExecutionStatus>,
    },
    AttachAccepted {
        execution_id: ExecutionId,
        read_only: bool,
    },
    KillAccepted {
        execution_id: ExecutionId,
        mode: KillMode,
    },
    Reloaded {
        visible_tools: Vec<String>,
        context_revision: u64,
    },
    Cleared {
        removed_messages: u64,
        cursor: PresentationCursor,
    },
    SessionExited {
        session_id: SessionId,
        cancelled_runs: u32,
        terminated_executions: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationActionResultEvent {
    pub result: ApplicationActionResult,
}
