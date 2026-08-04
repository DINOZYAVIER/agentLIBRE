use std::collections::BTreeSet;
use std::fmt::{self, Debug, Formatter};
use std::path::Path;

use agl_content::Content;
use agl_ids::{AttemptId, DaemonInstanceId, EventId, MessageId, RunId, SessionId, StepId, TurnId};
use agl_kernel::ToolAccessMode;
use agl_process::{
    ExecutionCursor, ExecutionExit, ExecutionId, ExecutionProfile, ExecutionState,
    SanitizedTerminalOutput,
};
use agl_terminal::TerminalId;
use serde::{Deserialize, Serialize};

use crate::{
    ApplicationError, ApplicationErrorCode, CommandContext, MAX_QUEUED_PROMPTS_PER_SESSION,
    MAX_TERMINAL_PATH_BYTES, MAX_TERMINALS_PER_SESSION, TerminalSessionView,
};

pub const MAX_PRESENTATION_ITEMS: usize = 2_000;
pub const MAX_EXECUTION_VIEWS: usize = 2_000;
pub const MAX_PRESENTATION_CONTENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_HUMAN_COMMAND_CARDS: usize = 32;
pub const MAX_HUMAN_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_HUMAN_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_HUMAN_COMMAND_AGGREGATE_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ACTIVITY_NODES: usize = 512;
pub const MAX_ACTIVE_ACTIVITY_NODES: usize = 256;
pub const MAX_ACTIVITY_PATH_NODES: usize = 32;
pub const MAX_ACTIVITY_SUMMARY_BYTES: usize = 1024;
pub const MAX_ACTIVITY_NODE_BYTES: usize = 8 * 1024;
pub const MAX_ACTIVITY_GRAPH_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ACTIVE_ACTIVITY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ACTIVITY_DELTA_BYTES: usize = 700 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SanitizedDisplayPath {
    pub text: String,
    pub truncated: bool,
}

impl SanitizedDisplayPath {
    /// Builds a one-way presentation value. It must never be parsed back into
    /// a filesystem or authority path.
    pub fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            Self::from_path_bytes(path.as_os_str().as_bytes())
        }
        #[cfg(not(unix))]
        {
            Self::from_path_bytes(path.to_string_lossy().as_bytes())
        }
    }

    pub fn from_utf8(value: &str) -> Self {
        Self::from_path_bytes(value.as_bytes())
    }

    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.text.is_empty()
            || self.text.len() > MAX_TERMINAL_PATH_BYTES
            || self.text.chars().any(is_unsafe_display_path_character)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "sanitized display path must be nonempty bounded text without control or format characters",
            ));
        }
        Ok(())
    }

    fn from_path_bytes(mut bytes: &[u8]) -> Self {
        let mut text = String::new();
        let mut truncated = false;
        while !bytes.is_empty() && !truncated {
            match std::str::from_utf8(bytes) {
                Ok(valid) => {
                    append_display_path_text(&mut text, valid, &mut truncated);
                    break;
                }
                Err(error) => {
                    let valid = &bytes[..error.valid_up_to()];
                    append_display_path_text(
                        &mut text,
                        // SAFETY: `valid_up_to` identifies a valid UTF-8 prefix.
                        unsafe { std::str::from_utf8_unchecked(valid) },
                        &mut truncated,
                    );
                    if truncated {
                        break;
                    }
                    let invalid = error.error_len().unwrap_or(bytes.len() - valid.len());
                    for byte in &bytes[valid.len()..valid.len() + invalid] {
                        if !append_display_path_fragment(&mut text, &format!("\\x{byte:02X}")) {
                            truncated = true;
                            break;
                        }
                    }
                    bytes = &bytes[valid.len() + invalid..];
                }
            }
        }
        Self { text, truncated }
    }
}

