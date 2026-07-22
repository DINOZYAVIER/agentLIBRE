use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Display, Formatter, Write as _};

use agl_content::Content;
use agl_ids::{
    AttemptId, DaemonInstanceId, EventId, ExecutionId, MessageId, RequestId, RunId, SessionId,
    StepId, TerminalSessionId, TurnId, WriterLeaseId,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    DaemonEventKind, DaemonRequestKind, ExecutionCursor, ExecutionExit, ExecutionProfile,
    ExecutionState, KillMode, ProtocolToolMode, TerminalSize,
};

pub const MAX_JSONL_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_ASSISTANT_DELTA_BYTES: usize = 16 * 1024;
pub const MAX_PRESENTATION_ITEMS: usize = 2_000;
pub const MAX_PRESENTATION_CONTENT_BYTES: usize = 8 * 1024 * 1024;
/// Raw bytes per snapshot transfer frame. Base64 encoding keeps the complete
/// JSONL daemon event comfortably below the 1 MiB frame boundary.
pub const MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES: usize = 700 * 1024;
pub const MAX_PRESENTATION_SNAPSHOT_CHUNK_ENCODED_BYTES: usize =
    MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES.div_ceil(3) * 4;
pub const MAX_PRESENTATION_SNAPSHOT_CHUNKS: usize =
    MAX_PRESENTATION_CONTENT_BYTES.div_ceil(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES);
pub const MAX_TERMINAL_RECORDS: usize = 128;
pub const MAX_ENVIRONMENT_NAMES: usize = 256;
pub const MAX_ENVIRONMENT_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_DESCRIPTORS: usize = 256;
pub const MAX_COMMAND_ARGUMENTS: usize = 64;
pub const MAX_SUGGESTIONS: usize = 50;
pub const MAX_COMMAND_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_DISPLAY_BYTES: usize = 8 * 1024;
pub const MAX_SAFE_METADATA_ENTRIES: usize = 64;
pub const MAX_HUMAN_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_HUMAN_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_HUMAN_COMMAND_CARDS: usize = 32;
pub const MAX_HUMAN_COMMAND_AGGREGATE_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ACTIVITY_NODES: usize = 512;
pub const MAX_ACTIVE_ACTIVITY_NODES: usize = 256;
pub const MAX_ACTIVITY_PATH_NODES: usize = 32;
pub const MAX_ACTIVITY_SUMMARY_BYTES: usize = 1024;
pub const MAX_ACTIVITY_NODE_BYTES: usize = 8 * 1024;
pub const MAX_ACTIVITY_GRAPH_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ACTIVE_ACTIVITY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ACTIVITY_DELTA_BYTES: usize = 700 * 1024;
pub const MAX_INFERENCE_DEVICES: usize = 64;
pub const MAX_SETUP_SMOKE_MODEL_BINDINGS: usize = 16;
pub const MAX_SETUP_SMOKE_OUTPUT_TOKENS: u32 = 4_096;

const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 8 * 1024;
const MAX_DIGEST_BYTES: usize = 512;
const MAX_CLIENT_EFFECTS: usize = 16;
const MAX_SKILLS: usize = 128;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 255;
const MAX_ENVIRONMENT_SINGLE_VALUE_BYTES: usize = 16 * 1024;
const MAX_SECRET_REFERENCE_BYTES: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceValidationError {
    message: String,
}

