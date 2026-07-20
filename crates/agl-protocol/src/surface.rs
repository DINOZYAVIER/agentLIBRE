use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Display, Formatter, Write as _};

use agl_content::Content;
use agl_ids::{
    DaemonInstanceId, EventId, ExecutionId, MessageId, RequestId, RunId, SessionId, StepId,
    TerminalSessionId, TurnId,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    DaemonEventKind, DaemonRequestKind, ExecutionExit, ExecutionProfile, ExecutionState, KillMode,
    ProtocolToolMode, TerminalSize,
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
    pub cwd: String,
    pub exit: Option<ExecutionExit>,
    pub last_sequence: u64,
    pub output_truncated: bool,
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
    pub program: String,
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
    pub workspace_root: String,
    pub cwd: String,
    pub initial_environment_digest: String,
    pub environment_names: Vec<String>,
    pub command_sequence: u64,
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
        bound_string(
            &self.workspace_root,
            MAX_PATH_BYTES,
            "terminal workspace root",
            false,
        )?;
        bound_string(&self.cwd, MAX_PATH_BYTES, "terminal cwd", false)?;
        if self.workspace_root.contains('\0') || self.cwd.contains('\0') {
            return Err(SurfaceValidationError::new(
                "terminal paths must not contain NUL",
            ));
        }
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
        SessionPresentationEventPayload::TerminalCommandFinished { cwd, .. } => {
            bound_string(cwd, MAX_PATH_BYTES, "terminal cwd", false)
        }
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
    if header.workspace_root.contains('\0') || header.cwd.contains('\0') {
        return Err(SurfaceValidationError::new(
            "presentation paths must not contain NUL",
        ));
    }
    bound_optional_string(header.model_id.as_deref(), MAX_IDENTIFIER_BYTES, "model ID")?;
    validate_identifier_list(
        &header.selected_skills,
        MAX_SKILLS,
        "selected skills",
        "skill ID",
    )?;
    bound_string(
        &header.workspace_root,
        MAX_PATH_BYTES,
        "workspace root",
        false,
    )?;
    bound_string(&header.cwd, MAX_PATH_BYTES, "working directory", false)
}

fn validate_item(item: &SessionPresentationItem) -> Result<(), SurfaceValidationError> {
    let result = match item {
        SessionPresentationItem::UserMessage { .. }
        | SessionPresentationItem::AssistantMessage { .. } => Ok(()),
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
    bound_string(&execution.cwd, MAX_PATH_BYTES, "execution cwd", false)?;
    if execution.cwd.contains('\0') {
        return Err(SurfaceValidationError::new(
            "execution cwd must not contain NUL",
        ));
    }
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
    bound_string(&shell.program, MAX_PATH_BYTES, "shell program", false)?;
    if shell.program.contains('\0') {
        return Err(SurfaceValidationError::new(
            "shell program must not contain NUL",
        ));
    }
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
                program: "/bin/bash".to_owned(),
                executable_digest: "sha256:aaaaaaaa".to_owned(),
                config_digest: "sha256:bbbbbbbb".to_owned(),
            },
            workspace_root: "/workspace".to_owned(),
            cwd: "/workspace".to_owned(),
            initial_environment_digest: "sha256:cccccccc".to_owned(),
            environment_names: vec!["LANG".to_owned(), "PATH".to_owned()],
            command_sequence: 0,
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
                workspace_root: "/workspace".to_owned(),
                cwd: "/workspace".to_owned(),
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
    fn human_terminal_ensure_has_strict_v5_wire_shape_and_redacted_debug() {
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
}