fn append_display_path_text(output: &mut String, value: &str, truncated: &mut bool) {
    for character in value.chars() {
        let fragment = if character == '\\' {
            "\\\\".to_owned()
        } else if is_unsafe_display_path_character(character) {
            format!("\\u{{{:X}}}", character as u32)
        } else {
            character.to_string()
        };
        if !append_display_path_fragment(output, &fragment) {
            *truncated = true;
            break;
        }
    }
}

fn append_display_path_fragment(output: &mut String, fragment: &str) -> bool {
    if output.len().saturating_add(fragment.len()) > MAX_TERMINAL_PATH_BYTES {
        return false;
    }
    output.push_str(fragment);
    true
}

fn is_unsafe_display_path_character(character: char) -> bool {
    character.is_control() || is_unicode_format_control(character as u32)
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
    pub operation_mode: ToolAccessMode,
    pub selected_skills: Vec<String>,
    pub runtime_context_revision: u64,
    pub workspace_root: SanitizedDisplayPath,
    /// Opaque identity for private per-workspace client state. This is not a
    /// filesystem path and must never be used as authority input.
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

impl SessionPresentationItem {
    pub fn key(&self) -> String {
        match self {
            Self::UserMessage { message_id, .. } | Self::AssistantMessage { message_id, .. } => {
                message_id.to_string()
            }
            Self::IncompleteAssistant { item } => item.message_id.to_string(),
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
    /// Converts only the output of the daemon-side terminal sanitizer into a
    /// presentation value; raw PTY bytes and shell-integration strings have no
    /// direct constructor on this type.
    pub fn from_process_sanitized(output: &SanitizedTerminalOutput) -> Self {
        Self(output.text().to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self, maximum_bytes: usize, allow_empty: bool) -> Result<(), ApplicationError> {
        if (!allow_empty && self.0.is_empty())
            || self.0.len() > maximum_bytes
            || self.0.chars().any(is_forbidden_presentation_character)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
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
    pub terminal_id: TerminalId,
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

impl HumanCommandCardView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.command.validate(MAX_HUMAN_COMMAND_BYTES, false)?;
        self.output.validate(MAX_HUMAN_COMMAND_OUTPUT_BYTES, true)?;
        if self.output_start.after_sequence > self.output_end.after_sequence
            || self.updated_at_unix_ms < self.started_at_unix_ms
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "Human command card cursors or timestamps are inconsistent",
            ));
        }
        self.cwd.validate()?;
        if matches!(self.state, HumanCommandCardState::Exited) != self.exit_status.is_some() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "only an exited Human command card carries an exit status",
            ));
        }
        Ok(())
    }
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

impl ActivityNodeView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.node_id.is_empty()
            || self.node_id.len() > 512
            || self
                .parent_node_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 512)
            || self
                .node_id
                .chars()
                .any(is_forbidden_presentation_character)
            || self
                .parent_node_id
                .as_ref()
                .is_some_and(|id| id.chars().any(is_forbidden_presentation_character))
            || self.summary.len() > MAX_ACTIVITY_SUMMARY_BYTES
            || self
                .summary
                .chars()
                .any(is_forbidden_presentation_character)
            || contains_absolute_display_path(&self.summary)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity node identity or summary exceeds its safe display bound",
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
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity node timing is inconsistent with its state",
            ));
        }
        let encoded_bytes = serde_json::to_vec(self)
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "activity node could not be encoded",
                )
            })?
            .len();
        if encoded_bytes > MAX_ACTIVITY_NODE_BYTES {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity node exceeds its encoded display bound",
            ));
        }
        Ok(())
    }
}

