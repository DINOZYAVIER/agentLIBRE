mod envelope;
mod payload;
mod semantic;
mod taxonomy;
mod writer;

pub use envelope::{
    EVENT_SCHEMA, EnvelopeValidationError, EventDraft, EventEnvelope, EventScope,
    EventScopeBuilder, EventScopeError,
};
pub use payload::{
    HookResultEvent, JsonMetadata, ObservedEffectEvent, RuntimeEvent, RuntimeEventEnvelope,
    SafeRuntimeEvent, SafeRuntimeEventEnvelope, ToolExclusionEvent,
};
pub use semantic::{
    SEMANTIC_TRACE_SCHEMA, SemanticContentRef, SemanticDrift, SemanticReplayReport, SemanticTrace,
    SemanticTraceError, SemanticTraceIdentity, export_semantic_trace, replay_semantic_trace,
};
pub use taxonomy::{
    HookBatchOutcomeEvent, IncompleteOutputReasonEvent, InferenceFinishStatus, ParsedActionEvent,
    SafeParsedActionEvent, StopReasonEvent, ToolJsonMalformedKind, TurnFinishStatus,
};
pub use writer::{EventAppender, RuntimeEventWriter};

#[cfg(test)]
mod tests;
