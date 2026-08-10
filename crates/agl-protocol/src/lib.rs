use std::collections::BTreeMap;

mod extension;
mod surface;
pub use extension::*;
pub use surface::*;

use agl_content::Content;
use agl_events::SafeRuntimeEventEnvelope;
pub use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, ExecutionId, OpaqueOwnerId,
};
pub use agl_exec::{
    ExecutionCursor, ExecutionExit, ExecutionIo, ExecutionOwner, ExecutionProfile, ExecutionState,
    KillMode, ProcessBytes, ProcessBytesEncoding, TerminalSize,
};
use agl_ids::{
    AttemptId, DaemonInstanceId, MessageId, RequestId, RunId, SessionId, StepId, TurnId,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;

pub const REQUEST_SCHEMA: &str = "agentlibre.daemon.request.v8alpha";
pub const EVENT_SCHEMA: &str = "agentlibre.daemon.event.v8alpha";
pub const PROTOCOL_VERSION: &str = "v8alpha";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DaemonRequest {
    pub schema: String,
    pub request_id: RequestId,
    #[serde(flatten)]
    pub kind: DaemonRequestKind,
}

impl DaemonRequest {
    pub fn new(request_id: RequestId, kind: DaemonRequestKind) -> Self {
        Self {
            schema: REQUEST_SCHEMA.to_string(),
            request_id,
            kind,
        }
    }

    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        if self.schema != REQUEST_SCHEMA {
            return Err(SurfaceValidationError::new(
                "daemon request schema does not match protocol v8alpha",
            ));
        }
        self.kind.validate_surface()?;
        let encoded = serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("daemon request is not encodable"))?;
        if encoded.len() > MAX_JSONL_FRAME_BYTES {
            return Err(SurfaceValidationError::new(
                "daemon request exceeds the 1 MiB JSONL frame bound",
            ));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for DaemonRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest {
            schema: String,
            request_id: RequestId,
            kind: String,
            payload: Value,
        }

        let wire = WireRequest::deserialize(deserializer)?;
        require_schema::<D::Error>(&wire.schema, REQUEST_SCHEMA)?;
        let kind = decode_tagged::<DaemonRequestKind, D::Error>(wire.kind, wire.payload)?;
        let request = Self {
            schema: wire.schema,
            request_id: wire.request_id,
            kind,
        };
        request.validate().map_err(D::Error::custom)?;
        Ok(request)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DaemonRequestKind {
    Hello(HelloRequest),
    SessionOpen(SessionOpenRequest),
    SetupSmokeSessionOpen(Box<SetupSmokeSessionOpenRequest>),
    SessionClear(SessionClearRequest),
    SessionCancelActive(SessionCancelActiveRequest),
    SessionFinish(SessionFinishRequest),
    SessionStatus(SessionStatusRequest),
    SessionList(SessionListRequest),
    SessionTranscript(SessionTranscriptRequest),
    RunSubmit(RunSubmitRequest),
    RunStatus(RunStatusRequest),
    RunTree(RunTreeRequest),
    RunCancel(RunCancelRequest),
    RunEvents(RunEventsRequest),
    RunSubscribe(RunSubscribeRequest),
    InferenceInventory(InferenceInventoryRequest),
    InferenceStatus(InferenceStatusRequest),
    ModelUnload(ModelUnloadRequest),
    CommandCatalog(CommandCatalogRequest),
    CommandSuggestions(CommandSuggestionsRequest),
    ApplicationAction(ApplicationActionRequest),
    SessionPresentation(SessionPresentationRequest),
    SessionPresentationSubscribe(SessionPresentationSubscribeRequest),
    SubscriptionCancel(SubscriptionCancelRequest),
    HumanTerminalEnsure(HumanTerminalEnsureRequest),
    HumanHostTerminalEnsure(HumanHostTerminalEnsureRequest),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DaemonEvent {
    pub schema: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_metadata: BTreeMap<String, String>,
    #[serde(flatten)]
    pub kind: DaemonEventKind,
}

impl DaemonEvent {
    pub fn new(request_id: Option<RequestId>, kind: DaemonEventKind) -> Self {
        Self {
            schema: EVENT_SCHEMA.to_string(),
            request_id,
            safe_metadata: BTreeMap::new(),
            kind,
        }
    }

    pub fn validate(&self) -> Result<(), SurfaceValidationError> {
        if self.schema != EVENT_SCHEMA {
            return Err(SurfaceValidationError::new(
                "daemon event schema does not match protocol v8alpha",
            ));
        }
        validate_safe_metadata(&self.safe_metadata)?;
        self.kind.validate_surface()?;
        let encoded = serde_json::to_vec(self)
            .map_err(|_| SurfaceValidationError::new("daemon event is not encodable"))?;
        if encoded.len() > MAX_JSONL_FRAME_BYTES {
            return Err(SurfaceValidationError::new(
                "daemon event exceeds the 1 MiB JSONL frame bound",
            ));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for DaemonEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEvent {
            schema: String,
            #[serde(default)]
            request_id: Option<RequestId>,
            #[serde(default)]
            safe_metadata: BTreeMap<String, String>,
            kind: String,
            payload: Value,
        }

        let wire = WireEvent::deserialize(deserializer)?;
        require_schema::<D::Error>(&wire.schema, EVENT_SCHEMA)?;
        let kind = decode_tagged::<DaemonEventKind, D::Error>(wire.kind, wire.payload)?;
        let event = Self {
            schema: wire.schema,
            request_id: wire.request_id,
            safe_metadata: wire.safe_metadata,
            kind,
        };
        event.validate().map_err(D::Error::custom)?;
        Ok(event)
    }
}

fn require_schema<E>(actual: &str, expected: &'static str) -> Result<(), E>
where
    E: serde::de::Error,
{
    if actual == expected {
        Ok(())
    } else {
        Err(E::custom(format_args!(
            "unsupported schema `{actual}`; expected `{expected}`"
        )))
    }
}

fn decode_tagged<T, E>(kind: String, payload: Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let mut value = serde_json::Map::new();
    value.insert("kind".to_string(), Value::String(kind));
    value.insert("payload".to_string(), payload);
    serde_json::from_value(Value::Object(value)).map_err(E::custom)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DaemonEventKind {
    Hello(HelloEvent),
    SessionOpened(SessionOpenedEvent),
    SessionActiveWorkCancelled(SessionActiveWorkCancelledEvent),
    SessionFinished(SessionFinishedEvent),
    SessionStatus(SessionStatusEvent),
    SessionList(SessionListEvent),
    SessionTranscript(SessionTranscriptEvent),
    RunAccepted(RunAcceptedEvent),
    RunStatus(Box<RunStatusEvent>),
    RunTree(RunTreeEvent),
    RunEvents(RunEventsEvent),
    RunSubscriptionStarted(RunSubscriptionStartedEvent),
    RunEvent(Box<SafeRuntimeEventEnvelope>),
    RunSubscriptionFinished(RunSubscriptionFinishedEvent),
    InferenceInventory(InferenceInventoryEvent),
    InferenceStatus(InferenceStatusEvent),
    ModelUnload(ModelUnloadEvent),
    CommandCatalog(CommandCatalogEvent),
    CommandSuggestions(CommandSuggestionsEvent),
    ApplicationToolResult(ApplicationToolResultEvent),
    SessionPresentationSnapshotManifest(SessionPresentationSnapshotManifestEvent),
    SessionPresentationSnapshotChunk(SessionPresentationSnapshotChunkEvent),
    SessionPresentationSnapshotFinished(SessionPresentationSnapshotFinishedEvent),
    SessionPresentationEvent(Box<SessionPresentationEventEnvelope>),
    SessionPresentationSubscriptionFinished(SessionPresentationSubscriptionFinishedEvent),
    SubscriptionCancelled(SubscriptionCancelledEvent),
    HumanTerminalEnsured(HumanTerminalEnsuredEvent),
    Error(ProtocolError),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_protocol_versions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_runtime: Option<RuntimeGenerationIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeGenerationKind {
    Sealed,
    Development,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGenerationIdentity {
    pub kind: RuntimeGenerationKind,
    pub generation_id: String,
    pub builtin_catalog_digest: String,
    pub executable_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloEvent {
    pub protocol_version: String,
    pub product_version: String,
    pub daemon_instance_id: DaemonInstanceId,
    pub daemon_runtime: RuntimeGenerationIdentity,
    pub worker_build_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_bundle_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub composite_worker_build_id: Option<String>,
    pub tools: Vec<DaemonTool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceInventoryRequest {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolInferenceDeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceDeviceEvent {
    pub physical_device_id: String,
    pub pci_device_id: Option<String>,
    pub pci_subsystem_id: Option<String>,
    pub driver_build_id: String,
    pub backend_name: String,
    pub description: String,
    pub kind: ProtocolInferenceDeviceKind,
    pub free_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub usable: bool,
    pub supports_gpu_offload: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceInventoryEvent {
    pub devices: Vec<InferenceDeviceEvent>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceStatusRequest {
    pub detail: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolInferenceWorkerState {
    Cold,
    Starting,
    Ready,
    Busy,
    CoolingDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReleaseReason {
    IdleContext,
    IdleModel,
    Manual,
    Shutdown,
    Capacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReleaseOutcome {
    Released,
    Failed,
    BackendLost,
}

/// Safe aggregate status for the daemon-owned native inference boundary.
///
/// This deliberately carries process and resource accounting only. Model
/// paths, prompts, generated content, backend logs and allocation receipts are
/// private runtime evidence and never enter the public daemon protocol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceStatusEvent {
    pub worker_build_id: String,
    pub worker_state: ProtocolInferenceWorkerState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_device_id: Option<String>,
    pub reserved_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_not_before_unix_ms: Option<u64>,
    pub resident_models: u32,
    pub resident_contexts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_residency_deadline_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_release_reason: Option<ModelReleaseReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_release_outcome: Option<ModelReleaseOutcome>,
    pub automatic_context_unloads: u64,
    pub automatic_model_unloads: u64,
    pub manual_unloads: u64,
    pub unload_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_model_digests: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident_model_digests_truncated: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelUnloadTarget {
    All,
    Digest { digest: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUnloadRequest {
    pub target: ModelUnloadTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUnloadOutcome {
    Released,
    NotResident,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUnloadEvent {
    pub matched_models: u32,
    pub released_models: u32,
    pub released_contexts: u32,
    pub outcome: ModelUnloadOutcome,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonTool {
    SessionOpen,
    SetupSmokeSessionOpen,
    SessionClear,
    SessionCancelActive,
    SessionFinish,
    SessionStatus,
    SessionList,
    SessionTranscript,
    FinalAssistantMessage,
    RuntimeEvents,
    RunSubmit,
    RunStatus,
    RunTree,
    RunCancel,
    RunReplay,
    RunSubscribe,
    InferenceInventory,
    InferenceStatus,
    ModelUnload,
    CommandCatalog,
    CommandSuggestions,
    ApplicationActions,
    SessionPresentation,
    HumanTerminal,
    AssistantDeltas,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub new_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default)]
    pub tool_mode: ProtocolToolMode,
}

/// Opens one daemon-owned setup smoke session with staged model state.
///
/// The daemon fixes this session to read-only, no-history execution. This
/// request deliberately carries no generic Chat/session authority knobs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSmokeSessionOpenRequest {
    pub workspace_root: String,
    pub function_ref: String,
    pub staged_bindings: agl_config::ModelBindings,
    pub runtime_plan: SetupSmokeRuntimePlan,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSmokeRuntimePlan {
    pub profile_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_device_identity: Option<agl_model::LlamaDeviceInfo>,
    pub model: agl_model::RuntimePlanModelIdentity,
    pub runtime: agl_config::InferenceRuntimeConfig,
    pub smoke_timeout_seconds: u64,
    pub expected_speed: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpenedEvent {
    pub session_id: SessionId,
    pub resumed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSubmitRequest {
    pub session_id: SessionId,
    pub content: Content,
    pub client_submission_id: String,
    #[serde(default)]
    pub budget: RunBudgetRequest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStatusRequest {
    pub run_id: RunId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTreeRequest {
    pub run_id: RunId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelRequest {
    pub run_id: RunId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEventsRequest {
    pub run_id: RunId,
    #[serde(default)]
    pub after_sequence: u64,
    #[serde(default = "default_event_replay_limit")]
    pub limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSubscribeRequest {
    pub run_id: RunId,
    #[serde(default)]
    pub after_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunAcceptedEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub state: ProtocolRunState,
    pub replayed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStatusEvent {
    pub session_id: Option<SessionId>,
    pub run_id: RunId,
    pub turn_id: Option<TurnId>,
    pub run_kind: ProtocolRunKind,
    pub state: ProtocolRunState,
    pub concurrency_key: Option<String>,
    pub usage: RunUsageEvent,
    pub cancellation_requested: bool,
    pub attempts: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub error_code: Option<String>,
    pub terminal_result: Option<Value>,
    pub error_message: Option<String>,
    pub parent_run_id: Option<RunId>,
    pub root_run_id: RunId,
    pub depth: u32,
    pub subagent_id: Option<String>,
    pub spawned_by_step_id: Option<StepId>,
    pub child_spec_digest: Option<String>,
    pub model_profile_digest: Option<String>,
    pub result_delivered: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTreeEvent {
    pub requested_run_id: RunId,
    pub runs: Vec<RunTreeNodeEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTreeNodeEvent {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub run_kind: ProtocolRunKind,
    pub state: ProtocolRunState,
    pub concurrency_key: Option<String>,
    pub usage: RunUsageEvent,
    pub cancellation_requested: bool,
    pub attempts: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub error_code: Option<String>,
    pub parent_run_id: Option<RunId>,
    pub root_run_id: RunId,
    pub depth: u32,
    pub subagent_id: Option<String>,
    pub spawned_by_step_id: Option<StepId>,
    pub child_spec_digest: Option<String>,
    pub model_profile_digest: Option<String>,
    pub result_delivered: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEventsEvent {
    pub run_id: RunId,
    pub after_sequence: u64,
    pub events: Vec<SafeRuntimeEventEnvelope>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSubscriptionStartedEvent {
    pub run_id: RunId,
    pub after_sequence: u64,
    pub replay_boundary: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSubscriptionFinishedEvent {
    pub run_id: RunId,
    pub state: ProtocolRunState,
    pub last_sequence: u64,
    pub terminal_result: Option<Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBudgetRequest {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub tool_calls: u32,
}

impl Default for RunBudgetRequest {
    fn default() -> Self {
        Self {
            wall_time_ms: 600_000,
            model_input_tokens: 1_000_000,
            model_output_tokens: 100_000,
            model_attempts: 32,
            tool_calls: 64,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUsageEvent {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub tool_calls: u32,
}

fn default_event_replay_limit() -> usize {
    1_000
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionClearRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFinishRequest {
    pub session_id: SessionId,
    pub reason: SessionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCancelActiveRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListRequest {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTranscriptRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub include_content: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFinishedEvent {
    pub session_id: SessionId,
    pub reason: SessionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionActiveWorkCancelledEvent {
    pub session_id: SessionId,
    pub cancelled_runs: u32,
    pub terminated_terminals: u32,
    pub terminated_executions: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusEvent {
    pub session_id: SessionId,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListEvent {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTranscriptEvent {
    pub session_id: SessionId,
    pub events: Vec<TranscriptEvent>,
    pub content_included: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub status: SessionStatus,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProtocolToolMode {
    #[default]
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Answered,
    Stopped,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolRunKind {
    Turn,
    Cron,
    Subagent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolRunState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Incomplete,
    Failed,
    Cancelled,
}

impl ProtocolRunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Incomplete | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFinishReason {
    Eof,
    ExitCommand,
    HostShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Open,
    Busy,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEvent {
    UserMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Content>,
    },
    AssistantMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Content>,
    },
    AssistantIncomplete {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        source_attempt_id: AttemptId,
        reason: IncompleteOutputReason,
        continuation_index: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Content>,
    },
    AssistantToolCall {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
    },
    ToolMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    ModelAttemptLinked {
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
    },
    ContextCleared,
    SessionFinished {
        reason: SessionFinishReason,
    },
    SessionFailed {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<ProtocolErrorDetails>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_metadata: BTreeMap<String, String>,
}

impl ProtocolError {
    pub fn new(code: ProtocolErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            details: None,
            safe_metadata: BTreeMap::new(),
        }
    }

    pub fn with_details(mut self, details: ProtocolErrorDetails) -> Self {
        self.details = Some(Box::new(details));
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtocolErrorDetails {
    RuntimeIdentityMismatch {
        client: RuntimeGenerationIdentity,
        daemon: RuntimeGenerationIdentity,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    UnsupportedProtocolVersion,
    RuntimeIdentityMismatch,
    InvalidRequest,
    Unauthorized,
    NotFound,
    Busy,
    Unsupported,
    RuntimeFailure,
    InvalidArguments,
    CommandUnavailable,
    SessionBusy,
    NotAuthorized,
    AuthorizationRequired,
    ConfirmationRequired,
    StaleContextRevision,
    TerminalOwnerMismatch,
    WriterLeaseBusy,
    ModelNotInstalled,
    ModelContextTooSmall,
    SkillNotAdmitted,
    IncompleteOutputNotFound,
    ContinuationAlreadyClaimed,
    StaleContinuationContext,
    InputBackpressure,
    ActivityCapacityExceeded,
    ResyncRequired,
    OutcomeUnknown,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_run_request_budget_covers_the_longest_admitted_inference_operation() {
        let budget = RunBudgetRequest::default();

        assert_eq!(budget.wall_time_ms, 600_000);
        assert_eq!(budget.model_input_tokens, 1_000_000);
        assert_eq!(budget.model_output_tokens, 100_000);
        assert_eq!(budget.model_attempts, 32);
        assert_eq!(budget.tool_calls, 64);
    }

    const REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000001";
    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000002";
    const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000003";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000004";
    const MESSAGE_ID_1: &str = "msg_01890f17-4a00-7000-8000-000000000005";
    const MESSAGE_ID_2: &str = "msg_01890f17-4a00-7000-8000-000000000006";
    const MESSAGE_ID_3: &str = "msg_01890f17-4a00-7000-8000-000000000007";
    const ATTEMPT_ID: &str = "attempt_01890f17-4a00-7000-8000-000000000008";

    fn request_id() -> RequestId {
        RequestId::parse(REQUEST_ID).unwrap()
    }

    fn session_id() -> SessionId {
        SessionId::parse(SESSION_ID).unwrap()
    }

    fn run_id() -> RunId {
        RunId::parse(RUN_ID).unwrap()
    }

    fn turn_id() -> TurnId {
        TurnId::parse(TURN_ID).unwrap()
    }

    fn message_id(value: &str) -> MessageId {
        MessageId::parse(value).unwrap()
    }

    fn attempt_id() -> AttemptId {
        AttemptId::parse(ATTEMPT_ID).unwrap()
    }

    fn runtime_identity(fill: char) -> RuntimeGenerationIdentity {
        RuntimeGenerationIdentity {
            kind: RuntimeGenerationKind::Sealed,
            generation_id: format!("sha256:{}", fill.to_string().repeat(64)),
            builtin_catalog_digest: format!("sha256:{}", fill.to_string().repeat(64)),
            executable_digest: format!("sha256:{}", fill.to_string().repeat(64)),
        }
    }

    #[test]
    fn run_submit_request_round_trips_as_jsonl_shape() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::RunSubmit(RunSubmitRequest {
                session_id: session_id(),
                content: Content::text("hello").unwrap(),
                client_submission_id: "matrix-event-001".to_string(),
                budget: RunBudgetRequest::default(),
            }),
        );

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"schema\":\"agentlibre.daemon.request.v8alpha\""));
        assert!(json.contains(&format!("\"request_id\":\"{REQUEST_ID}\"")));
        assert!(json.contains("\"kind\":\"run_submit\""));
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }

    fn setup_smoke_request() -> SetupSmokeSessionOpenRequest {
        SetupSmokeSessionOpenRequest {
            workspace_root: "/workspace".to_owned(),
            function_ref: "gemma4-12b".to_owned(),
            staged_bindings: agl_config::ModelBindings {
                version: 1,
                models: BTreeMap::from([(
                    agl_config::ModelId::new("gemma4-12b").unwrap(),
                    agl_config::ModelBinding {
                        path: "/models/gemma4-12b.gguf".into(),
                    },
                )]),
            },
            runtime_plan: SetupSmokeRuntimePlan {
                profile_id: "gpu-64k".to_owned(),
                selected_device: Some("pci:0000:03:00.0".to_owned()),
                selected_device_identity: Some(agl_model::LlamaDeviceInfo {
                    name: "pci:0000:03:00.0".to_owned(),
                    description: "test GPU".to_owned(),
                    kind: agl_model::LlamaDeviceKind::DiscreteGpu,
                    pci_device_id: Some("1002:744c".to_owned()),
                    pci_subsystem_id: Some("1da2:471e".to_owned()),
                    free_memory_bytes: 20_000_000_000,
                    total_memory_bytes: 24_000_000_000,
                    usable: true,
                    supports_gpu_offload: true,
                }),
                model: test_runtime_plan_model_identity("gemma4-12b"),
                runtime: agl_config::InferenceRuntimeConfig {
                    gpu_layers: 65,
                    context_tokens: 65_536,
                    threads: 8,
                    device: Some("pci:0000:03:00.0".to_owned()),
                    batch_size: Some(1_024),
                    ubatch_size: Some(256),
                    flash_attention: Some(agl_config::RuntimeSwitch::On),
                    cache_type_k: Some(agl_config::KvCacheType::Q8_0),
                    cache_type_v: Some(agl_config::KvCacheType::Q8_0),
                    mmap: Some(true),
                    kv_unified: Some(true),
                    structured_decoding: agl_config::StructuredDecodingMode::Auto,
                    repair_malformed_tool_calls: true,
                    mtp: agl_config::MtpRuntimeConfig::default(),
                },
                smoke_timeout_seconds: 300,
                expected_speed: "interactive".to_owned(),
            },
            max_output_tokens: 32,
        }
    }

    fn test_runtime_plan_model_identity(id: &str) -> agl_model::RuntimePlanModelIdentity {
        serde_json::from_value(serde_json::json!({
            "provenance": {
                "reference": format!("model:{id}@=1.0.0"),
                "source_id": "test",
                "source_tier": "workspace",
                "source_kind": "directory",
                "package_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            },
            "weights": [{
                "role": "main",
                "model_id": id,
                "filename": format!("{id}.gguf"),
                "byte_size": 18,
                "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                "required": true
            }]
        }))
        .unwrap()
    }

    #[test]
    fn setup_smoke_session_request_is_typed_bounded_and_exact() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::SetupSmokeSessionOpen(Box::new(setup_smoke_request())),
        );
        request.validate().unwrap();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["kind"], "setup_smoke_session_open");
        assert_eq!(
            value["payload"]["staged_bindings"]["models"]["gemma4-12b"]["path"],
            "/models/gemma4-12b.gguf"
        );
        assert_eq!(
            serde_json::from_value::<DaemonRequest>(value).unwrap(),
            request
        );

        let mut relative = setup_smoke_request();
        relative
            .staged_bindings
            .models
            .values_mut()
            .next()
            .unwrap()
            .path = "relative.gguf".into();
        assert!(
            DaemonRequest::new(
                request_id(),
                DaemonRequestKind::SetupSmokeSessionOpen(Box::new(relative))
            )
            .validate()
            .is_err()
        );

        let mut unbounded = setup_smoke_request();
        unbounded.max_output_tokens = MAX_SETUP_SMOKE_OUTPUT_TOKENS + 1;
        assert!(
            DaemonRequest::new(
                request_id(),
                DaemonRequestKind::SetupSmokeSessionOpen(Box::new(unbounded))
            )
            .validate()
            .is_err()
        );
    }

    #[test]
    fn hello_event_declares_version_and_capabilities() {
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::Hello(HelloEvent {
                protocol_version: PROTOCOL_VERSION.to_string(),
                product_version: "1.0.0-alpha.6".to_string(),
                daemon_instance_id: DaemonInstanceId::generate(),
                daemon_runtime: runtime_identity('a'),
                worker_build_id: format!("sha256:{}", "b".repeat(64)),
                native_bundle_id: Some(format!("sha256:{}", "c".repeat(64))),
                composite_worker_build_id: Some(format!(
                    "sha256:{}+sha256:{}",
                    "b".repeat(64),
                    "c".repeat(64)
                )),
                tools: vec![
                    DaemonTool::SessionOpen,
                    DaemonTool::RunSubmit,
                    DaemonTool::RunSubscribe,
                ],
            }),
        );

        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["schema"], EVENT_SCHEMA);
        assert_eq!(value["kind"], "hello");
        assert_eq!(value["payload"]["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(
            value["payload"]["daemon_runtime"]["generation_id"],
            format!("sha256:{}", "a".repeat(64))
        );
        assert_eq!(value["payload"]["tools"][1], "run_submit");
        assert_eq!(serde_json::from_value::<DaemonEvent>(value).unwrap(), event);
    }

    #[test]
    fn run_control_frames_carry_the_admitted_identity() {
        let accepted = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::RunAccepted(RunAcceptedEvent {
                session_id: session_id(),
                run_id: run_id(),
                turn_id: turn_id(),
                state: ProtocolRunState::Queued,
                replayed: false,
            }),
        );
        let finished = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::RunSubscriptionFinished(RunSubscriptionFinishedEvent {
                run_id: run_id(),
                state: ProtocolRunState::Succeeded,
                last_sequence: 3,
                terminal_result: Some(serde_json::json!({ "status": "answered" })),
                error_code: None,
                error_message: None,
            }),
        );

        let accepted_value = serde_json::to_value(&accepted).unwrap();
        assert_eq!(accepted_value["payload"]["session_id"], SESSION_ID);
        assert_eq!(accepted_value["payload"]["run_id"], RUN_ID);
        assert_eq!(accepted_value["payload"]["turn_id"], TURN_ID);
        assert_eq!(
            serde_json::from_value::<DaemonEvent>(accepted_value).unwrap(),
            accepted
        );

        let finished_value = serde_json::to_value(&finished).unwrap();
        assert_eq!(finished_value["payload"]["run_id"], RUN_ID);
        assert_eq!(finished_value["payload"]["last_sequence"], 3);
        assert_eq!(
            serde_json::from_value::<DaemonEvent>(finished_value).unwrap(),
            finished
        );
    }

    #[test]
    fn run_tree_exposes_safe_relationships_without_private_results() {
        let child_run_id = RunId::generate();
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::RunTree(RunTreeEvent {
                requested_run_id: run_id(),
                runs: vec![RunTreeNodeEvent {
                    run_id: child_run_id,
                    session_id: None,
                    turn_id: None,
                    run_kind: ProtocolRunKind::Subagent,
                    state: ProtocolRunState::Failed,
                    concurrency_key: None,
                    usage: RunUsageEvent::default(),
                    cancellation_requested: false,
                    attempts: 1,
                    created_at_ms: 1,
                    updated_at_ms: 2,
                    started_at_ms: Some(1),
                    finished_at_ms: Some(2),
                    error_code: Some("chat_turn_failed".to_string()),
                    parent_run_id: Some(run_id()),
                    root_run_id: run_id(),
                    depth: 1,
                    subagent_id: Some("reviewer".to_string()),
                    spawned_by_step_id: Some(StepId::generate()),
                    child_spec_digest: Some(format!("sha256:{}", "a".repeat(64))),
                    model_profile_digest: Some(format!("sha256:{}", "b".repeat(64))),
                    result_delivered: true,
                }],
            }),
        );

        let value = serde_json::to_value(&event).unwrap();
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(value["kind"], "run_tree");
        assert_eq!(value["payload"]["runs"][0]["run_kind"], "subagent");
        assert!(!encoded.contains("terminal_result"));
        assert!(!encoded.contains("error_message"));
        assert!(!encoded.contains("task"));
        assert_eq!(serde_json::from_value::<DaemonEvent>(value).unwrap(), event);
    }

    #[test]
    fn transcript_can_omit_content_by_default() {
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::SessionTranscript(SessionTranscriptEvent {
                session_id: session_id(),
                content_included: false,
                events: vec![
                    TranscriptEvent::UserMessage {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        message_id: message_id(MESSAGE_ID_1),
                        content: None,
                    },
                    TranscriptEvent::AssistantToolCall {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        message_id: message_id(MESSAGE_ID_2),
                        name: "core.workspace:fs.read".to_string(),
                        arguments: None,
                    },
                    TranscriptEvent::ToolMessage {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        message_id: message_id(MESSAGE_ID_3),
                        name: "core.workspace:fs.read".to_string(),
                        data: None,
                    },
                    TranscriptEvent::ModelAttemptLinked {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        attempt_id: attempt_id(),
                    },
                ],
            }),
        );

        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains("\"content_included\":false"));
        assert!(!json.contains("secret prompt"));
        assert!(!json.contains("\"arguments\""));
        assert!(!json.contains("\"content\""));
        let decoded: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn protocol_error_has_stable_shape() {
        let error = ProtocolError::new(
            ProtocolErrorCode::UnsupportedProtocolVersion,
            "unsupported protocol version",
            false,
        );
        let event = DaemonEvent::new(Some(request_id()), DaemonEventKind::Error(error));

        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["kind"], "error");
        assert_eq!(value["payload"]["code"], "unsupported_protocol_version");
        assert_eq!(value["payload"]["retryable"], false);
    }

    #[test]
    fn runtime_mismatch_error_round_trips_both_typed_identities() {
        let client = runtime_identity('c');
        let daemon = runtime_identity('d');
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::Error(
                ProtocolError::new(
                    ProtocolErrorCode::RuntimeIdentityMismatch,
                    "runtime identities do not match",
                    false,
                )
                .with_details(ProtocolErrorDetails::RuntimeIdentityMismatch {
                    client: client.clone(),
                    daemon: daemon.clone(),
                }),
            ),
        );

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["payload"]["code"], "runtime_identity_mismatch");
        assert_eq!(
            value["payload"]["details"]["kind"],
            "runtime_identity_mismatch"
        );
        assert_eq!(
            value["payload"]["details"]["client"]["generation_id"],
            client.generation_id
        );
        assert_eq!(serde_json::from_value::<DaemonEvent>(value).unwrap(), event);
    }

    #[test]
    fn inference_status_detail_request_and_private_default_shape_round_trip() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::InferenceStatus(InferenceStatusRequest { detail: true }),
        );
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonRequest>(&encoded).unwrap(),
            request
        );
        assert!(encoded.contains("\"detail\":true"));

        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::InferenceStatus(InferenceStatusEvent {
                worker_build_id: "sha256:worker".to_owned(),
                worker_state: ProtocolInferenceWorkerState::Cold,
                worker_pid: None,
                launch_generation: None,
                physical_device_id: None,
                reserved_bytes: 0,
                cooldown_not_before_unix_ms: None,
                resident_models: 0,
                resident_contexts: 0,
                next_residency_deadline_after_ms: None,
                last_release_reason: None,
                last_release_outcome: None,
                automatic_context_unloads: 0,
                automatic_model_unloads: 0,
                manual_unloads: 0,
                unload_failures: 0,
                resident_model_digests: None,
                resident_model_digests_truncated: None,
            }),
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonEvent>(&encoded).unwrap(),
            event
        );
        assert!(!encoded.contains("resident_model_digests"));
        assert!(!encoded.contains("model_path"));
        assert!(!encoded.contains("prompt"));
        assert!(!encoded.contains("backend_log"));

        let missing_detail = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "inference_status",
            "payload": {}
        });
        assert!(serde_json::from_value::<DaemonRequest>(missing_detail).is_err());
    }

    #[test]
    fn model_unload_rejects_unknown_target_fields() {
        let value = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "model_unload",
            "payload": {
                "target": {
                    "kind": "digest",
                    "digest": "a".repeat(64),
                    "path": "/private/model.gguf"
                }
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(value).is_err());
    }

    #[test]
    fn previous_alpha_and_untyped_id_shapes_are_rejected() {
        let previous_alpha = serde_json::json!({
            "schema": format!("agentlibre.daemon.request.v{}alpha", 4),
            "request_id": REQUEST_ID,
            "kind": "session_turn",
            "payload": {
                "session_id": SESSION_ID,
                "text": "hello"
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(previous_alpha).is_err());

        let untyped_ids = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": "req-001",
            "kind": "run_submit",
            "payload": {
                "session_id": "session-001",
                "text": "hello",
                "budget": RunBudgetRequest::default()
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(untyped_ids).is_err());
    }

    #[test]
    fn previous_transcript_and_session_opened_shapes_are_rejected() {
        let previous_transcript = serde_json::json!({
            "schema": EVENT_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_transcript",
            "payload": {
                "session_id": SESSION_ID,
                "content_included": false,
                "events": [{
                    "kind": "user_message",
                    "message_id": MESSAGE_ID_1
                }]
            }
        });
        assert!(serde_json::from_value::<DaemonEvent>(previous_transcript).is_err());

        let previous_opened = serde_json::json!({
            "schema": EVENT_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_opened",
            "payload": {
                "session_id": SESSION_ID,
                "run_id": RUN_ID,
                "resumed": false
            }
        });
        assert!(serde_json::from_value::<DaemonEvent>(previous_opened).is_err());
    }

    #[test]
    fn protocol_envelopes_and_payloads_reject_unknown_fields() {
        let unknown_envelope_field = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_list",
            "payload": {},
            "legacy": true
        });
        assert!(serde_json::from_value::<DaemonRequest>(unknown_envelope_field).is_err());

        let unknown_payload_field = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_list",
            "payload": { "legacy": true }
        });
        assert!(serde_json::from_value::<DaemonRequest>(unknown_payload_field).is_err());
    }

    #[test]
    fn removed_execution_payloads_are_rejected_by_the_agent_protocol() {
        let removed_resize = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "execution_resize",
            "payload": {
                "attachment_id": "exec_req_01890f17-4a00-7000-8000-00000000000a",
                "columns": 80,
                "rows": 24,
                "legacy": true
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(removed_resize).is_err());

        let removed_detach = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "execution_detach",
            "payload": {
                "attachment_id": REQUEST_ID
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(removed_detach).is_err());

        let removed_status = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "execution_status",
            "payload": {
                "execution_id": "process-9"
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(removed_status).is_err());
    }
}