impl SurfaceValidationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for SurfaceValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SurfaceValidationError {}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
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
    ClientHelp,
    ClientDisconnect,
    SessionNew,
    SessionResume,
    SessionStatus,
    ModelSelect,
    OperationModeSelect,
    SkillsSelect,
    WorkspaceGet,
    WorkspaceSet,
    TerminalList,
    TerminalPromote,
    IncompleteTurnContinue,
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
        confirm_terminate_terminals: bool,
    },
    TerminalList {
        include_finished: bool,
    },
    TerminalPromote {
        terminal_id: TerminalSessionId,
    },
    IncompleteTurnContinue {
        message_id: MessageId,
        expected_execution_context_revision: u64,
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
    pub page_cursor: Option<String>,
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

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEnvironmentReference {
    pub name: String,
    pub reference_id: String,
}

impl Debug for SecretEnvironmentReference {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretEnvironmentReference")
            .field("name", &self.name)
            .field("reference_id", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredEnvironmentOverlay {
    pub values: BTreeMap<String, String>,
    pub inherited_names: Vec<String>,
    pub secret_refs: Vec<SecretEnvironmentReference>,
}

impl Debug for StructuredEnvironmentOverlay {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let secret_names = self
            .secret_refs
            .iter()
            .map(|reference| reference.name.as_str())
            .collect::<Vec<_>>();
        let value_bytes = self.values.values().map(String::len).sum::<usize>();
        formatter
            .debug_struct("StructuredEnvironmentOverlay")
            .field("value_names", &self.values.keys().collect::<Vec<_>>())
            .field("value_bytes", &value_bytes)
            .field("inherited_names", &self.inherited_names)
            .field("secret_names", &secret_names)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStartupPolicy {
    ManagedOnly,
    SourceUserRc,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalEnsureRequest {
    pub session_id: SessionId,
    pub client_submission_id: String,
    pub execution_context_revision: u64,
    pub profile: ExecutionProfile,
    pub shell_profile_id: String,
    pub terminal_size: TerminalSize,
    pub agl_env: StructuredEnvironmentOverlay,
    pub host_startup: HostStartupPolicy,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalCommandSubmitRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalSessionId,
    pub client_submission_id: String,
    pub writer_lease_id: WriterLeaseId,
    pub expected_command_sequence: u64,
    pub expected_prompt_generation: u64,
    pub command: String,
}

impl Debug for HumanTerminalCommandSubmitRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanTerminalCommandSubmitRequest")
            .field("session_id", &self.session_id)
            .field("terminal_id", &self.terminal_id)
            .field("client_submission_id", &self.client_submission_id)
            .field("writer_lease_present", &true)
            .field("expected_command_sequence", &self.expected_command_sequence)
            .field(
                "expected_prompt_generation",
                &self.expected_prompt_generation,
            )
            .field("command_bytes", &self.command.len())
            .finish()
    }
}

/// Explicit local-operator admission for one Human Host terminal lifetime.
///
/// The confirmation is deliberately non-secret and carries no reusable model
/// capability. The daemon additionally authenticates the local operator from
/// the private Unix-socket peer credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanHostTerminalEnsureRequest {
    pub terminal: HumanTerminalEnsureRequest,
    pub confirm_host_authority: bool,
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
pub struct SanitizedDisplayPath {
    pub text: String,
    pub truncated: bool,
}

impl SanitizedDisplayPath {
    fn validate(&self) -> Result<(), SurfaceValidationError> {
        bound_string(&self.text, MAX_PATH_BYTES, "sanitized display path", false)?;
        if self
            .text
            .chars()
            .any(|character| character.is_control() || is_unicode_format_control(character as u32))
        {
            return Err(SurfaceValidationError::new(
                "sanitized display path contains a control or format character",
            ));
        }
        Ok(())
    }
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
    pub workspace_root: SanitizedDisplayPath,
    pub workspace_history_scope: String,
    pub cwd: SanitizedDisplayPath,
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
pub enum IncompleteOutputReason {
    ModelLength,
    ContentByteLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinueUnavailableReason {
    StaleContext,
    PolicyDenied,
    SessionFinished,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinueActionView {
    Available,
    Claimed { continuation_run_id: RunId },
    Unavailable { reason: ContinueUnavailableReason },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncompleteAssistantItemView {
    pub message_id: MessageId,
    pub content: Content,
    pub source_run_id: RunId,
    pub source_turn_id: TurnId,
    pub source_attempt_id: AttemptId,
    pub reason: IncompleteOutputReason,
    pub continuation_index: u16,
    pub continue_action: ContinueActionView,
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
    IncompleteAssistant {
        item: IncompleteAssistantItemView,
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
    pub cwd: SanitizedDisplayPath,
    pub exit: Option<ExecutionExit>,
    pub last_sequence: u64,
    pub output_truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanCommandCardState {
    Starting,
    Running,
    Exited,
    OutcomeUnknown,
}

pub type ExecutionOutputCursor = ExecutionCursor;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SanitizedTerminalText(String);

impl SanitizedTerminalText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(
        &self,
        maximum_bytes: usize,
        allow_empty: bool,
    ) -> Result<(), SurfaceValidationError> {
        if (!allow_empty && self.0.is_empty())
            || self.0.len() > maximum_bytes
            || self.0.chars().any(is_forbidden_presentation_character)
        {
            return Err(SurfaceValidationError::new(
                "sanitized terminal text is empty, oversized, or contains a forbidden character",
            ));
        }
        Ok(())
    }
}

impl Debug for SanitizedTerminalText {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SanitizedTerminalText")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanCommandCardView {
    pub terminal_id: TerminalSessionId,
    pub execution_id: ExecutionId,
    pub command_sequence: u64,
    pub command: SanitizedTerminalText,
    pub output: SanitizedTerminalText,
    pub output_start: ExecutionOutputCursor,
    pub output_end: ExecutionOutputCursor,
    pub state: HumanCommandCardState,
    pub exit_status: Option<i32>,
    pub cwd: SanitizedDisplayPath,
    pub truncated: bool,
    pub filtered_effects: u32,
    pub started_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityNodeKind {
    Run,
    Turn,
    Attempt,
    Step,
    ChildRun,
    Inference,
    Aggregate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    Queued,
    Policy,
    Model,
    Tool,
    ChildRun,
    InferenceQueue,
    InferenceAdmission,
    ModelLoad,
    Context,
    Prefill,
    Generation,
    OutputParsing,
    Terminal,
    Retention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityNodeState {
    Pending,
    Waiting,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Incomplete,
    Truncated,
}

impl ActivityNodeState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Incomplete | Self::Truncated
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityCompleteness {
    Complete,
    Truncated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPolicyOutcome {
    Allowed,
    Denied,
    ConfirmationRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityCacheDisposition {
    NotApplicable,
    Cold,
    Reused,
    Rebuilt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProgressUnit {
    Tokens,
    Chunks,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProductStageView {
    Queued,
    Admission,
    ModelLoad,
    ModelReuse,
    ContextReuse,
    ContextRebuild,
    Prefill,
    Generation,
    OutputParse,
    Completed,
    Incomplete,
    Cancelled,
    Failed,
    BackendLost,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "capability", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityActivityDetail {
    FilesystemList {
        path: SanitizedDisplayPath,
        entries: u32,
        completeness: ActivityCompleteness,
    },
    FilesystemRead {
        path: SanitizedDisplayPath,
        bytes: u64,
    },
    RepositorySearch {
        scope: SanitizedDisplayPath,
        matches: u32,
        complete: bool,
    },
    ProcessExecution {
        profile: ExecutionProfile,
        exit_status: Option<i32>,
    },
    PolicyCheck {
        capability_id: String,
        outcome: ActivityPolicyOutcome,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceActivityDetail {
    pub stage: InferenceProductStageView,
    pub completed: Option<u64>,
    pub total: Option<u64>,
    pub unit: Option<InferenceProgressUnit>,
    pub cache: ActivityCacheDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityAggregateReason {
    Retention,
    NodeLimit,
    ByteLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityAggregateDetail {
    pub collapsed_nodes: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub incomplete: u32,
    pub elapsed_ms: u64,
    pub reason: ActivityAggregateReason,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "detail",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ActivityDetailView {
    #[default]
    None,
    Capability(CapabilityActivityDetail),
    Inference(InferenceActivityDetail),
    Aggregate(ActivityAggregateDetail),
    UnknownCapability {
        capability_id: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityRemovalReason {
    CollapsedIntoAggregate,
    RetentionExpired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityNodeRemoval {
    pub subtree_root_id: String,
    pub reason: ActivityRemovalReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityNodeView {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub order_index: u64,
    pub run_id: RunId,
    pub turn_id: Option<TurnId>,
    pub attempt_id: Option<AttemptId>,
    pub step_id: Option<StepId>,
    pub kind: ActivityNodeKind,
    pub phase: ActivityPhase,
    pub state: ActivityNodeState,
    pub retry: u32,
    pub started_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub finished_at_unix_ms: Option<i64>,
    pub elapsed_ms: u64,
    pub summary: String,
    pub detail: ActivityDetailView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityGraphView {
    pub graph_revision: u64,
    pub roots: Vec<String>,
    pub nodes: Vec<ActivityNodeView>,
    pub current_path: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityGraphDeltaBatch {
    pub graph_revision: u64,
    pub upserts: Vec<ActivityNodeView>,
    pub removals: Vec<ActivityNodeRemoval>,
    pub current_path: Option<Vec<String>>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalOwnerView {
    Human {
        session_id: SessionId,
    },
    MainAgent {
        session_id: SessionId,
    },
    Subagent {
        root_run_id: RunId,
        owner_run_id: RunId,
    },
    SessionPromoted {
        session_id: SessionId,
        previous_owner_run_id: RunId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellProfileView {
    pub profile_id: String,
    pub program: SanitizedDisplayPath,
    pub executable_digest: String,
    pub config_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalPromptState {
    Starting,
    Ready,
    CommandRunning,
    ForegroundProcess,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalWriterView {
    Unassigned,
    Owner,
    HumanTakeover,
    Revoked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSessionView {
    pub terminal_id: TerminalSessionId,
    pub execution_id: ExecutionId,
    pub owner: TerminalOwnerView,
    pub profile: ExecutionProfile,
    pub shell: ShellProfileView,
    pub workspace_root: SanitizedDisplayPath,
    pub cwd: SanitizedDisplayPath,
    pub initial_environment_digest: String,
    pub environment_names: Vec<String>,
    pub command_sequence: u64,
    pub prompt_generation: Option<u64>,
    pub prompt_state: TerminalPromptState,
    pub process_state: ExecutionState,
    pub exit: Option<ExecutionExit>,
    pub writer: TerminalWriterView,
    pub promoted: bool,
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
    pub older_page_cursor: Option<String>,
    pub header: SessionHeader,
    pub items: Vec<SessionPresentationItem>,
    pub active_run: Option<ActiveRunView>,
    pub queued_prompts: Vec<QueuedPromptView>,
    pub terminals: Vec<TerminalSessionView>,
    pub executions: Vec<ExecutionView>,
    pub human_commands: Vec<HumanCommandCardView>,
    pub activity: Option<ActivityGraphView>,
    pub command_context: CommandContext,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PresentationSnapshotDigest(String);

impl PresentationSnapshotDigest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut value = String::with_capacity("sha256:".len() + digest.len() * 2);
        value.push_str("sha256:");
        for byte in digest {
            write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
        }
        Self(value)
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, SurfaceValidationError> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(SurfaceValidationError::new(
                "presentation snapshot digest must use sha256",
            ));
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SurfaceValidationError::new(
                "presentation snapshot digest must be lowercase sha256 hex",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PresentationSnapshotDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PresentationSnapshotDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPresentationSnapshotTransferPurpose {
    Requested,
    SubscriptionInitial,
    Replacement { event_id: EventId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSnapshotTransferIdentity {
    pub transfer_id: RequestId,
    pub session_id: SessionId,
    pub cursor: PresentationCursor,
    pub purpose: SessionPresentationSnapshotTransferPurpose,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSnapshotManifestEvent {
    pub transfer: SessionPresentationSnapshotTransferIdentity,
    pub item_count: u32,
    pub decoded_bytes: u64,
    pub chunk_count: u16,
    pub digest: PresentationSnapshotDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSnapshotChunkEvent {
    pub transfer: SessionPresentationSnapshotTransferIdentity,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub bytes: crate::ProcessBytes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationSnapshotFinishedEvent {
    pub transfer: SessionPresentationSnapshotTransferIdentity,
    pub item_count: u32,
    pub decoded_bytes: u64,
    pub chunk_count: u16,
    pub digest: PresentationSnapshotDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPresentationSnapshotTransfer {
    pub manifest: SessionPresentationSnapshotManifestEvent,
    pub chunks: Vec<SessionPresentationSnapshotChunkEvent>,
    pub finished: SessionPresentationSnapshotFinishedEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPresentationEventPayload {
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
        cwd: SanitizedDisplayPath,
    },
    HumanCommandCardUpsert {
        card: HumanCommandCardView,
    },
    HumanCommandCardRemoved {
        terminal_id: TerminalSessionId,
        command_sequence: u64,
    },
    ActivityGraphDelta {
        batch: ActivityGraphDeltaBatch,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEnsureDisposition {
    Created,
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalEnsuredEvent {
    pub terminal: TerminalSessionView,
    pub disposition: TerminalEnsureDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalCommandAcceptedEvent {
    pub terminal_id: TerminalSessionId,
    pub command_sequence: u64,
    pub output_after_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationActionResult {
    SessionOpened {
        session_id: SessionId,
        resumed: bool,
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
    Terminals {
        terminals: Vec<TerminalSessionView>,
    },
    TerminalPromoted {
        terminal: TerminalSessionView,
    },
    Executions {
        executions: Vec<ExecutionView>,
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
        terminated_terminals: u32,
    },
    IncompleteTurnContinued {
        admission: PromptAdmission,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptAdmission {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub ordinal: u32,
    pub queued: bool,
    pub state: PromptAdmissionState,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptAdmissionState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Incomplete,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationActionResultEvent {
    pub result: ApplicationActionResult,
}

impl StructuredEnvironmentOverlay {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        let name_count = self.values.len() + self.inherited_names.len() + self.secret_refs.len();
        bound_count(
            name_count,
            MAX_ENVIRONMENT_NAMES,
            "environment overlay names",
        )?;
        let value_bytes = self.values.values().map(String::len).sum::<usize>();
        bound_count(
            value_bytes,
            MAX_ENVIRONMENT_VALUE_BYTES,
            "environment overlay values",
        )?;

        let mut seen = BTreeSet::new();
        for (name, value) in &self.values {
            validate_overlay_environment_name(name)?;
            bound_string(
                value,
                MAX_ENVIRONMENT_SINGLE_VALUE_BYTES,
                "environment value",
                true,
            )?;
            if value.contains('\0') {
                return Err(SurfaceValidationError::new(
                    "environment values must not contain NUL",
                ));
            }
            if !seen.insert(name.as_str()) {
                return Err(SurfaceValidationError::new(
                    "environment names must be unique across overlay sources",
                ));
            }
        }
        let mut overlay_bytes = self
            .values
            .iter()
            .map(|(name, value)| name.len().saturating_add(value.len()))
            .sum::<usize>();
        for name in &self.inherited_names {
            validate_overlay_environment_name(name)?;
            if !seen.insert(name.as_str()) {
                return Err(SurfaceValidationError::new(
                    "environment names must be unique across overlay sources",
                ));
            }
            overlay_bytes = overlay_bytes.saturating_add(name.len());
        }
        for secret in &self.secret_refs {
            validate_overlay_environment_name(&secret.name)?;
            bound_string(
                &secret.reference_id,
                MAX_SECRET_REFERENCE_BYTES,
                "secret environment reference",
                false,
            )?;
            if secret.reference_id.contains(['\0', '\n', '\r']) {
                return Err(SurfaceValidationError::new(
                    "secret environment reference must be single-line text",
                ));
            }
            if !seen.insert(secret.name.as_str()) {
                return Err(SurfaceValidationError::new(
                    "environment names must be unique across overlay sources",
                ));
            }
            overlay_bytes = overlay_bytes
                .saturating_add(secret.name.len())
                .saturating_add(secret.reference_id.len());
        }
        bound_count(
            overlay_bytes,
            MAX_ENVIRONMENT_VALUE_BYTES,
            "environment overlay",
        )?;
        Ok(())
    }
}

impl HumanTerminalEnsureRequest {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        bound_string(
            &self.client_submission_id,
            MAX_IDENTIFIER_BYTES,
            "client submission ID",
            false,
        )?;
        validate_single_line(&self.client_submission_id, "client submission ID")?;
        bound_string(
            &self.shell_profile_id,
            MAX_IDENTIFIER_BYTES,
            "shell profile ID",
            false,
        )?;
        validate_single_line(&self.shell_profile_id, "shell profile ID")?;
        self.terminal_size.validate().map_err(|_| {
            SurfaceValidationError::new("terminal columns and rows must be nonzero")
        })?;
        if self.profile == ExecutionProfile::Workspace
            && self.host_startup != HostStartupPolicy::ManagedOnly
        {
            return Err(SurfaceValidationError::new(
                "workspace terminals require managed-only startup",
            ));
        }
        self.agl_env.validate()
    }
}

impl HumanTerminalCommandSubmitRequest {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        bound_string(
            &self.client_submission_id,
            MAX_IDENTIFIER_BYTES,
            "client submission ID",
            false,
        )?;
        validate_single_line(&self.client_submission_id, "client submission ID")?;
        bound_string(
            &self.command,
            MAX_HUMAN_COMMAND_BYTES,
            "Human terminal command",
            false,
        )?;
        if self
            .command
            .chars()
            .any(is_forbidden_human_command_character)
        {
            return Err(SurfaceValidationError::new(
                "Human terminal command contains a forbidden control character",
            ));
        }
        Ok(())
    }
}

impl HumanCommandCardView {
    fn validate(&self) -> Result<(), SurfaceValidationError> {
        self.command.validate(MAX_HUMAN_COMMAND_BYTES, false)?;
        self.output.validate(MAX_HUMAN_COMMAND_OUTPUT_BYTES, true)?;
        if self.output_start.after_sequence > self.output_end.after_sequence
            || self.updated_at_unix_ms < self.started_at_unix_ms
        {
            return Err(SurfaceValidationError::new(
                "Human command card cursors or timestamps are inconsistent",
            ));
        }
        self.cwd.validate()?;
        if matches!(self.state, HumanCommandCardState::Exited) != self.exit_status.is_some() {
            return Err(SurfaceValidationError::new(
                "only an exited Human command card carries an exit status",
            ));
        }
        Ok(())
    }
}

impl ActivityNodeView {
    fn validate(&self) -> Result<(), SurfaceValidationError> {
        bound_string(&self.node_id, 512, "activity node ID", false)?;
        bound_optional_string(
            self.parent_node_id.as_deref(),
            512,
            "activity parent node ID",
        )?;
        bound_string(
            &self.summary,
            MAX_ACTIVITY_SUMMARY_BYTES,
            "activity summary",
            true,
        )?;
        if self
            .node_id
            .chars()
            .any(is_forbidden_presentation_character)
            || self
                .parent_node_id
                .as_ref()
                .is_some_and(|id| id.chars().any(is_forbidden_presentation_character))
            || self
                .summary
                .chars()
                .any(is_forbidden_presentation_character)
            || contains_absolute_display_path(&self.summary)
        {
            return Err(SurfaceValidationError::new(
                "activity summary contains forbidden presentation controls",
            ));
        }
        self.detail.validate()?;
        let terminal = self.state.is_terminal();
        if self.started_at_unix_ms < 0
            || self.updated_at_unix_ms < self.started_at_unix_ms
            || terminal != self.finished_at_unix_ms.is_some()
            || self
                .finished_at_unix_ms
                .is_some_and(|finished| finished < self.started_at_unix_ms)
            || self.finished_at_unix_ms.is_some_and(|finished| {
                self.elapsed_ms
                    > u64::try_from(finished.saturating_sub(self.started_at_unix_ms))
                        .unwrap_or_default()
            })
        {
            return Err(SurfaceValidationError::new(
                "activity node timing is inconsistent with its state",
            ));
        }
        if serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("activity node could not be encoded"))?
            .len()
            > MAX_ACTIVITY_NODE_BYTES
        {
            return Err(SurfaceValidationError::new(
                "activity node exceeds its encoded display bound",
            ));
        }
        Ok(())
    }
}

impl ActivityDetailView {
    fn validate(&self) -> Result<(), SurfaceValidationError> {
        let path = match self {
            Self::Capability(CapabilityActivityDetail::FilesystemList { path, .. })
            | Self::Capability(CapabilityActivityDetail::FilesystemRead { path, .. }) => Some(path),
            Self::Capability(CapabilityActivityDetail::RepositorySearch { scope, .. }) => {
                Some(scope)
            }
            _ => None,
        };
        if let Some(path) = path {
            path.validate()?;
            if !is_redacted_capability_display_path(&path.text) {
                return Err(SurfaceValidationError::new(
                    "capability activity path must be a normalized workspace-relative display value",
                ));
            }
        }
        let capability_id = match self {
            Self::Capability(CapabilityActivityDetail::PolicyCheck { capability_id, .. })
            | Self::UnknownCapability { capability_id } => Some(capability_id),
            _ => None,
        };
        if let Some(capability_id) = capability_id {
            bound_string(
                capability_id,
                MAX_IDENTIFIER_BYTES,
                "activity capability ID",
                false,
            )?;
            if capability_id
                .chars()
                .any(is_forbidden_presentation_character)
                || contains_absolute_display_path(capability_id)
            {
                return Err(SurfaceValidationError::new(
                    "activity capability identity contains unsafe display data",
                ));
            }
        }
        if let Self::Inference(detail) = self
            && matches!((detail.completed, detail.total), (Some(done), Some(total)) if done > total)
        {
            return Err(SurfaceValidationError::new(
                "activity inference progress cannot exceed its total",
            ));
        }
        if let Self::Inference(detail) = self
            && (detail.completed.is_some() || detail.total.is_some())
            && detail.unit.is_none()
        {
            return Err(SurfaceValidationError::new(
                "activity inference counters require a typed unit",
            ));
        }
        if let Self::Inference(detail) = self {
            let expected = match detail.stage {
                InferenceProductStageView::ModelLoad => ActivityCacheDisposition::Cold,
                InferenceProductStageView::ModelReuse | InferenceProductStageView::ContextReuse => {
                    ActivityCacheDisposition::Reused
                }
                InferenceProductStageView::ContextRebuild => ActivityCacheDisposition::Rebuilt,
                _ => ActivityCacheDisposition::NotApplicable,
            };
            if detail.cache != expected {
                return Err(SurfaceValidationError::new(
                    "activity inference cache disposition does not match its stage",
                ));
            }
        }
        if let Self::Aggregate(detail) = self
            && detail.collapsed_nodes == 0
        {
            return Err(SurfaceValidationError::new(
                "activity aggregate must represent at least one collapsed node",
            ));
        }
        Ok(())
    }
}

impl ActivityGraphView {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        if self.graph_revision == 0 {
            return Err(SurfaceValidationError::new(
                "activity graph revision must be nonzero",
            ));
        }
        bound_count(self.nodes.len(), MAX_ACTIVITY_NODES, "activity nodes")?;
        bound_count(self.roots.len(), MAX_ACTIVITY_NODES, "activity roots")?;
        bound_count(
            self.current_path.len(),
            MAX_ACTIVITY_PATH_NODES,
            "activity current path",
        )?;
        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            node.validate()?;
            if !ids.insert(node.node_id.as_str()) {
                return Err(SurfaceValidationError::new(
                    "activity graph node identities must be unique",
                ));
            }
        }
        let derived_roots = self
            .nodes
            .iter()
            .filter(|node| node.parent_node_id.is_none())
            .collect::<Vec<_>>();
        if derived_roots.len() != self.roots.len()
            || derived_roots
                .iter()
                .zip(&self.roots)
                .any(|(node, id)| &node.node_id != id)
            || derived_roots.iter().any(|root| {
                !matches!(
                    root.kind,
                    ActivityNodeKind::Run | ActivityNodeKind::Aggregate
                )
            })
        {
            return Err(SurfaceValidationError::new(
                "activity graph roots must be canonical run or aggregate roots",
            ));
        }
        for node in &self.nodes {
            if node
                .parent_node_id
                .as_ref()
                .is_some_and(|parent| !ids.contains(parent.as_str()))
            {
                return Err(SurfaceValidationError::new(
                    "activity graph parent must reference an existing node",
                ));
            }
            let mut cursor = Some(node.node_id.as_str());
            let mut visited = BTreeSet::new();
            while let Some(node_id) = cursor {
                if !visited.insert(node_id) {
                    return Err(SurfaceValidationError::new(
                        "activity graph must be acyclic",
                    ));
                }
                let current = self
                    .nodes
                    .iter()
                    .find(|candidate| candidate.node_id == node_id)
                    .expect("validated activity node identity exists");
                cursor = current.parent_node_id.as_deref();
            }
            if !visited
                .iter()
                .any(|id| self.roots.iter().any(|root| root == *id))
            {
                return Err(SurfaceValidationError::new(
                    "activity graph node is disconnected from its run root",
                ));
            }
        }
        let canonical = canonical_activity_node_ids(&self.nodes)?;
        if canonical
            .iter()
            .zip(&self.nodes)
            .any(|(id, node)| *id != node.node_id)
            || canonical.len() != self.nodes.len()
        {
            return Err(SurfaceValidationError::new(
                "activity nodes must use deterministic parent-before-child ordering",
            ));
        }
        let mut order_indices = BTreeSet::new();
        if self
            .nodes
            .iter()
            .any(|node| node.order_index == 0 || !order_indices.insert(node.order_index))
        {
            return Err(SurfaceValidationError::new(
                "activity order indices must be unique and nonzero",
            ));
        }
        let has_active = self.nodes.iter().any(|node| !node.state.is_terminal());
        let path_valid = (!has_active && self.current_path.is_empty())
            || (has_active
                && self
                    .current_path
                    .first()
                    .is_some_and(|root| self.roots.contains(root))
                && self.current_path.iter().all(|id| {
                    ids.contains(id.as_str())
                        && self
                            .nodes
                            .iter()
                            .find(|node| &node.node_id == id)
                            .is_some_and(|node| {
                                !node.state.is_terminal()
                                    && node.kind != ActivityNodeKind::Aggregate
                            })
                })
                && self.current_path.windows(2).all(|pair| {
                    self.nodes
                        .iter()
                        .find(|node| node.node_id == pair[1])
                        .and_then(|node| node.parent_node_id.as_deref())
                        == Some(pair[0].as_str())
                })
                && self.current_path.last().is_some_and(|leaf| {
                    self.nodes.iter().all(|node| {
                        node.parent_node_id.as_deref() != Some(leaf.as_str())
                            || node.state.is_terminal()
                    })
                })
                && self.current_path.iter().collect::<BTreeSet<_>>().len()
                    == self.current_path.len());
        if !path_valid || self.current_path != deterministic_activity_current_path(&self.nodes) {
            return Err(SurfaceValidationError::new(
                "activity current path must be a connected root-to-node chain",
            ));
        }
        if serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("activity graph could not be encoded"))?
            .len()
            > MAX_ACTIVITY_GRAPH_BYTES
        {
            return Err(SurfaceValidationError::new(
                "activity graph exceeds its encoded display bound",
            ));
        }
        Ok(())
    }
}

impl ActivityGraphDeltaBatch {
    pub fn validate_shape(&self) -> Result<(), SurfaceValidationError> {
        if self.graph_revision == 0 {
            return Err(SurfaceValidationError::new(
                "activity delta revision must be nonzero",
            ));
        }
        bound_count(
            self.upserts.len(),
            MAX_ACTIVITY_NODES,
            "activity delta upserts",
        )?;
        bound_count(
            self.removals.len(),
            MAX_ACTIVITY_NODES,
            "activity delta removals",
        )?;
        if let Some(path) = &self.current_path {
            bound_count(
                path.len(),
                MAX_ACTIVITY_PATH_NODES,
                "activity delta current path",
            )?;
            for id in path {
                bound_string(id, 512, "activity delta path ID", false)?;
                if id.chars().any(is_forbidden_presentation_character) {
                    return Err(SurfaceValidationError::new(
                        "activity delta path contains an unsafe node identity",
                    ));
                }
            }
        }
        let mut upserts = BTreeSet::new();
        let mut order_indices = BTreeSet::new();
        for (index, node) in self.upserts.iter().enumerate() {
            node.validate()?;
            if node.order_index == 0
                || !upserts.insert(node.node_id.as_str())
                || !order_indices.insert(node.order_index)
            {
                return Err(SurfaceValidationError::new(
                    "activity delta upsert identities and order indices must be unique and nonzero",
                ));
            }
            if node.parent_node_id.as_ref().is_some_and(|parent| {
                self.upserts
                    .iter()
                    .position(|candidate| &candidate.node_id == parent)
                    .is_some_and(|parent_index| parent_index >= index)
            }) {
                return Err(SurfaceValidationError::new(
                    "activity delta parents must precede their children",
                ));
            }
        }
        let mut removals = BTreeSet::new();
        for removal in &self.removals {
            bound_string(
                &removal.subtree_root_id,
                512,
                "activity delta removal ID",
                false,
            )?;
            if removal
                .subtree_root_id
                .chars()
                .any(is_forbidden_presentation_character)
                || !removals.insert(removal.subtree_root_id.as_str())
                || upserts.contains(removal.subtree_root_id.as_str())
            {
                return Err(SurfaceValidationError::new(
                    "activity delta removal identities must be unique",
                ));
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("activity delta could not be encoded"))?
            .len()
            > MAX_ACTIVITY_DELTA_BYTES
        {
            return Err(SurfaceValidationError::new(
                "activity delta exceeds its encoded wire bound",
            ));
        }
        Ok(())
    }
}

fn canonical_activity_node_ids(
    nodes: &[ActivityNodeView],
) -> Result<Vec<String>, SurfaceValidationError> {
    let mut children = BTreeMap::<Option<&str>, Vec<&ActivityNodeView>>::new();
    for node in nodes {
        children
            .entry(node.parent_node_id.as_deref())
            .or_default()
            .push(node);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|left, right| {
            (left.order_index, left.node_id.as_str())
                .cmp(&(right.order_index, right.node_id.as_str()))
        });
    }
    fn visit(
        parent: Option<&str>,
        children: &BTreeMap<Option<&str>, Vec<&ActivityNodeView>>,
        visiting: &mut BTreeSet<String>,
        output: &mut Vec<String>,
    ) -> Result<(), SurfaceValidationError> {
        for node in children.get(&parent).into_iter().flatten() {
            if !visiting.insert(node.node_id.clone()) {
                return Err(SurfaceValidationError::new(
                    "activity graph must be acyclic",
                ));
            }
            output.push(node.node_id.clone());
            visit(Some(&node.node_id), children, visiting, output)?;
            visiting.remove(&node.node_id);
        }
        Ok(())
    }
    let mut output = Vec::with_capacity(nodes.len());
    visit(None, &children, &mut BTreeSet::new(), &mut output)?;
    Ok(output)
}

fn deterministic_activity_current_path(nodes: &[ActivityNodeView]) -> Vec<String> {
    let priority = |state: ActivityNodeState| match state {
        ActivityNodeState::Running => Some(0u8),
        ActivityNodeState::Waiting => Some(1),
        ActivityNodeState::Pending => Some(2),
        _ => None,
    };
    let depth = |node: &ActivityNodeView| {
        let mut depth = 0usize;
        let mut parent = node.parent_node_id.as_deref();
        while let Some(parent_id) = parent {
            let Some(parent_node) = nodes.iter().find(|node| node.node_id == parent_id) else {
                break;
            };
            depth = depth.saturating_add(1);
            parent = parent_node.parent_node_id.as_deref();
        }
        depth
    };
    let mut leaves = nodes
        .iter()
        .filter(|node| priority(node.state).is_some())
        .filter(|node| {
            nodes.iter().all(|child| {
                child.parent_node_id.as_deref() != Some(node.node_id.as_str())
                    || priority(child.state).is_none()
            })
        })
        .collect::<Vec<_>>();
    leaves.sort_by(|left, right| {
        priority(left.state)
            .cmp(&priority(right.state))
            .then_with(|| right.updated_at_unix_ms.cmp(&left.updated_at_unix_ms))
            .then_with(|| depth(right).cmp(&depth(left)))
            .then_with(|| left.order_index.cmp(&right.order_index))
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    let Some(leaf) = leaves.first() else {
        return Vec::new();
    };
    let mut path = vec![leaf.node_id.clone()];
    let mut parent = leaf.parent_node_id.as_deref();
    while let Some(parent_id) = parent {
        let Some(parent_node) = nodes.iter().find(|node| node.node_id == parent_id) else {
            return Vec::new();
        };
        path.push(parent_node.node_id.clone());
        parent = parent_node.parent_node_id.as_deref();
    }
    path.reverse();
    path
}

fn contains_absolute_display_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| {
        *byte == b'/'
            && bytes.get(index + 1).is_some_and(|next| *next != b'/')
            && (index == 0
                || bytes[index - 1].is_ascii_whitespace()
                || b"([{=:,'\"".contains(&bytes[index - 1]))
    })
}

fn is_redacted_capability_display_path(value: &str) -> bool {
    !value.starts_with('/')
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

impl HumanHostTerminalEnsureRequest {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        self.terminal.validate()?;
        if self.terminal.profile != ExecutionProfile::Host {
            return Err(SurfaceValidationError::new(
                "local-operator Host admission requires a Host terminal profile",
            ));
        }
        if !self.confirm_host_authority {
            return Err(SurfaceValidationError::new(
                "local-operator Host admission requires explicit confirmation",
            ));
        }
        Ok(())
    }
}

impl TerminalSessionView {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        validate_shell_profile(&self.shell)?;
        self.workspace_root.validate()?;
        self.cwd.validate()?;
        bound_string(
            &self.initial_environment_digest,
            MAX_DIGEST_BYTES,
            "initial environment digest",
            false,
        )?;
        validate_ascii_graphic(
            &self.initial_environment_digest,
            "initial environment digest",
        )?;
        bound_count(
            self.environment_names.len(),
            MAX_ENVIRONMENT_NAMES,
            "terminal environment names",
        )?;
        let mut names = BTreeSet::new();
        for name in &self.environment_names {
            validate_environment_name(name)?;
            if !names.insert(name) {
                return Err(SurfaceValidationError::new(
                    "terminal environment names must be unique",
                ));
            }
        }
        if self.profile == ExecutionProfile::Host
            && !matches!(self.owner, TerminalOwnerView::Human { .. })
        {
            return Err(SurfaceValidationError::new(
                "persistent host terminals require a Human owner",
            ));
        }
        if self.promoted != matches!(self.owner, TerminalOwnerView::SessionPromoted { .. }) {
            return Err(SurfaceValidationError::new(
                "terminal promoted flag must match its lifecycle owner",
            ));
        }
        if self.process_state.is_live() && self.exit.is_some() {
            return Err(SurfaceValidationError::new(
                "a live terminal cannot carry a process exit outcome",
            ));
        }
        if matches!(self.prompt_state, TerminalPromptState::Ready)
            != self.prompt_generation.is_some()
        {
            return Err(SurfaceValidationError::new(
                "terminal prompt generation must be present exactly for a trusted ready prompt",
            ));
        }
        validate_exit(self.exit.as_ref())
    }

    fn validate_for_session(&self, session_id: &SessionId) -> Result<(), SurfaceValidationError> {
        self.validate()?;
        let projected_session = match &self.owner {
            TerminalOwnerView::Human { session_id }
            | TerminalOwnerView::MainAgent { session_id }
            | TerminalOwnerView::SessionPromoted { session_id, .. } => Some(session_id),
            TerminalOwnerView::Subagent { .. } => None,
        };
        if projected_session.is_some_and(|projected| projected != session_id) {
            return Err(SurfaceValidationError::new(
                "terminal owner belongs to a different session",
            ));
        }
        Ok(())
    }
}

impl SessionPresentationSnapshot {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        bound_optional_string(
            self.older_page_cursor.as_deref(),
            MAX_IDENTIFIER_BYTES,
            "older presentation page cursor",
        )?;
        bound_count(
            self.items.len(),
            MAX_PRESENTATION_ITEMS,
            "presentation items",
        )?;
        bound_count(
            self.terminals.len(),
            MAX_TERMINAL_RECORDS,
            "terminal records",
        )?;
        bound_count(
            self.executions.len(),
            MAX_PRESENTATION_ITEMS,
            "execution records",
        )?;
        bound_count(
            self.human_commands.len(),
            MAX_HUMAN_COMMAND_CARDS,
            "Human command cards",
        )?;
        validate_header(&self.header)?;
        for item in &self.items {
            validate_item(item)?;
        }
        for run in self.active_run.iter() {
            bound_string(&run.state, MAX_IDENTIFIER_BYTES, "run state", false)?;
        }
        bound_count(
            self.queued_prompts.len(),
            MAX_PRESENTATION_ITEMS,
            "queued prompts",
        )?;
        for terminal in &self.terminals {
            terminal.validate_for_session(&self.session_id)?;
        }
        for execution in &self.executions {
            validate_execution(execution)?;
        }
        let mut command_keys = BTreeSet::new();
        let mut command_output_bytes = 0usize;
        for command in &self.human_commands {
            command.validate()?;
            command_output_bytes =
                command_output_bytes.saturating_add(command.output.as_str().len());
            if !command_keys.insert((&command.terminal_id, command.command_sequence)) {
                return Err(SurfaceValidationError::new(
                    "Human command card identities must be unique",
                ));
            }
        }
        if command_output_bytes > MAX_HUMAN_COMMAND_AGGREGATE_OUTPUT_BYTES {
            return Err(SurfaceValidationError::new(
                "Human command cards exceed their aggregate output bound",
            ));
        }
        if let Some(activity) = &self.activity {
            activity.validate()?;
        }
        if self.header.session_id != self.session_id
            || self
                .command_context
                .session_id
                .as_ref()
                .is_some_and(|session_id| session_id != &self.session_id)
        {
            return Err(SurfaceValidationError::new(
                "snapshot session identities must agree",
            ));
        }
        let mut terminal_ids = BTreeSet::new();
        let mut terminal_execution_ids = BTreeSet::new();
        for terminal in &self.terminals {
            if !terminal_ids.insert(&terminal.terminal_id)
                || !terminal_execution_ids.insert(&terminal.execution_id)
            {
                return Err(SurfaceValidationError::new(
                    "snapshot terminal identities must be unique",
                ));
            }
        }
        let mut execution_ids = BTreeSet::new();
        for execution in &self.executions {
            if !execution_ids.insert(&execution.execution_id) {
                return Err(SurfaceValidationError::new(
                    "snapshot execution identities must be unique",
                ));
            }
        }
        let decoded_bytes = serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("presentation snapshot is not encodable"))?
            .len();
        bound_count(
            decoded_bytes,
            MAX_PRESENTATION_CONTENT_BYTES,
            "presentation decoded content",
        )
    }

    pub fn canonical_json_bytes(&self) -> Result<Vec<u8>, SurfaceValidationError> {
        self.validate()?;
        serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("presentation snapshot is not encodable"))
    }
}

impl SessionPresentationSnapshotTransferIdentity {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        if let SessionPresentationSnapshotTransferPurpose::Replacement { event_id } = &self.purpose
            && event_id.to_string().is_empty()
        {
            return Err(SurfaceValidationError::new(
                "replacement snapshot event identity must be nonempty",
            ));
        }
        Ok(())
    }
}

impl SessionPresentationSnapshotManifestEvent {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        self.transfer.validate()?;
        validate_snapshot_transfer_summary(self.item_count, self.decoded_bytes, self.chunk_count)
    }
}

impl SessionPresentationSnapshotChunkEvent {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        self.transfer.validate()?;
        validate_snapshot_chunk_count(self.chunk_count)?;
        if self.chunk_index >= self.chunk_count {
            return Err(SurfaceValidationError::new(
                "presentation snapshot chunk index is outside the transfer",
            ));
        }
        if self.bytes.encoding != crate::ProcessBytesEncoding::Base64 {
            return Err(SurfaceValidationError::new(
                "presentation snapshot chunks must use base64 binary encoding",
            ));
        }
        bound_count(
            self.bytes.data.len(),
            MAX_PRESENTATION_SNAPSHOT_CHUNK_ENCODED_BYTES,
            "presentation snapshot encoded chunk bytes",
        )?;
        let decoded = self
            .bytes
            .decode(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES)
            .map_err(|_| {
                SurfaceValidationError::new("presentation snapshot chunk bytes are invalid")
            })?;
        if decoded.is_empty() || BASE64_STANDARD.encode(&decoded) != self.bytes.data {
            return Err(SurfaceValidationError::new(
                "presentation snapshot chunk is empty or not canonical base64",
            ));
        }
        if usize::from(self.chunk_index) + 1 < usize::from(self.chunk_count)
            && decoded.len() != MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES
        {
            return Err(SurfaceValidationError::new(
                "non-final presentation snapshot chunks must have the canonical size",
            ));
        }
        Ok(())
    }
}

impl SessionPresentationSnapshotFinishedEvent {
    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        self.transfer.validate()?;
        validate_snapshot_transfer_summary(self.item_count, self.decoded_bytes, self.chunk_count)
    }
}

impl SessionPresentationSnapshotTransfer {
    pub fn encode(
        transfer_id: RequestId,
        purpose: SessionPresentationSnapshotTransferPurpose,
        snapshot: &SessionPresentationSnapshot,
    ) -> Result<Self, SurfaceValidationError> {
        let bytes = snapshot.canonical_json_bytes()?;
        let decoded_bytes = u64::try_from(bytes.len()).map_err(|_| {
            SurfaceValidationError::new("presentation snapshot byte length does not fit u64")
        })?;
        let item_count = u32::try_from(snapshot.items.len()).map_err(|_| {
            SurfaceValidationError::new("presentation snapshot item count does not fit u32")
        })?;
        let chunk_count = u16::try_from(
            bytes.len().div_ceil(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES),
        )
        .map_err(|_| {
            SurfaceValidationError::new("presentation snapshot chunk count does not fit u16")
        })?;
        let digest = PresentationSnapshotDigest::from_bytes(&bytes);
        let transfer = SessionPresentationSnapshotTransferIdentity {
            transfer_id,
            session_id: snapshot.session_id.clone(),
            cursor: snapshot.cursor.clone(),
            purpose,
        };
        let manifest = SessionPresentationSnapshotManifestEvent {
            transfer: transfer.clone(),
            item_count,
            decoded_bytes,
            chunk_count,
            digest: digest.clone(),
        };
        let chunks = bytes
            .chunks(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES)
            .enumerate()
            .map(|(index, bytes)| {
                Ok(SessionPresentationSnapshotChunkEvent {
                    transfer: transfer.clone(),
                    chunk_index: u16::try_from(index).map_err(|_| {
                        SurfaceValidationError::new(
                            "presentation snapshot chunk index does not fit u16",
                        )
                    })?,
                    chunk_count,
                    bytes: crate::ProcessBytes {
                        encoding: crate::ProcessBytesEncoding::Base64,
                        data: BASE64_STANDARD.encode(bytes),
                    },
                })
            })
            .collect::<Result<Vec<_>, SurfaceValidationError>>()?;
        let finished = SessionPresentationSnapshotFinishedEvent {
            transfer,
            item_count,
            decoded_bytes,
            chunk_count,
            digest,
        };
        manifest.validate()?;
        for chunk in &chunks {
            chunk.validate()?;
        }
        finished.validate()?;
        crate::DaemonEvent::new(
            Some(RequestId::generate()),
            DaemonEventKind::SessionPresentationSnapshotManifest(manifest.clone()),
        )
        .validate()?;
        for chunk in &chunks {
            crate::DaemonEvent::new(
                Some(RequestId::generate()),
                DaemonEventKind::SessionPresentationSnapshotChunk(chunk.clone()),
            )
            .validate()?;
        }
        crate::DaemonEvent::new(
            Some(RequestId::generate()),
            DaemonEventKind::SessionPresentationSnapshotFinished(finished.clone()),
        )
        .validate()?;
        Ok(Self {
            manifest,
            chunks,
            finished,
        })
    }
}

fn validate_snapshot_chunk_count(chunk_count: u16) -> Result<(), SurfaceValidationError> {
    if chunk_count == 0 || usize::from(chunk_count) > MAX_PRESENTATION_SNAPSHOT_CHUNKS {
        return Err(SurfaceValidationError::new(
            "presentation snapshot chunk count is outside the bounded range",
        ));
    }
    Ok(())
}

fn validate_snapshot_transfer_summary(
    item_count: u32,
    decoded_bytes: u64,
    chunk_count: u16,
) -> Result<(), SurfaceValidationError> {
    bound_count(
        usize::try_from(item_count).unwrap_or(usize::MAX),
        MAX_PRESENTATION_ITEMS,
        "presentation snapshot transfer items",
    )?;
    let decoded_bytes = usize::try_from(decoded_bytes).map_err(|_| {
        SurfaceValidationError::new("presentation snapshot decoded byte count does not fit usize")
    })?;
    if decoded_bytes == 0 || decoded_bytes > MAX_PRESENTATION_CONTENT_BYTES {
        return Err(SurfaceValidationError::new(
            "presentation snapshot decoded byte count is outside the bounded range",
        ));
    }
    validate_snapshot_chunk_count(chunk_count)?;
    let canonical_count = decoded_bytes.div_ceil(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES);
    if canonical_count != usize::from(chunk_count) {
        return Err(SurfaceValidationError::new(
            "presentation snapshot transfer does not use the canonical chunk count",
        ));
    }
    Ok(())
}

impl DaemonRequestKind {
    pub fn validate_surface(&self) -> Result<(), SurfaceValidationError> {
        match self {
            Self::SetupSmokeSessionOpen(request) => validate_setup_smoke_session_open(request),
            Self::CommandCatalog(request) => {
                bound_count(
                    request.client_effects.len(),
                    MAX_CLIENT_EFFECTS,
                    "client effects",
                )?;
                ensure_unique_copy(&request.client_effects, "client effects")
            }
            Self::CommandSuggestions(request) => {
                bound_string(
                    &request.command_id,
                    MAX_IDENTIFIER_BYTES,
                    "command ID",
                    false,
                )?;
                bound_string(
                    &request.argument_id,
                    MAX_IDENTIFIER_BYTES,
                    "argument ID",
                    false,
                )?;
                bound_string(
                    &request.query,
                    MAX_COMMAND_INPUT_BYTES,
                    "suggestion query",
                    true,
                )?;
                bound_optional_string(
                    request.cursor.as_deref(),
                    MAX_IDENTIFIER_BYTES,
                    "suggestion cursor",
                )
            }
            Self::ApplicationAction(request) => validate_application_action_request(request),
            Self::HumanTerminalEnsure(request) => request.validate(),
            Self::HumanHostTerminalEnsure(request) => request.validate(),
            Self::HumanTerminalCommandSubmit(request) => request.validate(),
            Self::SessionPresentation(request) => bound_optional_string(
                request.page_cursor.as_deref(),
                MAX_IDENTIFIER_BYTES,
                "presentation page cursor",
            ),
            Self::SessionPresentationSubscribe(_) | Self::SubscriptionCancel(_) => Ok(()),
            _ => Ok(()),
        }
    }
}

fn validate_setup_smoke_session_open(
    request: &crate::SetupSmokeSessionOpenRequest,
) -> Result<(), SurfaceValidationError> {
    bound_string(
        &request.workspace_root,
        MAX_PATH_BYTES,
        "setup smoke workspace root",
        false,
    )?;
    if !std::path::Path::new(&request.workspace_root).is_absolute()
        || request.workspace_root.contains('\0')
    {
        return Err(SurfaceValidationError::new(
            "setup smoke workspace root must be an absolute path without NUL bytes",
        ));
    }
    bound_string(
        &request.function_ref,
        MAX_PATH_BYTES,
        "setup smoke function reference",
        false,
    )?;
    if request.function_ref.contains('\0') {
        return Err(SurfaceValidationError::new(
            "setup smoke function reference cannot contain NUL bytes",
        ));
    }
    if request.max_output_tokens == 0 || request.max_output_tokens > MAX_SETUP_SMOKE_OUTPUT_TOKENS {
        return Err(SurfaceValidationError::new(format!(
            "setup smoke output limit must be between 1 and {MAX_SETUP_SMOKE_OUTPUT_TOKENS}"
        )));
    }
    request.staged_bindings.validate().map_err(|error| {
        SurfaceValidationError::new(format!("invalid staged bindings: {error}"))
    })?;
    bound_count(
        request.staged_bindings.models.len(),
        MAX_SETUP_SMOKE_MODEL_BINDINGS,
        "setup smoke model bindings",
    )?;
    for binding in request.staged_bindings.models.values() {
        let path = binding.path.to_str().ok_or_else(|| {
            SurfaceValidationError::new("setup smoke model binding path must be UTF-8")
        })?;
        bound_string(
            path,
            MAX_PATH_BYTES,
            "setup smoke model binding path",
            false,
        )?;
        if !binding.path.is_absolute() || path.contains('\0') {
            return Err(SurfaceValidationError::new(
                "setup smoke model binding path must be absolute and contain no NUL bytes",
            ));
        }
    }
    bound_string(
        &request.runtime_plan.profile_id,
        MAX_IDENTIFIER_BYTES,
        "setup smoke runtime profile ID",
        false,
    )?;
    bound_optional_string(
        request.runtime_plan.selected_device.as_deref(),
        MAX_LABEL_BYTES,
        "setup smoke selected device",
    )?;
    bound_string(
        &request.runtime_plan.expected_speed,
        MAX_LABEL_BYTES,
        "setup smoke expected speed",
        false,
    )?;
    if request.runtime_plan.smoke_timeout_seconds == 0
        || request.runtime_plan.smoke_timeout_seconds > 3_600
    {
        return Err(SurfaceValidationError::new(
            "setup smoke runtime timeout must be between 1 and 3600 seconds",
        ));
    }
    request
        .runtime_plan
        .runtime
        .validate()
        .map_err(|error| SurfaceValidationError::new(format!("invalid runtime plan: {error}")))?;
    bound_optional_string(
        request.runtime_plan.runtime.device.as_deref(),
        MAX_LABEL_BYTES,
        "setup smoke runtime device",
    )?;
    if let Some(draft_model) = request.runtime_plan.runtime.mtp.draft_model.as_deref() {
        let path = draft_model.to_str().ok_or_else(|| {
            SurfaceValidationError::new("setup smoke MTP draft model path must be UTF-8")
        })?;
        bound_string(
            path,
            MAX_PATH_BYTES,
            "setup smoke MTP draft model path",
            false,
        )?;
        if !draft_model.is_absolute() || path.contains('\0') {
            return Err(SurfaceValidationError::new(
                "setup smoke MTP draft model path must be absolute and contain no NUL bytes",
            ));
        }
    }
    Ok(())
}

impl DaemonEventKind {
    pub fn validate_surface(&self) -> Result<(), SurfaceValidationError> {
        match self {
            Self::CommandCatalog(event) => validate_catalog(event),
            Self::CommandSuggestions(event) => validate_suggestions(event),
            Self::ApplicationActionResult(event) => validate_action_result(&event.result),
            Self::SessionPresentationSnapshotManifest(event) => event.validate(),
            Self::SessionPresentationSnapshotChunk(event) => event.validate(),
            Self::SessionPresentationSnapshotFinished(event) => event.validate(),
            Self::SessionPresentationEvent(event) => validate_presentation_event(event),
            Self::SessionPresentationSubscriptionFinished(_) | Self::SubscriptionCancelled(_) => {
                Ok(())
            }
            Self::HumanTerminalEnsured(event) => event.terminal.validate(),
            Self::HumanTerminalCommandAccepted(_) => Ok(()),
            Self::InferenceInventory(event) => validate_inference_inventory(event),
            Self::InferenceStatus(event) => validate_inference_status(event),
            Self::Error(error) => {
                bound_string(
                    &error.message,
                    MAX_DISPLAY_BYTES,
                    "protocol error message",
                    false,
                )?;
                validate_safe_metadata(&error.safe_metadata)
            }
            _ => Ok(()),
        }
    }
}

fn validate_inference_inventory(
    inventory: &crate::InferenceInventoryEvent,
) -> Result<(), SurfaceValidationError> {
    bound_count(
        inventory.devices.len(),
        MAX_INFERENCE_DEVICES,
        "inference devices",
    )?;
    let mut physical_ids = BTreeSet::new();
    let mut backend_names = BTreeSet::new();
    for device in &inventory.devices {
        bound_string(
            &device.physical_device_id,
            MAX_IDENTIFIER_BYTES,
            "inference physical device ID",
            false,
        )?;
        bound_string(
            &device.driver_build_id,
            MAX_DIGEST_BYTES,
            "inference driver build ID",
            false,
        )?;
        bound_string(
            &device.backend_name,
            MAX_IDENTIFIER_BYTES,
            "inference backend name",
            false,
        )?;
        bound_string(
            &device.description,
            MAX_DISPLAY_BYTES,
            "inference device description",
            false,
        )?;
        if !physical_ids.insert(device.physical_device_id.as_str()) {
            return Err(SurfaceValidationError::new(
                "inference inventory contains a duplicate physical device ID",
            ));
        }
        if !backend_names.insert(device.backend_name.as_str()) {
            return Err(SurfaceValidationError::new(
                "inference inventory contains a duplicate backend name",
            ));
        }
        if device.free_memory_bytes > device.total_memory_bytes {
            return Err(SurfaceValidationError::new(
                "inference device free memory exceeds total memory",
            ));
        }
    }
    Ok(())
}

fn validate_inference_status(
    status: &crate::InferenceStatusEvent,
) -> Result<(), SurfaceValidationError> {
    bound_string(
        &status.worker_build_id,
        MAX_DIGEST_BYTES,
        "inference worker build ID",
        false,
    )?;
    if status.worker_pid == Some(0) {
        return Err(SurfaceValidationError::new(
            "inference worker PID must be nonzero",
        ));
    }
    if status.launch_generation == Some(0) {
        return Err(SurfaceValidationError::new(
            "inference worker launch generation must be nonzero",
        ));
    }
    if status.worker_pid.is_some() != status.launch_generation.is_some() {
        return Err(SurfaceValidationError::new(
            "inference worker PID and launch generation must be present together",
        ));
    }
    bound_optional_string(
        status.physical_device_id.as_deref(),
        MAX_IDENTIFIER_BYTES,
        "inference physical device ID",
    )
}

fn validate_catalog(catalog: &CommandCatalogEvent) -> Result<(), SurfaceValidationError> {
    bound_count(
        catalog.descriptors.len(),
        MAX_COMMAND_DESCRIPTORS,
        "command descriptors",
    )?;
    let mut descriptor_ids = BTreeSet::new();
    let mut command_names = BTreeSet::new();
    for descriptor in &catalog.descriptors {
        bound_string(
            &descriptor.id,
            MAX_IDENTIFIER_BYTES,
            "command descriptor ID",
            false,
        )?;
        validate_command_id(&descriptor.id)?;
        if !descriptor_ids.insert(descriptor.id.as_str()) {
            return Err(SurfaceValidationError::new(
                "command descriptor IDs must be unique",
            ));
        }
        bound_string(
            &descriptor.name,
            MAX_IDENTIFIER_BYTES,
            "command name",
            false,
        )?;
        validate_command_name(&descriptor.name)?;
        if !command_names.insert(descriptor.name.as_str()) {
            return Err(SurfaceValidationError::new(
                "command names and aliases must be globally unique",
            ));
        }
        bound_count(
            descriptor.aliases.len(),
            MAX_COMMAND_ARGUMENTS,
            "command aliases",
        )?;
        for alias in &descriptor.aliases {
            bound_string(alias, MAX_IDENTIFIER_BYTES, "command alias", false)?;
            validate_command_name(alias)?;
            if !command_names.insert(alias.as_str()) {
                return Err(SurfaceValidationError::new(
                    "command names and aliases must be globally unique",
                ));
            }
        }
        bound_string(
            &descriptor.summary,
            MAX_DISPLAY_BYTES,
            "command summary",
            false,
        )?;
        bound_count(
            descriptor.arguments.len(),
            MAX_COMMAND_ARGUMENTS,
            "command arguments",
        )?;
        let mut argument_ids = BTreeSet::new();
        for argument in &descriptor.arguments {
            bound_string(
                &argument.id,
                MAX_IDENTIFIER_BYTES,
                "command argument ID",
                false,
            )?;
            if !argument_ids.insert(argument.id.as_str()) {
                return Err(SurfaceValidationError::new(
                    "command argument IDs must be unique within a descriptor",
                ));
            }
            bound_string(
                &argument.label,
                MAX_LABEL_BYTES,
                "command argument label",
                false,
            )?;
            bound_optional_string(
                argument.suggestion_source.as_deref(),
                MAX_IDENTIFIER_BYTES,
                "suggestion source",
            )?;
        }
        if let CommandAvailability::Disabled {
            reason_code,
            message,
        } = &descriptor.availability
        {
            bound_string(
                reason_code,
                MAX_IDENTIFIER_BYTES,
                "availability reason code",
                false,
            )?;
            bound_string(message, MAX_DISPLAY_BYTES, "availability message", false)?;
        }
    }
    Ok(())
}

fn validate_suggestions(event: &CommandSuggestionsEvent) -> Result<(), SurfaceValidationError> {
    bound_count(event.entries.len(), MAX_SUGGESTIONS, "suggestions")?;
    for entry in &event.entries {
        bound_string(
            &entry.value,
            MAX_COMMAND_INPUT_BYTES,
            "suggestion value",
            true,
        )?;
        bound_string(&entry.label, MAX_LABEL_BYTES, "suggestion label", false)?;
        bound_optional_string(
            entry.detail.as_deref(),
            MAX_DISPLAY_BYTES,
            "suggestion detail",
        )?;
    }
    bound_optional_string(
        event.next_cursor.as_deref(),
        MAX_IDENTIFIER_BYTES,
        "suggestion cursor",
    )
}

fn validate_application_action_request(
    request: &ApplicationActionRequest,
) -> Result<(), SurfaceValidationError> {
    bound_string(
        &request.client_submission_id,
        MAX_IDENTIFIER_BYTES,
        "client submission ID",
        false,
    )?;
    match &request.action {
        ApplicationAction::SessionNew { launch } => validate_launch(launch),
        ApplicationAction::ModelSelect { model_id } => {
            bound_string(model_id, MAX_IDENTIFIER_BYTES, "model ID", false)
        }
        ApplicationAction::SkillsSelect { skill_ids } => {
            validate_identifier_list(skill_ids, MAX_SKILLS, "selected skill IDs", "skill ID")
        }
        ApplicationAction::WorkspaceSet { path, .. } => {
            bound_string(path, MAX_PATH_BYTES, "workspace path", false)?;
            if path.contains('\0') {
                return Err(SurfaceValidationError::new(
                    "workspace path must not contain NUL",
                ));
            }
            Ok(())
        }
        ApplicationAction::SessionResume { .. }
        | ApplicationAction::SessionStatus
        | ApplicationAction::OperationModeSelect { .. }
        | ApplicationAction::WorkspaceGet
        | ApplicationAction::TerminalList { .. }
        | ApplicationAction::TerminalPromote { .. }
        | ApplicationAction::IncompleteTurnContinue { .. }
        | ApplicationAction::ExecutionList { .. }
        | ApplicationAction::ExecutionAttach { .. }
        | ApplicationAction::ExecutionKill { .. }
        | ApplicationAction::RuntimeContextReload
        | ApplicationAction::SessionClear
        | ApplicationAction::SessionExit { .. } => Ok(()),
    }
}

fn validate_launch(launch: &SessionLaunchOptions) -> Result<(), SurfaceValidationError> {
    bound_optional_string(
        launch.workspace_root.as_deref(),
        MAX_PATH_BYTES,
        "workspace root",
    )?;
    bound_optional_string(
        launch.function_ref.as_deref(),
        MAX_IDENTIFIER_BYTES,
        "function reference",
    )?;
    bound_optional_string(launch.model_id.as_deref(), MAX_IDENTIFIER_BYTES, "model ID")?;
    validate_identifier_list(
        &launch.skill_ids,
        MAX_SKILLS,
        "launch skill IDs",
        "skill ID",
    )
}

fn validate_action_result(result: &ApplicationActionResult) -> Result<(), SurfaceValidationError> {
    match result {
        ApplicationActionResult::SessionOpened { .. } => Ok(()),
        ApplicationActionResult::Status { header } => validate_header(header),
        ApplicationActionResult::ModelChanged { header }
        | ApplicationActionResult::ModeChanged { header }
        | ApplicationActionResult::SkillsChanged { header }
        | ApplicationActionResult::WorkspaceChanged { header } => validate_header(header),
        ApplicationActionResult::Terminals { terminals } => validate_terminals(terminals),
        ApplicationActionResult::TerminalPromoted { terminal } => terminal.validate(),
        ApplicationActionResult::Executions { executions } => {
            bound_count(
                executions.len(),
                MAX_PRESENTATION_ITEMS,
                "execution results",
            )?;
            for execution in executions {
                validate_execution(execution)?;
            }
            Ok(())
        }
        ApplicationActionResult::Reloaded { visible_tools, .. } => validate_identifier_list(
            visible_tools,
            MAX_COMMAND_DESCRIPTORS,
            "visible tools",
            "tool ID",
        ),
        ApplicationActionResult::AttachAccepted { .. }
        | ApplicationActionResult::KillAccepted { .. }
        | ApplicationActionResult::Cleared { .. }
        | ApplicationActionResult::SessionExited { .. } => Ok(()),
        ApplicationActionResult::IncompleteTurnContinued { admission } => {
            if admission.queued
                != matches!(
                    admission.state,
                    PromptAdmissionState::Queued | PromptAdmissionState::Waiting
                )
            {
                return Err(SurfaceValidationError::new(
                    "continuation admission queue state is inconsistent",
                ));
            }
            Ok(())
        }
    }
}

fn validate_presentation_event(
    envelope: &SessionPresentationEventEnvelope,
) -> Result<(), SurfaceValidationError> {
    match &envelope.event {
        SessionPresentationEventPayload::HeaderChanged { header } => validate_header(header),
        SessionPresentationEventPayload::ItemUpsert { item } => validate_item(item),
        SessionPresentationEventPayload::ItemRemoved { item_key } => {
            bound_string(item_key, MAX_IDENTIFIER_BYTES, "item key", false)
        }
        SessionPresentationEventPayload::AssistantTextDelta { text, .. } => bound_string(
            text,
            MAX_ASSISTANT_DELTA_BYTES,
            "assistant text delta",
            true,
        ),
        SessionPresentationEventPayload::PromptFinished { state, .. } => {
            bound_string(state, MAX_IDENTIFIER_BYTES, "prompt state", false)
        }
        SessionPresentationEventPayload::TerminalAdded { terminal }
        | SessionPresentationEventPayload::TerminalChanged { terminal } => {
            terminal.validate_for_session(&envelope.session_id)
        }
        SessionPresentationEventPayload::TerminalCommandFinished { cwd, .. } => cwd.validate(),
        SessionPresentationEventPayload::HumanCommandCardUpsert { card } => card.validate(),
        SessionPresentationEventPayload::ActivityGraphDelta { batch } => batch.validate_shape(),
        SessionPresentationEventPayload::ExecutionStateChanged { execution } => {
            validate_execution(execution)
        }
        SessionPresentationEventPayload::Notice { code, message, .. } => {
            bound_string(code, MAX_IDENTIFIER_BYTES, "notice code", false)?;
            bound_string(message, MAX_DISPLAY_BYTES, "notice message", false)
        }
        SessionPresentationEventPayload::PromptQueued { .. }
        | SessionPresentationEventPayload::PromptActivated { .. }
        | SessionPresentationEventPayload::TerminalRemoved { .. }
        | SessionPresentationEventPayload::TerminalCommandStarted { .. }
        | SessionPresentationEventPayload::HumanCommandCardRemoved { .. }
        | SessionPresentationEventPayload::CommandAvailabilityChanged
        | SessionPresentationEventPayload::SessionFinished => Ok(()),
    }
}

fn validate_header(header: &SessionHeader) -> Result<(), SurfaceValidationError> {
    bound_optional_string(header.title.as_deref(), MAX_DISPLAY_BYTES, "session title")?;
    bound_string(
        &header.function_name,
        MAX_IDENTIFIER_BYTES,
        "function name",
        false,
    )?;
    header.workspace_root.validate()?;
    validate_workspace_history_scope(&header.workspace_history_scope)?;
    header.cwd.validate()?;
    bound_optional_string(header.model_id.as_deref(), MAX_IDENTIFIER_BYTES, "model ID")?;
    validate_identifier_list(
        &header.selected_skills,
        MAX_SKILLS,
        "selected skills",
        "skill ID",
    )?;
    Ok(())
}

fn validate_workspace_history_scope(scope: &str) -> Result<(), SurfaceValidationError> {
    let Some(digest) = scope.strip_prefix("sha256:") else {
        return Err(SurfaceValidationError::new(
            "workspace history scope must be an opaque SHA-256 identity",
        ));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SurfaceValidationError::new(
            "workspace history scope must be an opaque SHA-256 identity",
        ));
    }
    Ok(())
}

fn validate_item(item: &SessionPresentationItem) -> Result<(), SurfaceValidationError> {
    let result = match item {
        SessionPresentationItem::UserMessage { .. }
        | SessionPresentationItem::AssistantMessage { .. } => Ok(()),
        SessionPresentationItem::IncompleteAssistant { item } => {
            let encoded = serde_json::to_vec(&item.content).map_err(|_| {
                SurfaceValidationError::new("incomplete assistant content is not encodable")
            })?;
            bound_count(
                encoded.len(),
                MAX_PRESENTATION_CONTENT_BYTES,
                "incomplete assistant content",
            )
        }
        SessionPresentationItem::AgentAction {
            capability_id,
            summary,
            ..
        } => {
            bound_optional_string(
                capability_id.as_deref(),
                MAX_IDENTIFIER_BYTES,
                "capability ID",
            )?;
            bound_string(summary, MAX_DISPLAY_BYTES, "agent action summary", false)
        }
        SessionPresentationItem::ContextBoundary { reason, .. } => {
            bound_string(reason, MAX_DISPLAY_BYTES, "context boundary reason", false)
        }
        SessionPresentationItem::Notice { code, message, .. } => {
            bound_string(code, MAX_IDENTIFIER_BYTES, "notice code", false)?;
            bound_string(message, MAX_DISPLAY_BYTES, "notice message", false)
        }
    };
    result?;
    let encoded = serde_json::to_vec(item)
        .map_err(|_| SurfaceValidationError::new("presentation item is not encodable"))?;
    bound_count(
        encoded.len(),
        MAX_PRESENTATION_CONTENT_BYTES,
        "presentation item content",
    )
}

fn validate_execution(execution: &ExecutionView) -> Result<(), SurfaceValidationError> {
    execution.cwd.validate()?;
    if execution.state.is_live() && execution.exit.is_some() {
        return Err(SurfaceValidationError::new(
            "a live execution cannot carry an exit outcome",
        ));
    }
    validate_exit(execution.exit.as_ref())
}

fn validate_terminals(terminals: &[TerminalSessionView]) -> Result<(), SurfaceValidationError> {
    bound_count(terminals.len(), MAX_TERMINAL_RECORDS, "terminal records")?;
    for terminal in terminals {
        terminal.validate()?;
    }
    Ok(())
}

fn validate_shell_profile(shell: &ShellProfileView) -> Result<(), SurfaceValidationError> {
    bound_string(
        &shell.profile_id,
        MAX_IDENTIFIER_BYTES,
        "shell profile ID",
        false,
    )?;
    shell.program.validate()?;
    bound_string(
        &shell.executable_digest,
        MAX_DIGEST_BYTES,
        "shell executable digest",
        false,
    )?;
    validate_ascii_graphic(&shell.executable_digest, "shell executable digest")?;
    bound_string(
        &shell.config_digest,
        MAX_DIGEST_BYTES,
        "shell config digest",
        false,
    )?;
    validate_ascii_graphic(&shell.config_digest, "shell config digest")
}

fn validate_exit(exit: Option<&ExecutionExit>) -> Result<(), SurfaceValidationError> {
    if let Some(ExecutionExit::Error { code }) = exit {
        bound_string(
            code,
            MAX_IDENTIFIER_BYTES,
            "execution exit error code",
            false,
        )?;
    }
    Ok(())
}

fn validate_identifier_list(
    values: &[String],
    maximum: usize,
    list_name: &'static str,
    value_name: &'static str,
) -> Result<(), SurfaceValidationError> {
    bound_count(values.len(), maximum, list_name)?;
    let mut unique = BTreeSet::new();
    for value in values {
        bound_string(value, MAX_IDENTIFIER_BYTES, value_name, false)?;
        if !unique.insert(value) {
            return Err(SurfaceValidationError::new(format!(
                "{list_name} must contain unique values"
            )));
        }
    }
    Ok(())
}

fn is_forbidden_human_command_character(character: char) -> bool {
    let code = character as u32;
    (code <= 0x1f && character != '\n' && character != '\t') || (0x7f..=0x9f).contains(&code)
}

fn is_forbidden_presentation_character(character: char) -> bool {
    let code = character as u32;
    is_forbidden_human_command_character(character) || is_unicode_format_control(code)
}

fn is_unicode_format_control(code: u32) -> bool {
    matches!(
        code,
        0x00ad
            | 0x061c
            | 0x06dd
            | 0x070f
            | 0x180e
            | 0xfeff
            | 0x110bd
            | 0x110cd
            | 0xe0001
            | 0x0600..=0x0605
            | 0x0890..=0x0891
            | 0x08e2
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x206f
            | 0xfff9..=0xfffb
            | 0x13430..=0x1343f
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0020..=0xe007f
    )
}

fn validate_environment_name(name: &str) -> Result<(), SurfaceValidationError> {
    bound_string(name, MAX_ENVIRONMENT_NAME_BYTES, "environment name", false)?;
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(SurfaceValidationError::new(
            "environment name must not be empty",
        ));
    };
    if !(first == b'_' || first.is_ascii_alphabetic())
        || bytes.any(|byte| !(byte == b'_' || byte.is_ascii_alphanumeric()))
    {
        return Err(SurfaceValidationError::new(
            "environment name has invalid syntax",
        ));
    }
    Ok(())
}

fn validate_command_name(name: &str) -> Result<(), SurfaceValidationError> {
    if name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Ok(())
    } else {
        Err(SurfaceValidationError::new(
            "command names must use lowercase ASCII letters, digits or hyphens",
        ))
    }
}

fn validate_command_id(id: &str) -> Result<(), SurfaceValidationError> {
    if id.split('.').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) {
        Ok(())
    } else {
        Err(SurfaceValidationError::new(
            "command IDs must be lowercase dotted identifiers",
        ))
    }
}

fn validate_overlay_environment_name(name: &str) -> Result<(), SurfaceValidationError> {
    validate_environment_name(name)?;
    const RESERVED: &[&str] = &[
        "PATH", "PWD", "OLDPWD", "HOME", "SHELL", "ENV", "BASH_ENV", "ZDOTDIR",
    ];
    if RESERVED.contains(&name)
        || name.starts_with("AGL_INTERNAL_")
        || name.starts_with("AGL_SHELL_INTEGRATION_")
    {
        return Err(SurfaceValidationError::new(
            "environment name is owned by terminal admission",
        ));
    }
    Ok(())
}

fn validate_single_line(value: &str, field: &'static str) -> Result<(), SurfaceValidationError> {
    if value.contains(['\0', '\n', '\r']) {
        Err(SurfaceValidationError::new(format!(
            "{field} must be single-line text"
        )))
    } else {
        Ok(())
    }
}

fn validate_ascii_graphic(value: &str, field: &'static str) -> Result<(), SurfaceValidationError> {
    if value.bytes().all(|byte| byte.is_ascii_graphic()) {
        Ok(())
    } else {
        Err(SurfaceValidationError::new(format!(
            "{field} must be ASCII graphic text"
        )))
    }
}

fn ensure_unique_copy<T: Copy + Ord>(
    values: &[T],
    field: &'static str,
) -> Result<(), SurfaceValidationError> {
    let mut unique = BTreeSet::new();
    if values.iter().copied().all(|value| unique.insert(value)) {
        Ok(())
    } else {
        Err(SurfaceValidationError::new(format!(
            "{field} must contain unique values"
        )))
    }
}

fn bound_optional_string(
    value: Option<&str>,
    maximum: usize,
    field: &'static str,
) -> Result<(), SurfaceValidationError> {
    match value {
        Some(value) => bound_string(value, maximum, field, false),
        None => Ok(()),
    }
}

fn bound_string(
    value: &str,
    maximum: usize,
    field: &'static str,
    allow_empty: bool,
) -> Result<(), SurfaceValidationError> {
    if !allow_empty && value.is_empty() {
        return Err(SurfaceValidationError::new(format!(
            "{field} must not be empty"
        )));
    }
    bound_count(value.len(), maximum, field)
}

fn bound_count(
    actual: usize,
    maximum: usize,
    field: &'static str,
) -> Result<(), SurfaceValidationError> {
    if actual > maximum {
        Err(SurfaceValidationError::new(format!(
            "{field} exceeds its bound of {maximum}"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_safe_metadata(
    metadata: &BTreeMap<String, String>,
) -> Result<(), SurfaceValidationError> {
    bound_count(
        metadata.len(),
        MAX_SAFE_METADATA_ENTRIES,
        "safe metadata entries",
    )?;
    for (key, value) in metadata {
        bound_string(key, MAX_IDENTIFIER_BYTES, "safe metadata key", false)?;
        bound_string(value, MAX_DISPLAY_BYTES, "safe metadata value", true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DaemonEvent, DaemonRequest, EVENT_SCHEMA, REQUEST_SCHEMA};

    const REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000001";
    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000002";
    const EXECUTION_ID: &str = "exec_01890f17-4a00-7000-8000-000000000003";
    const TERMINAL_ID: &str = "term_01890f17-4a00-7000-8000-000000000004";
    const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000005";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000006";
    const MESSAGE_ID: &str = "msg_01890f17-4a00-7000-8000-000000000007";
    const EVENT_ID: &str = "evt_01890f17-4a00-7000-8000-000000000008";

    fn request_id() -> RequestId {
        RequestId::parse(REQUEST_ID).unwrap()
    }

    fn session_id() -> SessionId {
        SessionId::parse(SESSION_ID).unwrap()
    }

    fn display_path(text: &str) -> SanitizedDisplayPath {
        SanitizedDisplayPath {
            text: text.to_owned(),
            truncated: false,
        }
    }

    fn workspace_history_scope() -> String {
        format!("sha256:{}", "a".repeat(64))
    }

    fn terminal() -> TerminalSessionView {
        TerminalSessionView {
            terminal_id: TerminalSessionId::parse(TERMINAL_ID).unwrap(),
            execution_id: ExecutionId::parse(EXECUTION_ID).unwrap(),
            owner: TerminalOwnerView::Human {
                session_id: session_id(),
            },
            profile: ExecutionProfile::Workspace,
            shell: ShellProfileView {
                profile_id: "bash-managed".to_owned(),
                program: display_path("/bin/bash"),
                executable_digest: "sha256:aaaaaaaa".to_owned(),
                config_digest: "sha256:bbbbbbbb".to_owned(),
            },
            workspace_root: display_path("/workspace"),
            cwd: display_path("/workspace"),
            initial_environment_digest: "sha256:cccccccc".to_owned(),
            environment_names: vec!["LANG".to_owned(), "PATH".to_owned()],
            command_sequence: 0,
            prompt_generation: Some(1),
            prompt_state: TerminalPromptState::Ready,
            process_state: ExecutionState::Running,
            exit: None,
            writer: TerminalWriterView::Owner,
            promoted: false,
        }
    }

    fn presentation_snapshot(text_bytes: usize) -> SessionPresentationSnapshot {
        let session_id = session_id();
        SessionPresentationSnapshot {
            session_id: session_id.clone(),
            cursor: PresentationCursor {
                daemon_instance_id: DaemonInstanceId::generate(),
                revision: 17,
            },
            older_page_cursor: Some("older-page-1".to_owned()),
            header: SessionHeader {
                session_id: session_id.clone(),
                status: SessionPresentationStatus::Active,
                durable: true,
                resumed: false,
                title: None,
                function_name: "agentLIBRE".to_owned(),
                model_id: None,
                operation_mode: ProtocolToolMode::ReadOnly,
                selected_skills: Vec::new(),
                runtime_context_revision: 1,
                workspace_root: display_path("/workspace"),
                workspace_history_scope: workspace_history_scope(),
                cwd: display_path("/workspace"),
                execution_context_revision: 1,
                context_used_tokens: None,
                context_limit_tokens: None,
                active_run_count: 0,
                queued_prompt_count: 0,
                active_execution_count: 0,
            },
            items: vec![SessionPresentationItem::UserMessage {
                message_id: MessageId::parse(MESSAGE_ID).unwrap(),
                content: Content::text("x".repeat(text_bytes)).unwrap(),
            }],
            active_run: None,
            queued_prompts: Vec::new(),
            terminals: Vec::new(),
            executions: Vec::new(),
            human_commands: Vec::new(),
            activity: None,
            command_context: CommandContext {
                session_id: Some(session_id),
                session_active: true,
                active_or_queued_turns: 0,
                active_executions: 0,
                host_shell_available: true,
                operation_mode: ProtocolToolMode::ReadOnly,
            },
        }
    }

    fn ensure_request() -> HumanTerminalEnsureRequest {
        HumanTerminalEnsureRequest {
            session_id: session_id(),
            client_submission_id: "terminal-create-1".to_owned(),
            execution_context_revision: 7,
            profile: ExecutionProfile::Workspace,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: TerminalSize {
                columns: 120,
                rows: 40,
            },
            agl_env: StructuredEnvironmentOverlay {
                values: BTreeMap::from([(
                    "AGL_THEME".to_owned(),
                    "private-non-secret-value".to_owned(),
                )]),
                inherited_names: vec!["LANG".to_owned()],
                secret_refs: vec![SecretEnvironmentReference {
                    name: "API_TOKEN".to_owned(),
                    reference_id: "secret-ref-private".to_owned(),
                }],
            },
            host_startup: HostStartupPolicy::ManagedOnly,
        }
    }

    fn host_ensure_request() -> HumanHostTerminalEnsureRequest {
        let mut terminal = ensure_request();
        terminal.profile = ExecutionProfile::Host;
        HumanHostTerminalEnsureRequest {
            terminal,
            confirm_host_authority: true,
        }
    }

    #[test]
    fn human_terminal_ensure_has_strict_v6_wire_shape_and_redacted_debug() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalEnsure(ensure_request()),
        );
        let value = serde_json::to_value(&request).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "schema": REQUEST_SCHEMA,
                "request_id": REQUEST_ID,
                "kind": "human_terminal_ensure",
                "payload": {
                    "session_id": SESSION_ID,
                    "client_submission_id": "terminal-create-1",
                    "execution_context_revision": 7,
                    "profile": "workspace",
                    "shell_profile_id": "bash-managed",
                    "terminal_size": { "columns": 120, "rows": 40 },
                    "agl_env": {
                        "values": { "AGL_THEME": "private-non-secret-value" },
                        "inherited_names": ["LANG"],
                        "secret_refs": [{
                            "name": "API_TOKEN",
                            "reference_id": "secret-ref-private"
                        }]
                    },
                    "host_startup": "managed_only"
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<DaemonRequest>(value.clone()).unwrap(),
            request
        );

        let debug = format!("{request:?}");
        assert!(!debug.contains("private-non-secret-value"));
        assert!(!debug.contains("secret-ref-private"));
        assert!(debug.contains("API_TOKEN"));
    }

    #[test]
    fn human_host_terminal_admission_is_explicit_strict_and_non_secret() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanHostTerminalEnsure(host_ensure_request()),
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["kind"], "human_host_terminal_ensure");
        assert_eq!(value["payload"]["terminal"]["profile"], "host");
        assert_eq!(value["payload"]["confirm_host_authority"], true);
        assert!(value["payload"].get("grant_id").is_none());
        assert_eq!(
            serde_json::from_value::<DaemonRequest>(value).unwrap(),
            request
        );

        let debug = format!("{request:?}");
        assert!(!debug.contains("private-non-secret-value"));
        assert!(!debug.contains("secret-ref-private"));
    }

    #[test]
    fn human_host_terminal_rejects_missing_confirmation_workspace_and_unknown_authority() {
        let mut unconfirmed = host_ensure_request();
        unconfirmed.confirm_host_authority = false;
        assert!(
            DaemonRequest::new(
                request_id(),
                DaemonRequestKind::HumanHostTerminalEnsure(unconfirmed)
            )
            .validate()
            .is_err()
        );

        let mut workspace = host_ensure_request();
        workspace.terminal.profile = ExecutionProfile::Workspace;
        assert!(
            DaemonRequest::new(
                request_id(),
                DaemonRequestKind::HumanHostTerminalEnsure(workspace)
            )
            .validate()
            .is_err()
        );

        let mut unknown = serde_json::to_value(DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanHostTerminalEnsure(host_ensure_request()),
        ))
        .unwrap();
        unknown["payload"]["authority_token"] = serde_json::json!("must-not-exist");
        assert!(serde_json::from_value::<DaemonRequest>(unknown).is_err());
    }

    #[test]
    fn terminal_ensure_rejects_legacy_shape_reserved_env_and_invalid_host_startup() {
        let legacy = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "user_shell_start",
            "payload": {
                "session_id": SESSION_ID,
                "command": "pwd"
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(legacy).is_err());

        let mut reserved = serde_json::to_value(DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalEnsure(ensure_request()),
        ))
        .unwrap();
        reserved["payload"]["agl_env"]["values"] = serde_json::json!({ "PATH": "/untrusted" });
        assert!(serde_json::from_value::<DaemonRequest>(reserved).is_err());

        let mut source_rc = ensure_request();
        source_rc.host_startup = HostStartupPolicy::SourceUserRc;
        let source_rc = serde_json::to_value(DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalEnsure(source_rc),
        ))
        .unwrap();
        assert!(serde_json::from_value::<DaemonRequest>(source_rc).is_err());
    }

    #[test]
    fn terminal_ensure_rejects_unknown_fields_and_environment_bounds() {
        let mut unknown = serde_json::to_value(DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalEnsure(ensure_request()),
        ))
        .unwrap();
        unknown["payload"]["history_policy"] = serde_json::json!("global");
        assert!(serde_json::from_value::<DaemonRequest>(unknown).is_err());

        let mut too_many = ensure_request();
        too_many.agl_env.values = (0..=MAX_ENVIRONMENT_NAMES)
            .map(|index| (format!("VALUE_{index}"), "x".to_owned()))
            .collect();
        let too_many = serde_json::to_value(DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalEnsure(too_many),
        ))
        .unwrap();
        assert!(serde_json::from_value::<DaemonRequest>(too_many).is_err());

        let mut too_large = ensure_request();
        too_large.agl_env.values.insert(
            "LARGE".to_owned(),
            "x".repeat(MAX_ENVIRONMENT_VALUE_BYTES + 1),
        );
        let too_large = serde_json::to_value(DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalEnsure(too_large),
        ))
        .unwrap();
        assert!(serde_json::from_value::<DaemonRequest>(too_large).is_err());
    }

    #[test]
    fn ensured_terminal_event_contains_metadata_but_no_pty_or_environment_values() {
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::HumanTerminalEnsured(HumanTerminalEnsuredEvent {
                terminal: terminal(),
                disposition: TerminalEnsureDisposition::Created,
            }),
        );
        let value = serde_json::to_value(&event).unwrap();
        let encoded = serde_json::to_string(&value).unwrap();

        assert_eq!(value["schema"], EVENT_SCHEMA);
        assert_eq!(value["kind"], "human_terminal_ensured");
        assert_eq!(value["payload"]["terminal"]["terminal_id"], TERMINAL_ID);
        assert_eq!(value["payload"]["terminal"]["execution_id"], EXECUTION_ID);
        for forbidden in [
            "output",
            "bytes",
            "command_text",
            "secret_refs",
            "reference_id",
        ] {
            assert!(!encoded.contains(forbidden), "leaked field {forbidden}");
        }
        assert_eq!(serde_json::from_value::<DaemonEvent>(value).unwrap(), event);
    }

    #[test]
    fn presentation_rejects_oversized_assistant_delta() {
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::SessionPresentationEvent(Box::new(SessionPresentationEventEnvelope {
                event_id: EventId::parse(EVENT_ID).unwrap(),
                session_id: session_id(),
                cursor: PresentationCursor {
                    daemon_instance_id: DaemonInstanceId::generate(),
                    revision: 9,
                },
                event: SessionPresentationEventPayload::AssistantTextDelta {
                    run_id: RunId::parse(RUN_ID).unwrap(),
                    turn_id: TurnId::parse(TURN_ID).unwrap(),
                    provisional_message_id: MessageId::parse(MESSAGE_ID).unwrap(),
                    sequence: 1,
                    text: "x".repeat(MAX_ASSISTANT_DELTA_BYTES + 1),
                },
            })),
        );
        let value = serde_json::to_value(event).unwrap();
        assert!(serde_json::from_value::<DaemonEvent>(value).is_err());
    }

    #[test]
    fn incomplete_output_action_and_item_have_strict_stable_wire_shapes() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::ApplicationAction(ApplicationActionRequest {
                session_id: Some(session_id()),
                client_submission_id: "continue-stable-1".to_owned(),
                action: ApplicationAction::IncompleteTurnContinue {
                    message_id: MessageId::parse(MESSAGE_ID).unwrap(),
                    expected_execution_context_revision: 41,
                },
            }),
        );
        let request_wire = serde_json::to_value(&request).unwrap();
        assert_eq!(request_wire["kind"], "application_action");
        assert_eq!(
            request_wire["payload"],
            serde_json::json!({
                "session_id": SESSION_ID,
                "client_submission_id": "continue-stable-1",
                "action": {
                    "kind": "incomplete_turn_continue",
                    "message_id": MESSAGE_ID,
                    "expected_execution_context_revision": 41,
                },
            })
        );
        assert_eq!(
            serde_json::from_value::<DaemonRequest>(request_wire.clone()).unwrap(),
            request
        );
        let mut unknown = request_wire;
        unknown["payload"]["action"]["retry"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DaemonRequest>(unknown).is_err());

        let item = SessionPresentationItem::IncompleteAssistant {
            item: IncompleteAssistantItemView {
                message_id: MessageId::parse(MESSAGE_ID).unwrap(),
                content: Content::text("bounded partial").unwrap(),
                source_run_id: RunId::parse(RUN_ID).unwrap(),
                source_turn_id: TurnId::parse(TURN_ID).unwrap(),
                source_attempt_id: AttemptId::generate(),
                reason: IncompleteOutputReason::ContentByteLimit,
                continuation_index: 2,
                continue_action: ContinueActionView::Available,
            },
        };
        let item_wire = serde_json::to_value(&item).unwrap();
        assert_eq!(item_wire["kind"], "incomplete_assistant");
        assert_eq!(item_wire["item"]["reason"], "content_byte_limit");
        assert_eq!(item_wire["item"]["continuation_index"], 2);
        assert_eq!(item_wire["item"]["continue_action"]["state"], "available");
        assert_eq!(
            serde_json::from_value::<SessionPresentationItem>(item_wire).unwrap(),
            item
        );
    }

    #[test]
    fn command_catalog_rejects_descriptor_count_overflow() {
        let descriptor = CommandDescriptor {
            id: "session.status".to_owned(),
            name: "status".to_owned(),
            aliases: Vec::new(),
            summary: "Show status".to_owned(),
            category: CommandCategory::Session,
            arguments: Vec::new(),
            action_kind: ApplicationActionKind::SessionStatus,
            concurrency: CommandConcurrency::ReadOnly,
            availability: CommandAvailability::Enabled,
        };
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::CommandCatalog(CommandCatalogEvent {
                descriptors: vec![descriptor; MAX_COMMAND_DESCRIPTORS + 1],
            }),
        );
        let value = serde_json::to_value(event).unwrap();
        assert!(serde_json::from_value::<DaemonEvent>(value).is_err());
    }

    #[test]
    fn inference_inventory_is_bounded_and_rejects_impossible_memory() {
        let device = crate::InferenceDeviceEvent {
            physical_device_id: "pci:0000:03:00.0".to_owned(),
            driver_build_id: "sha256:driver".to_owned(),
            backend_name: "Vulkan0".to_owned(),
            description: "RX 7900 XTX".to_owned(),
            kind: crate::ProtocolInferenceDeviceKind::DiscreteGpu,
            free_memory_bytes: 20,
            total_memory_bytes: 24,
            usable: true,
            supports_gpu_offload: true,
        };
        let event = DaemonEvent::new(
            Some(request_id()),
            crate::DaemonEventKind::InferenceInventory(crate::InferenceInventoryEvent {
                devices: vec![device.clone()],
            }),
        );
        event.validate().unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonEvent>(&encoded).unwrap(),
            event
        );

        let impossible = DaemonEvent::new(
            Some(request_id()),
            crate::DaemonEventKind::InferenceInventory(crate::InferenceInventoryEvent {
                devices: vec![crate::InferenceDeviceEvent {
                    free_memory_bytes: 25,
                    ..device
                }],
            }),
        );
        assert!(impossible.validate().is_err());
    }

    #[test]
    fn inference_status_is_bounded_and_requires_complete_worker_identity() {
        let status = crate::InferenceStatusEvent {
            worker_build_id: "sha256:worker".to_owned(),
            worker_state: crate::ProtocolInferenceWorkerState::Busy,
            worker_pid: Some(42),
            launch_generation: Some(7),
            physical_device_id: Some("pci:0000:03:00.0".to_owned()),
            reserved_bytes: 16 * 1024 * 1024,
            cooldown_not_before_unix_ms: None,
        };
        let event = DaemonEvent::new(
            Some(request_id()),
            crate::DaemonEventKind::InferenceStatus(status.clone()),
        );
        event.validate().unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonEvent>(&encoded).unwrap(),
            event
        );

        let incomplete_identity = DaemonEvent::new(
            Some(request_id()),
            crate::DaemonEventKind::InferenceStatus(crate::InferenceStatusEvent {
                launch_generation: None,
                ..status
            }),
        );
        assert!(incomplete_identity.validate().is_err());
    }

    #[test]
    fn snapshot_transfer_splits_one_typed_page_into_wire_safe_frames() {
        let snapshot = presentation_snapshot(900_000);
        let transfer = SessionPresentationSnapshotTransfer::encode(
            request_id(),
            SessionPresentationSnapshotTransferPurpose::Requested,
            &snapshot,
        )
        .unwrap();

        assert_eq!(transfer.manifest.item_count, 1);
        assert_eq!(
            usize::from(transfer.manifest.chunk_count),
            transfer.chunks.len()
        );
        assert!(transfer.chunks.len() >= 2);
        assert_eq!(transfer.finished.digest, transfer.manifest.digest);
        assert_eq!(
            transfer.manifest.decoded_bytes,
            u64::try_from(snapshot.canonical_json_bytes().unwrap().len()).unwrap()
        );

        let kinds = std::iter::once(DaemonEventKind::SessionPresentationSnapshotManifest(
            transfer.manifest.clone(),
        ))
        .chain(
            transfer
                .chunks
                .iter()
                .cloned()
                .map(DaemonEventKind::SessionPresentationSnapshotChunk),
        )
        .chain(std::iter::once(
            DaemonEventKind::SessionPresentationSnapshotFinished(transfer.finished.clone()),
        ));
        for kind in kinds {
            let event = DaemonEvent::new(Some(RequestId::generate()), kind);
            event.validate().unwrap();
            assert!(serde_json::to_vec(&event).unwrap().len() <= MAX_JSONL_FRAME_BYTES);
        }

        let decoded = transfer
            .chunks
            .iter()
            .flat_map(|chunk| {
                chunk
                    .bytes
                    .decode(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(decoded, snapshot.canonical_json_bytes().unwrap());
        assert_eq!(
            PresentationSnapshotDigest::from_bytes(&decoded),
            transfer.manifest.digest
        );
    }

    #[test]
    fn snapshot_transfer_rejects_noncanonical_counts_chunks_and_digests() {
        let snapshot = presentation_snapshot(900_000);
        let transfer = SessionPresentationSnapshotTransfer::encode(
            request_id(),
            SessionPresentationSnapshotTransferPurpose::SubscriptionInitial,
            &snapshot,
        )
        .unwrap();

        let mut bad_count = transfer.manifest.clone();
        bad_count.chunk_count = bad_count.chunk_count.saturating_add(1);
        assert!(bad_count.validate().is_err());

        let mut bad_index = transfer.chunks[0].clone();
        bad_index.chunk_index = bad_index.chunk_count;
        assert!(bad_index.validate().is_err());

        let mut bad_encoding = transfer.chunks[0].clone();
        bad_encoding.bytes = crate::ProcessBytes::from_bytes(b"not-binary");
        assert!(bad_encoding.validate().is_err());

        assert!(PresentationSnapshotDigest::parse("sha256:ABCDEF").is_err());
        assert!(PresentationSnapshotDigest::parse("md5:00").is_err());

        let mut oversized = transfer.manifest;
        oversized.decoded_bytes = u64::try_from(MAX_PRESENTATION_CONTENT_BYTES + 1).unwrap();
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn presentation_page_cursors_are_bounded_and_strict() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::SessionPresentation(SessionPresentationRequest {
                session_id: session_id(),
                page_cursor: Some("x".repeat(MAX_IDENTIFIER_BYTES + 1)),
            }),
        );
        assert!(request.validate().is_err());

        let mut snapshot = presentation_snapshot(1);
        snapshot.older_page_cursor = Some("x".repeat(MAX_IDENTIFIER_BYTES + 1));
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn activity_graph_requires_safe_connected_consistent_topology() {
        let run_id = RunId::parse(RUN_ID).unwrap();
        let turn_id = TurnId::parse(TURN_ID).unwrap();
        let root_id = format!("run:{run_id}");
        let turn_node_id = format!("turn:{turn_id}");
        let root = ActivityNodeView {
            node_id: root_id.clone(),
            parent_node_id: None,
            order_index: 1,
            run_id: run_id.clone(),
            turn_id: None,
            attempt_id: None,
            step_id: None,
            kind: ActivityNodeKind::Run,
            phase: ActivityPhase::Queued,
            state: ActivityNodeState::Running,
            retry: 0,
            started_at_unix_ms: 1,
            updated_at_unix_ms: 2,
            finished_at_unix_ms: None,
            elapsed_ms: 0,
            summary: "run".to_owned(),
            detail: ActivityDetailView::None,
        };
        let turn = ActivityNodeView {
            node_id: turn_node_id.clone(),
            parent_node_id: Some(root_id.clone()),
            order_index: 2,
            run_id: run_id.clone(),
            turn_id: Some(turn_id),
            attempt_id: None,
            step_id: None,
            kind: ActivityNodeKind::Turn,
            phase: ActivityPhase::Model,
            state: ActivityNodeState::Running,
            retry: 0,
            started_at_unix_ms: 2,
            updated_at_unix_ms: 3,
            finished_at_unix_ms: None,
            elapsed_ms: 0,
            summary: "turn".to_owned(),
            detail: ActivityDetailView::Capability(CapabilityActivityDetail::FilesystemList {
                path: display_path("src/lib.rs"),
                entries: 1,
                completeness: ActivityCompleteness::Complete,
            }),
        };
        let graph = ActivityGraphView {
            graph_revision: 1,
            roots: vec![root_id.clone()],
            nodes: vec![root, turn],
            current_path: vec![root_id, turn_node_id],
            truncated: false,
        };
        graph.validate().unwrap();

        let mut missing_parent = graph.clone();
        missing_parent.nodes[1].parent_node_id = Some("missing".to_owned());
        assert!(missing_parent.validate().is_err());

        let mut cycle = graph.clone();
        let attempt_id = AttemptId::generate();
        let attempt_node_id = format!("attempt:{attempt_id}");
        cycle.nodes[1].parent_node_id = Some(attempt_node_id.clone());
        cycle.nodes.push(ActivityNodeView {
            node_id: attempt_node_id,
            parent_node_id: Some(cycle.nodes[1].node_id.clone()),
            order_index: 3,
            run_id: run_id.clone(),
            turn_id: cycle.nodes[1].turn_id.clone(),
            attempt_id: Some(attempt_id),
            step_id: None,
            kind: ActivityNodeKind::Attempt,
            phase: ActivityPhase::Model,
            state: ActivityNodeState::Running,
            retry: 0,
            started_at_unix_ms: 3,
            updated_at_unix_ms: 3,
            finished_at_unix_ms: None,
            elapsed_ms: 0,
            summary: "attempt".to_owned(),
            detail: ActivityDetailView::None,
        });
        cycle.current_path.clear();
        assert!(cycle.validate().is_err());

        let mut bad_path = graph.clone();
        bad_path.current_path.reverse();
        assert!(bad_path.validate().is_err());

        for path in ["/home/user/private", "src/../secret", "./src"] {
            let mut unsafe_path = graph.clone();
            unsafe_path.nodes[1].detail =
                ActivityDetailView::Capability(CapabilityActivityDetail::FilesystemList {
                    path: display_path(path),
                    entries: 1,
                    completeness: ActivityCompleteness::Complete,
                });
            assert!(
                unsafe_path.validate().is_err(),
                "accepted unsafe path {path}"
            );
        }

        let mut unsafe_summary = graph.clone();
        unsafe_summary.nodes[1].summary = "reading /home/user/private".to_owned();
        assert!(unsafe_summary.validate().is_err());

        let mut impossible_progress = graph.clone();
        impossible_progress.nodes[1].detail =
            ActivityDetailView::Inference(InferenceActivityDetail {
                stage: InferenceProductStageView::Prefill,
                completed: Some(3),
                total: Some(2),
                unit: Some(InferenceProgressUnit::Tokens),
                cache: ActivityCacheDisposition::Rebuilt,
            });
        assert!(impossible_progress.validate().is_err());

        let mut noncanonical = graph.clone();
        noncanonical.nodes.reverse();
        assert!(noncanonical.validate().is_err());

        let encoded = serde_json::to_string(&graph).unwrap();
        for sentinel in ["super-secret-token", "raw prompt", "/home/user/private"] {
            assert!(!encoded.contains(sentinel));
        }
        assert!(
            serde_json::from_value::<ActivityDetailView>(serde_json::json!({
                "kind": "unknown_capability",
                "detail": {
                    "capability_id": "fs.list",
                    "raw_arguments": "super-secret-token"
                }
            }))
            .is_err(),
            "closed activity detail must reject unreviewed raw fields"
        );
        let mut unsafe_capability = graph.clone();
        unsafe_capability.nodes[1].detail = ActivityDetailView::UnknownCapability {
            capability_id: "/home/user/private".to_owned(),
        };
        assert!(unsafe_capability.validate().is_err());

        let delta = ActivityGraphDeltaBatch {
            graph_revision: 2,
            upserts: graph.nodes.clone(),
            removals: Vec::new(),
            current_path: Some(graph.current_path.clone()),
            truncated: false,
        };
        delta.validate_shape().unwrap();
        let mut child_first = delta.clone();
        child_first.upserts.reverse();
        assert!(child_first.validate_shape().is_err());

        let mut terminal_without_finish = graph;
        terminal_without_finish.nodes[1].state = ActivityNodeState::Succeeded;
        assert!(terminal_without_finish.validate().is_err());
    }

    #[test]
    fn display_path_and_workspace_scope_wire_values_fail_closed() {
        for hostile in ["line\nbreak", "escape\u{1b}", "bidi\u{202e}", "c1\u{85}"] {
            assert!(
                SanitizedDisplayPath {
                    text: hostile.to_owned(),
                    truncated: false,
                }
                .validate()
                .is_err()
            );
        }
        display_path("/workspace").validate().unwrap();
        validate_workspace_history_scope(&workspace_history_scope()).unwrap();
        for invalid in [
            "",
            "sha256:abc",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "md5:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_workspace_history_scope(invalid).is_err());
        }
    }

    #[test]
    fn human_command_card_wire_lifecycle_is_typed_and_debug_is_redacted() {
        let value = serde_json::json!({
            "terminal_id": TERMINAL_ID,
            "execution_id": EXECUTION_ID,
            "command_sequence": 4,
            "command": "printf private-value",
            "output": "done\n",
            "output_start": { "after_sequence": 10 },
            "output_end": { "after_sequence": 12 },
            "state": "exited",
            "exit_status": 0,
            "cwd": { "text": "/workspace", "truncated": false },
            "truncated": false,
            "filtered_effects": 0,
            "started_at_unix_ms": 20,
            "updated_at_unix_ms": 21
        });
        let card = serde_json::from_value::<HumanCommandCardView>(value.clone()).unwrap();
        card.validate().unwrap();
        assert!(!format!("{card:?}").contains("private-value"));

        let mut missing_exit = value.clone();
        missing_exit["exit_status"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<HumanCommandCardView>(missing_exit)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut raw_escape = value;
        raw_escape["command"] = serde_json::json!("printf \u{1b}[31m");
        assert!(
            serde_json::from_value::<HumanCommandCardView>(raw_escape)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn human_command_submit_uses_writer_authority_and_redacts_debug() {
        const SENTINEL: &str = "AGL_PROTOCOL_PRIVATE_COMMAND_148";
        let request = HumanTerminalCommandSubmitRequest {
            session_id: session_id(),
            terminal_id: TerminalSessionId::parse(TERMINAL_ID).unwrap(),
            client_submission_id: "typed-command".to_owned(),
            writer_lease_id: WriterLeaseId::generate(),
            expected_command_sequence: 2,
            expected_prompt_generation: 3,
            command: SENTINEL.to_owned(),
        };
        request.validate().unwrap();
        let request_debug = format!("{request:?}");
        assert!(!request_debug.contains(SENTINEL));
        assert!(!request_debug.contains(request.writer_lease_id.as_str()));
        let envelope = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::HumanTerminalCommandSubmit(request.clone()),
        );
        let envelope_debug = format!("{envelope:?}");
        assert!(!envelope_debug.contains(SENTINEL));
        assert!(!envelope_debug.contains(request.writer_lease_id.as_str()));
        let encoded = serde_json::to_value(envelope).unwrap();
        assert_eq!(
            encoded["payload"]["writer_lease_id"],
            request.writer_lease_id.as_str()
        );
        assert!(encoded["payload"].get("attachment_id").is_none());
    }
}