impl ActivityDetailView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
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
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "capability activity path must be a normalized workspace-relative display value",
                ));
            }
        }
        let capability_id = match self {
            Self::Capability(CapabilityActivityDetail::PolicyCheck { capability_id, .. })
            | Self::UnknownCapability { capability_id } => Some(capability_id),
            _ => None,
        };
        if capability_id.is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || value.chars().any(is_forbidden_presentation_character)
                || contains_absolute_display_path(value)
        }) {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity capability identity exceeds its safe display bound",
            ));
        }
        if let Self::Inference(detail) = self
            && matches!((detail.completed, detail.total), (Some(done), Some(total)) if done > total)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity inference progress cannot exceed its total",
            ));
        }
        if let Self::Inference(detail) = self
            && (detail.completed.is_some() || detail.total.is_some())
            && detail.unit.is_none()
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
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
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity inference cache disposition does not match its stage",
                ));
            }
        }
        if let Self::Aggregate(detail) = self
            && detail.collapsed_nodes == 0
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity aggregate must represent at least one collapsed node",
            ));
        }
        Ok(())
    }
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

impl ActivityGraphView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.graph_revision == 0
            || self.nodes.len() > MAX_ACTIVITY_NODES
            || self.roots.len() > MAX_ACTIVITY_NODES
            || self.current_path.len() > MAX_ACTIVITY_PATH_NODES
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity graph exceeds its bounded node or path limit",
            ));
        }
        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            node.validate()?;
            if !ids.insert(node.node_id.as_str()) {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
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
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity graph roots must be canonical run or aggregate roots",
            ));
        }
        for node in &self.nodes {
            if node
                .parent_node_id
                .as_ref()
                .is_some_and(|parent| !ids.contains(parent.as_str()))
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity graph parent must reference an existing node",
                ));
            }
            let mut cursor = Some(node.node_id.as_str());
            let mut visited = BTreeSet::new();
            while let Some(node_id) = cursor {
                if !visited.insert(node_id) {
                    return Err(ApplicationError::new(
                        ApplicationErrorCode::InvalidArguments,
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
                .any(|id| self.roots.iter().any(|root| root == id))
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
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
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity nodes must use deterministic parent-before-child ordering",
            ));
        }
        let mut order_indices = BTreeSet::new();
        if self
            .nodes
            .iter()
            .any(|node| node.order_index == 0 || !order_indices.insert(node.order_index))
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
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
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity current path must be a connected root-to-node chain",
            ));
        }
        let encoded_bytes = serde_json::to_vec(self)
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "activity graph could not be encoded",
                )
            })?
            .len();
        if encoded_bytes > MAX_ACTIVITY_GRAPH_BYTES {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity graph exceeds its encoded display bound",
            ));
        }
        Ok(())
    }
}

impl ActivityGraphDeltaBatch {
    pub fn validate_shape(&self) -> Result<(), ApplicationError> {
        if self.graph_revision == 0
            || self.upserts.len() > MAX_ACTIVITY_NODES
            || self.removals.len() > MAX_ACTIVITY_NODES
            || self
                .current_path
                .as_ref()
                .is_some_and(|path| path.len() > MAX_ACTIVITY_PATH_NODES)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity delta exceeds its bounded shape",
            ));
        }
        if self.current_path.as_ref().is_some_and(|path| {
            path.iter().any(|id| {
                id.is_empty()
                    || id.len() > 512
                    || id.chars().any(is_forbidden_presentation_character)
            })
        }) {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity delta current path contains an unsafe node identity",
            ));
        }
        let mut upserts = BTreeSet::new();
        let mut order_indices = BTreeSet::new();
        for (index, node) in self.upserts.iter().enumerate() {
            node.validate()?;
            if node.order_index == 0
                || !upserts.insert(node.node_id.as_str())
                || !order_indices.insert(node.order_index)
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity delta upsert identities and order indices must be unique and nonzero",
                ));
            }
            if node.parent_node_id.as_ref().is_some_and(|parent| {
                self.upserts
                    .iter()
                    .position(|candidate| &candidate.node_id == parent)
                    .is_some_and(|parent_index| parent_index >= index)
            }) {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity delta parents must precede their children",
                ));
            }
        }
        let mut removals = BTreeSet::new();
        for removal in &self.removals {
            if removal.subtree_root_id.is_empty()
                || removal.subtree_root_id.len() > 512
                || removal
                    .subtree_root_id
                    .chars()
                    .any(is_forbidden_presentation_character)
                || !removals.insert(removal.subtree_root_id.as_str())
                || upserts.contains(removal.subtree_root_id.as_str())
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity delta removal identities must be unique bounded text",
                ));
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "activity delta could not be encoded",
                )
            })?
            .len()
            > MAX_ACTIVITY_DELTA_BYTES
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity delta exceeds its encoded wire bound",
            ));
        }
        Ok(())
    }
}

