use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use serde::{Deserialize, Serialize};

pub const ENGINE_PROTOCOL_ID: &str =
    "sha256:56fd533515ddde79ea20f9d795613636e35506bdc0394ce53387a16570c4ab89";
pub const ENGINE_BINARY_NAME: &str = "llama-server";

#[derive(Clone, Default)]
pub struct InferenceCancellation {
    cancelled: Arc<AtomicBool>,
}

impl InferenceCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl fmt::Debug for InferenceCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InferenceCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InferenceHostJobScope {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub attempt_id: AttemptId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelManagerStatusDetail {
    #[default]
    Aggregate,
    ModelDigests,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelUnloadTarget {
    All,
    Digest(String),
}

impl ModelUnloadTarget {
    pub fn digest(digest: impl Into<String>) -> Result<Self, ModelManagerError> {
        let digest = digest.into();
        if !valid_digest(&digest) {
            return Err(ModelManagerError::InvalidUnloadTarget {
                message: "model digest must contain exactly 64 lowercase hexadecimal characters"
                    .to_owned(),
            });
        }
        Ok(Self::Digest(digest))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUnloadOutcome {
    Released,
    NotResident,
    Busy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelUnloadResult {
    pub matched_models: u32,
    pub released_models: u32,
    pub released_contexts: u32,
    pub outcome: ModelUnloadOutcome,
}

impl ModelUnloadResult {
    pub const fn not_resident() -> Self {
        Self {
            matched_models: 0,
            released_models: 0,
            released_contexts: 0,
            outcome: ModelUnloadOutcome::NotResident,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReleaseReason {
    IdleContext,
    IdleModel,
    Manual,
    Shutdown,
    Capacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReleaseOutcome {
    Released,
    Failed,
    BackendLost,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ModelManagerStatus {
    pub queue_depth: usize,
    pub active_scope: Option<InferenceHostJobScope>,
    pub resident_models: usize,
    pub resident_contexts: usize,
    pub next_residency_deadline_after_ms: Option<u64>,
    pub resident_model_digests: Vec<String>,
    pub resident_model_digests_truncated: bool,
    pub last_release_reason: Option<ModelReleaseReason>,
    pub last_release_outcome: Option<ModelReleaseOutcome>,
    pub automatic_context_unloads: u64,
    pub automatic_model_unloads: u64,
    pub manual_unloads: u64,
    pub unload_failures: u64,
    pub model_loads: u64,
    pub context_loads: u64,
    pub model_evictions: u64,
    pub context_evictions: u64,
    pub completed_jobs: u64,
    pub incomplete_jobs: u64,
    pub cancellations: u64,
    pub deadline_exceeded: u64,
    pub failures: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceAdmissionDetails {
    pub selected_profile_id: String,
    pub context_tokens: u32,
    pub model_key: String,
    pub context_key: String,
    pub required_bytes: u64,
    pub available_bytes: u64,
    pub reserved_bytes: u64,
    pub pressure_bytes: u64,
    pub reserve_bytes: u64,
    pub fallback_allowed: bool,
    pub model_load_started: bool,
    pub tool_effect_started: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelManagerError {
    QueueFull {
        capacity: usize,
    },
    InvalidUnloadTarget {
        message: String,
    },
    DeadlineExceeded,
    Cancelled,
    ResourceAdmission {
        code: String,
        message: String,
        details: Option<Box<ResourceAdmissionDetails>>,
    },
    GenerationFailed {
        message: String,
    },
    ManagerUnavailable,
}

impl ModelManagerError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::QueueFull { .. } | Self::ManagerUnavailable)
    }

    pub fn code(&self) -> &str {
        match self {
            Self::QueueFull { .. } => "host.queue_full",
            Self::InvalidUnloadTarget { .. } => "host.invalid_unload_target",
            Self::DeadlineExceeded => "host.deadline_exceeded",
            Self::Cancelled => "host.cancelled",
            Self::ResourceAdmission { code, .. } => code,
            Self::GenerationFailed { .. } => "host.generation_failed",
            Self::ManagerUnavailable => "host.unavailable",
        }
    }

    pub fn resource_admission_details(&self) -> Option<&ResourceAdmissionDetails> {
        match self {
            Self::ResourceAdmission { details, .. } => details.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for ModelManagerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull { capacity } => {
                write!(formatter, "inference queue is full ({capacity})")
            }
            Self::InvalidUnloadTarget { message } => formatter.write_str(message),
            Self::DeadlineExceeded => formatter.write_str("inference deadline exceeded"),
            Self::Cancelled => formatter.write_str("inference cancelled"),
            Self::ResourceAdmission { code, message, .. } => {
                write!(formatter, "inference admission failed ({code}): {message}")
            }
            Self::GenerationFailed { message } => formatter.write_str(message),
            Self::ManagerUnavailable => formatter.write_str("inference host is unavailable"),
        }
    }
}

impl std::error::Error for ModelManagerError {}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug)]
pub struct EngineRuntimeStatusHandle {
    inner: Arc<Mutex<EngineRuntimeStatus>>,
}

impl EngineRuntimeStatusHandle {
    pub fn snapshot(&self) -> EngineRuntimeStatus {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Default for EngineRuntimeStatusHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EngineRuntimeStatus {
                engine_protocol_id: ENGINE_PROTOCOL_ID.to_owned(),
                phase: crate::worker_supervisor::WorkerLifecyclePhase::Cold,
                engine_pid: None,
                launch_generation: None,
                physical_device_id: None,
                reserved_bytes: 0,
                cooldown_not_before_unix_ms: None,
            })),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineRuntimeStatus {
    engine_protocol_id: String,
    phase: crate::worker_supervisor::WorkerLifecyclePhase,
    engine_pid: Option<u32>,
    launch_generation: Option<u64>,
    physical_device_id: Option<String>,
    reserved_bytes: u64,
    cooldown_not_before_unix_ms: Option<u64>,
}

impl EngineRuntimeStatus {
    pub fn engine_protocol_id(&self) -> &str {
        &self.engine_protocol_id
    }
    pub fn phase(&self) -> crate::worker_supervisor::WorkerLifecyclePhase {
        self.phase
    }
    pub fn engine_pid(&self) -> Option<u32> {
        self.engine_pid
    }
    pub fn launch_generation(&self) -> Option<u64> {
        self.launch_generation
    }
    pub fn physical_device_id(&self) -> Option<&str> {
        self.physical_device_id.as_deref()
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
    pub fn cooldown_not_before_unix_ms(&self) -> Option<u64> {
        self.cooldown_not_before_unix_ms
    }
}
