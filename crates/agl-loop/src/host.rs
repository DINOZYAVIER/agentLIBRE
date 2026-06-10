use agl_events::AgentEvent;
use agl_turn::{ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse};
use anyhow::Result;

pub trait AgentLoopHost {
    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse>;
    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse>;
    fn emit_event(&mut self, event: AgentEvent) -> Result<()>;
}