fn canonical_activity_node_ids(
    nodes: &[ActivityNodeView],
) -> Result<Vec<String>, ApplicationError> {
    let mut children = std::collections::BTreeMap::<Option<&str>, Vec<&ActivityNodeView>>::new();
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
        children: &std::collections::BTreeMap<Option<&str>, Vec<&ActivityNodeView>>,
        visiting: &mut BTreeSet<String>,
        output: &mut Vec<String>,
    ) -> Result<(), ApplicationError> {
        for node in children.get(&parent).into_iter().flatten() {
            if !visiting.insert(node.node_id.clone()) {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
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

impl ExecutionView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.cwd.validate()?;
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
    pub human_commands: Vec<HumanCommandCardView>,
    pub activity: Option<ActivityGraphView>,
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
        self.header.workspace_root.validate()?;
        validate_workspace_history_scope(&self.header.workspace_history_scope)?;
        self.header.cwd.validate()?;
        if self.items.len() > MAX_PRESENTATION_ITEMS
            || self.queued_prompts.len() > MAX_QUEUED_PROMPTS_PER_SESSION
            || self.terminals.len() > MAX_TERMINALS_PER_SESSION
            || self.executions.len() > MAX_EXECUTION_VIEWS
            || self.human_commands.len() > MAX_HUMAN_COMMAND_CARDS
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
        let mut command_keys = BTreeSet::new();
        let mut command_output_bytes = 0usize;
        for command in &self.human_commands {
            command.validate()?;
            command_output_bytes =
                command_output_bytes.saturating_add(command.output.as_str().len());
            if !command_keys.insert((&command.terminal_id, command.command_sequence)) {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "Human command card identities must be unique",
                ));
            }
        }
        if command_output_bytes > MAX_HUMAN_COMMAND_AGGREGATE_OUTPUT_BYTES {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "Human command cards exceed their aggregate output bound",
            ));
        }
        if let Some(activity) = &self.activity {
            activity.validate()?;
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

pub(crate) fn validate_workspace_history_scope(scope: &str) -> Result<(), ApplicationError> {
    let digest = scope.strip_prefix("sha256:").ok_or_else(|| {
        ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "workspace history scope must be an opaque SHA-256 identity",
        )
    })?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "workspace history scope must be an opaque SHA-256 identity",
        ));
    }
    Ok(())
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
        terminal_id: TerminalId,
    },
    TerminalCommandStarted {
        terminal_id: TerminalId,
        sequence: u64,
    },
    TerminalCommandFinished {
        terminal_id: TerminalId,
        sequence: u64,
        exit_status: i32,
        cwd: SanitizedDisplayPath,
    },
    HumanCommandCardUpsert {
        card: HumanCommandCardView,
    },
    HumanCommandCardRemoved {
        terminal_id: TerminalId,
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

fn is_forbidden_presentation_character(character: char) -> bool {
    let code = character as u32;
    (code <= 0x1f && character != '\n' && character != '\t')
        || (0x7f..=0x9f).contains(&code)
        || is_unicode_format_control(code)
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPresentationEventEnvelope {
    pub event_id: EventId,
    pub session_id: SessionId,
    pub cursor: PresentationCursor,
    pub event: SessionPresentationEvent,
}
