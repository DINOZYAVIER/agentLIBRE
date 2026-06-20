use agl_events::{AgentEvent, RuntimeEventWriter};
use agl_loop::{
    AgentLoopHost, ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse,
    TurnTransitionRecord,
};
use anyhow::{Result, bail};

use crate::session::InferenceSession;

pub(crate) struct CliLoopHost {
    session: InferenceSession,
    event_sink: RuntimeEventWriter,
    generated_requests: usize,
}

impl CliLoopHost {
    pub(crate) fn new(session: InferenceSession) -> Self {
        let event_sink = RuntimeEventWriter::new(session.event_stream_path());
        Self {
            session,
            event_sink,
            generated_requests: 0,
        }
    }

    pub(crate) fn session(&self) -> &InferenceSession {
        &self.session
    }

    pub(crate) fn session_mut(&mut self) -> &mut InferenceSession {
        &mut self.session
    }

    pub(crate) fn event_sink_path(&self) -> &std::path::Path {
        self.event_sink.path()
    }

    pub(crate) fn reset_turn_counters(&mut self) {
        self.generated_requests = 0;
    }

    pub(crate) fn generated_requests(&self) -> usize {
        self.generated_requests
    }
}

impl AgentLoopHost for CliLoopHost {
    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse> {
        self.generated_requests += 1;
        let response = self.session.generate(request)?;
        Ok(ModelResponse {
            content: response.content,
        })
    }

    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse> {
        bail!(
            "tool dispatch is not implemented in the CLI alpha; model requested hidden or unavailable tool `{}`",
            request.name
        )
    }

    fn emit_transition(&mut self, record: &TurnTransitionRecord, event: &AgentEvent) -> Result<()> {
        self.event_sink.append_safe_runtime_event(
            event,
            "turn",
            record.transition.as_str(),
            record.sequence,
            record.from.as_str(),
            record.to.as_str(),
        )
    }
}
