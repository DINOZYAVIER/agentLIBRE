use agl_events::{AgentEvent, RuntimeEventWriter};
use agl_extension::{
    HookBatchRequest, HookBatchResult, HookInput, HookMessage, HookResult, HookStatus,
    StaticExtension,
};
use agl_loop::{
    AgentLoopHost, ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse,
    TurnTransitionRecord,
};
use anyhow::{Context, Result};

use crate::session::InferenceSession;

pub(crate) struct CliLoopHost {
    session: InferenceSession,
    event_sink: RuntimeEventWriter,
    core_guards: agl_core_guards::CoreGuards,
    core_tools: agl_core_tools::CoreTools,
    generated_requests: usize,
}

impl CliLoopHost {
    pub(crate) fn new(session: InferenceSession) -> Result<Self> {
        let event_sink = RuntimeEventWriter::new(session.event_stream_path());
        let core_tools = agl_core_tools::CoreTools::new(std::env::current_dir()?)
            .context("failed to initialize core filesystem tools")?;
        Ok(Self {
            session,
            event_sink,
            core_guards: agl_core_guards::CoreGuards::new(),
            core_tools,
            generated_requests: 0,
        })
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
    fn run_hooks(&mut self, request: HookBatchRequest) -> Result<HookBatchResult> {
        let results = request
            .hooks
            .iter()
            .map(|hook_id| {
                if self
                    .core_guards
                    .declaration()
                    .hooks
                    .iter()
                    .any(|hook| hook.id == *hook_id)
                {
                    self.core_guards.run_hook(HookInput {
                        hook_id: hook_id.clone(),
                        event: request.event,
                        payload: request.payload.clone(),
                    })
                } else {
                    missing_hook_result(hook_id.clone())
                }
            })
            .collect();
        Ok(HookBatchResult {
            event: request.event,
            results,
        })
    }

    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse> {
        self.generated_requests += 1;
        let response = self.session.generate(request)?;
        Ok(ModelResponse {
            content: response.content,
        })
    }

    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse> {
        let observation = self
            .core_tools
            .dispatch(&request.name, request.arguments)
            .with_context(|| format!("core tool `{}` failed", request.name))?;
        Ok(ToolDispatchResponse { observation })
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

fn missing_hook_result(hook_id: agl_extension::HookId) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Fail,
        messages: vec![HookMessage {
            code: "cli_hook.missing".to_string(),
            message: "hook is not available in the CLI host".to_string(),
            fix: None,
        }],
    }
}
