mod envelope;
mod payload;
mod taxonomy;
mod writer;

pub use envelope::{
    EVENT_SCHEMA, EnvelopeValidationError, EventDraft, EventEnvelope, EventScope,
    EventScopeBuilder, EventScopeError,
};
pub use payload::{
    HookResultEvent, JsonMetadata, RuntimeEvent, RuntimeEventEnvelope, SafeRuntimeEvent,
    SafeRuntimeEventEnvelope,
};
pub use taxonomy::{
    HookBatchOutcomeEvent, InferenceFinishStatus, ParsedActionEvent, StopReasonEvent,
    ToolJsonMalformedKind, TurnFinishStatus,
};
pub use writer::{EventAppender, RuntimeEventWriter};

#[cfg(test)]
mod tests;
