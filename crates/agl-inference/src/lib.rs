mod backend;
pub mod evidence;
mod llama_cpp;

pub use backend::{InferenceBackend, InferenceFinishReason, InferenceRequest, InferenceResponse};
pub use llama_cpp::LlamaCppBackend;

#[cfg(test)]
mod tests;
