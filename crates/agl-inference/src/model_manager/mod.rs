mod evidence;
mod queue;
mod runtime;
mod types;
mod worker;

pub use runtime::{
    ModelGeneration, ResourceAdmissionDetails, RuntimeFailure, RuntimeFailureKind, RuntimeOperation,
};
#[cfg(test)]
pub(crate) use types::WorkerJobPayload;
pub use types::{
    ContextKey, DEFAULT_CONTEXT_IDLE_DURATION, DEFAULT_MAX_CONTEXTS_PER_MODEL,
    DEFAULT_MAX_LOADED_MODELS, DEFAULT_MODEL_IDLE_DURATION, DEFAULT_MODEL_MANAGER_QUEUE_CAPACITY,
    InferenceCancellation, InferenceJob, InferenceJobScope, MAX_STATUS_MODEL_DIGESTS, ModelKey,
    ModelManagerError, ModelManagerOptions, ModelManagerStatus, ModelManagerStatusDetail,
    ModelReleaseOutcome, ModelReleaseReason, ModelUnloadOutcome, ModelUnloadResult,
    ModelUnloadTarget, ResolvedContentPart, ResolvedMessageContent, ResolvedModelContent,
};
pub use worker::{ModelManager, ModelManagerHandle, ModelRuntime};

#[cfg(test)]
mod tests;
