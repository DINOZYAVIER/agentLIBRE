pub mod admission;
mod attempt_fsm;
mod backend;
mod device;
#[cfg(target_os = "linux")]
pub mod device_lease;
#[cfg(target_os = "linux")]
pub mod durable_health;
pub mod evidence;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod gpu_profile;
mod model_manager;
mod output;
#[cfg(target_os = "linux")]
mod private_directory;
pub mod worker_protocol;
#[cfg(target_os = "linux")]
pub mod worker_resources;
#[cfg(target_os = "linux")]
mod worker_runtime;
pub mod worker_supervisor;

pub use attempt_fsm::{
    InferenceAttemptMachine, InferenceAttemptPhase, InferenceAttemptTransition,
    InferenceAttemptTransitionError, InferenceAttemptTransitionRecord,
};
pub use backend::{
    InferenceFinishReason, InferenceRequest, InferenceResponse, InferenceResponseMetadata,
};
pub use device::{InferenceDeviceInfo, InferenceDeviceKind};
#[cfg(target_os = "linux")]
pub use device_lease::{
    DeviceAuthorityLease, DeviceAuthorityLeaseError, DeviceLeaseSecurityObject,
    PhysicalDeviceLeaseIdentity,
};
pub use model_manager::{
    ContextKey, DEFAULT_CONTEXT_IDLE_DURATION, DEFAULT_MAX_CONTEXTS_PER_MODEL,
    DEFAULT_MAX_LOADED_MODELS, DEFAULT_MODEL_IDLE_DURATION, DEFAULT_MODEL_MANAGER_QUEUE_CAPACITY,
    InferenceCancellation, InferenceJob, InferenceJobScope, MAX_STATUS_MODEL_DIGESTS,
    ModelGeneration, ModelKey, ModelManager, ModelManagerError, ModelManagerHandle,
    ModelManagerOptions, ModelManagerStatus, ModelManagerStatusDetail, ModelReleaseOutcome,
    ModelReleaseReason, ModelRuntime, ModelUnloadOutcome, ModelUnloadResult, ModelUnloadTarget,
    ResolvedContentPart, ResolvedMessageContent, ResolvedModelContent, RuntimeFailure,
    RuntimeFailureKind, RuntimeOperation,
};
pub use output::{
    InferenceOutputEvent, InferenceOutputSink, InferenceProductStage, InferenceProgressUnit,
    InferenceStageAuthority, InferenceStageError, InferenceStageEvent, InferenceStageValidator,
    NoopInferenceOutputSink, OutputDelivery,
};
#[cfg(target_os = "linux")]
pub use worker_runtime::{
    RemoteContext, RemoteModel, WorkerModelRuntime, WorkerRuntimeOptions, WorkerRuntimeStatus,
    WorkerRuntimeStatusHandle,
};

#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "linux"))]
mod worker_supervision_matrix;
