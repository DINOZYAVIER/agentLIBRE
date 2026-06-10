use agl_events::AgentEvent;
use anyhow::Result;

use crate::model::{ModelRequest, ModelResponse};
use crate::tool::{ToolDispatchRequest, ToolDispatchResponse};

pub trait AgentLoopHost {
    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse>;
    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse>;
    fn emit_event(&mut self, event: AgentEvent) -> Result<()>;
}
