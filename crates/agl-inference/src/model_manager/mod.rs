mod evidence;
mod runtime;
mod types;
mod worker;

pub use runtime::{ModelGeneration, RuntimeFailure, RuntimeOperation};
pub use types::{
    ContextKey, DEFAULT_IDLE_CONTEXT_RETENTION, DEFAULT_MAX_CONTEXTS_PER_MODEL,
    DEFAULT_MAX_LOADED_MODELS, DEFAULT_MODEL_MANAGER_QUEUE_CAPACITY, InferenceCancellation,
    InferenceJob, InferenceJobScope, ModelKey, ModelManagerError, ModelManagerOptions,
    ModelManagerStatus, ResolvedContentPart, ResolvedMessageContent, ResolvedModelContent,
};
pub use worker::{ModelManager, ModelManagerHandle, ModelRuntime};

#[cfg(test)]
mod tests;
