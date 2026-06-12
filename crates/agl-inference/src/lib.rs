mod backend;
mod fake;

pub use backend::{InferenceBackend, InferenceFinishReason, InferenceRequest, InferenceResponse};
pub use fake::FakeInferenceBackend;

#[cfg(test)]
mod tests;
