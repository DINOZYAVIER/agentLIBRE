mod effect;
mod event_map;
mod executor;

pub use agl_capabilities::{
    HookBatchRequest, HookBatchResult, HookEvent, HookId, HookMessage, HookResult, HookStatus,
};
pub use agl_turn::{
    HookBatchOutcome, HookBatchSummary, HookResultSummary, ModelRequest, ModelResponse, StopDetail,
    StopReason, ToolDispatchRequest, ToolDispatchResponse, TurnHookBatch, TurnInput, TurnMessage,
    TurnOutput, TurnPhase, TurnTransition, TurnTransitionRecord, VisibleTool,
};
pub use effect::{
    EffectFailure, EffectFailureCode, EffectKey, EffectOutcome, HookEffectOutput, TurnAdvance,
    TurnAdvanceState, TurnEffect, TurnEffectKind, TurnEffectResult, TurnExecutionFailure,
    TurnExecutorError, TurnTerminal,
};
pub use executor::{TURN_CHECKPOINT_SCHEMA, TurnCheckpoint, TurnExecutor};

#[cfg(test)]
mod tests;
