mod artifact;
mod event;

pub use agl_events::InferenceFinishStatus;
pub use artifact::{InferenceArtifactPaths, InferenceArtifactRoot};
pub use event::InferenceEventWriter;

#[cfg(test)]
mod tests;
