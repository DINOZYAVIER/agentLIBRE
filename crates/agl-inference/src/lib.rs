mod backend;
pub mod evidence;
mod fake;
mod llama_cpp;

pub use backend::{InferenceBackend, InferenceFinishReason, InferenceRequest, InferenceResponse};
pub use fake::FakeInferenceBackend;
pub use llama_cpp::LlamaCppCliBackend;

#[cfg(test)]
mod tests;
