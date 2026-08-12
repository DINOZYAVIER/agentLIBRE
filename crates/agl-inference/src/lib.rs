pub mod admission;
mod attempt_fsm;
mod attempt_journal;
mod backend;
mod control;
mod device;
#[cfg(target_os = "linux")]
pub mod device_lease;
#[cfg(target_os = "linux")]
pub mod durable_health;
mod engine;
pub mod evidence;
mod host;
mod output;
pub mod worker_supervisor;

pub use attempt_fsm::{
    InferenceAdmissionEvidence, InferenceAttemptCancellation, InferenceAttemptFailure,
    InferenceAttemptMachine, InferenceAttemptOutcome, InferenceAttemptPhase,
    InferenceAttemptTransition, InferenceAttemptTransitionError, InferenceAttemptTransitionRecord,
    InferenceContentEvidence, InferenceDispatchEvidence, InferenceGenerationEvidence,
    InferencePlanEvidence, InferencePlanRejectionEvidence, InferenceRejectionStage,
    InferenceRuntimeEvidence,
};
pub use attempt_journal::{AttemptJournal, AttemptJournalError, AttemptJournalReplay};
pub use backend::{
    InferenceFinishReason, InferenceRequest, InferenceResponse, InferenceResponseMetadata,
};
pub use control::{
    ENGINE_BINARY_NAME, ENGINE_PROTOCOL_ID, EngineRuntimeStatus, EngineRuntimeStatusHandle,
    InferenceCancellation, InferenceHostJobScope, ModelManagerError, ModelManagerStatus,
    ModelManagerStatusDetail, ModelReleaseOutcome, ModelReleaseReason, ModelUnloadOutcome,
    ModelUnloadResult, ModelUnloadTarget, ResourceAdmissionDetails,
};
pub use device::{
    HostCapabilityProjectionError, InferenceDeviceInfo, InferenceDeviceKind,
    project_host_capabilities,
};
#[cfg(target_os = "linux")]
pub use device_lease::{
    DeviceAuthorityLease, DeviceAuthorityLeaseError, DeviceLeaseSecurityObject,
    PhysicalDeviceLeaseIdentity,
};
pub use host::{
    ArtifactFileHandle, DescriptorSetError, EngineDeviceRuntimeIdentity, EngineExecutable,
    EngineInventory, InferenceFailure, InferenceHost, InferenceHostConfig, InferenceHostStartError,
    InferenceHostStatus, InferenceQueueRejection, LiveAdmissionRejection, ResolvedMediaAttachment,
    ResourcePools, ResourceRequest, ResourceReservation, VolatileHandles,
};
pub use output::{
    InferenceOutputEvent, InferenceOutputSink, InferenceProductStage, InferenceProgressUnit,
    InferenceStageAuthority, InferenceStageError, InferenceStageEvent, InferenceStageValidator,
    NoopInferenceOutputSink, OutputDelivery,
};
