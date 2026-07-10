use agl_events::RuntimeEvent;
use agl_tools::{HookBatchRequest, HookBatchResult};
use agl_turn::{
    ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse, TurnMessage,
    TurnTransitionRecord,
};
use anyhow::Result;

pub trait AgentLoopHost {
    fn run_hooks(&mut self, request: HookBatchRequest) -> Result<HookBatchResult> {
        Ok(HookBatchResult {
            event: request.event,
            results: Vec::new(),
        })
    }

    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse>;
    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse>;
    fn record_turn_messages(&mut self, _messages: &[TurnMessage]) -> Result<()> {
        Ok(())
    }
    fn emit_transition(
        &mut self,
        record: &TurnTransitionRecord,
        event: &RuntimeEvent,
    ) -> Result<()>;
}
