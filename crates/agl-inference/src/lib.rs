mod attempt_fsm;
mod backend;
pub mod evidence;
mod llama_cpp;
mod model_manager;

pub use attempt_fsm::{
    InferenceAttemptMachine, InferenceAttemptPhase, InferenceAttemptTransition,
    InferenceAttemptTransitionError, InferenceAttemptTransitionRecord,
};
pub use backend::{
    InferenceFinishReason, InferenceRequest, InferenceResponse, InferenceResponseMetadata,
};
pub use llama_cpp::LlamaCppModelRuntime;
pub use model_manager::{
    ContextKey, DEFAULT_IDLE_CONTEXT_RETENTION, DEFAULT_MAX_CONTEXTS_PER_MODEL,
    DEFAULT_MAX_LOADED_MODELS, DEFAULT_MODEL_MANAGER_QUEUE_CAPACITY, InferenceCancellation,
    InferenceJob, InferenceJobScope, ModelGeneration, ModelKey, ModelManager, ModelManagerError,
    ModelManagerHandle, ModelManagerOptions, ModelManagerStatus, ModelRuntime, RuntimeFailure,
    RuntimeOperation,
};

#[cfg(test)]
mod tests;
