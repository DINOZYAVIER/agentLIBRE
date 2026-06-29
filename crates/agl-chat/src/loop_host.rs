use std::path::Path;

use agl_events::{AgentEvent, RuntimeEventWriter};
use agl_loop::{
    AgentLoopHost, ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse,
    TurnMessage, TurnTransitionRecord,
};
use agl_tools::{
    HookBatchRequest, HookBatchResult, HookInput, HookMessage, HookResult, HookStatus, ToolId,
    ToolInput, ToolRuntime,
};
use anyhow::{Context, Result};

use crate::session::InferenceSession;

pub struct ChatLoopHost {
    session: InferenceSession,
    event_sink: RuntimeEventWriter,
    core_guards: agl_tools::guards::CoreGuards,
    core_tools: agl_tools::CoreTools,
    tool_runtime: ToolRuntime,
    generated_requests: usize,
    turn_messages: Vec<TurnMessage>,
}

impl ChatLoopHost {
    pub fn new(session: InferenceSession, workspace_root: impl AsRef<Path>) -> Result<Self> {
        let event_sink = RuntimeEventWriter::new(session.event_stream_path());
        let core_tools = agl_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to initialize core filesystem tools")?;
        let tool_runtime = core_tool_runtime(&core_tools, session.store_root())?;
        Ok(Self {
            session,
            event_sink,
            core_guards: agl_tools::guards::CoreGuards::new(),
            core_tools,
            tool_runtime,
            generated_requests: 0,
            turn_messages: Vec::new(),
        })
    }

    pub fn session(&self) -> &InferenceSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut InferenceSession {
        &mut self.session
    }

    pub fn event_sink_path(&self) -> &std::path::Path {
        self.event_sink.path()
    }

    pub fn reset_turn_counters(&mut self) {
        self.generated_requests = 0;
        self.turn_messages.clear();
    }

    pub fn generated_requests(&self) -> usize {
        self.generated_requests
    }

    pub fn take_turn_messages(&mut self) -> Vec<TurnMessage> {
        std::mem::take(&mut self.turn_messages)
    }

    pub fn workspace_root(&self) -> &Path {
        self.core_tools.root()
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        let core_tools = agl_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to update core filesystem tool root")?;
        let tool_runtime = core_tool_runtime(&core_tools, self.session.store_root())?;
        self.core_tools = core_tools;
        self.tool_runtime = tool_runtime;
        Ok(())
    }
}

impl AgentLoopHost for ChatLoopHost {
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
        let tool_id = ToolId::new(request.name.clone())
            .with_context(|| format!("tool id is invalid: {}", request.name))?;
        let output = self
            .tool_runtime
            .dispatch(ToolInput {
                id: tool_id,
                arguments: request.arguments,
            })
            .with_context(|| format!("tool `{}` failed", request.name))?;
        Ok(ToolDispatchResponse {
            observation: output.observation,
        })
    }

    fn record_turn_messages(&mut self, messages: &[TurnMessage]) -> Result<()> {
        self.turn_messages = messages.to_vec();
        Ok(())
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

fn missing_hook_result(hook_id: agl_tools::HookId) -> HookResult {
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

fn core_tool_runtime(core_tools: &agl_tools::CoreTools, store_root: &Path) -> Result<ToolRuntime> {
    let mut runtime = ToolRuntime::new();
    runtime
        .register_provider(agl_tools::fs::declaration())
        .context("failed to register core filesystem tool provider")?;
    runtime
        .register_provider(agl_tools::notes::declaration())
        .context("failed to register builtin notes tool provider")?;
    for tool_id in [
        agl_tools::FS_READ_TOOL_ID,
        agl_tools::FS_LIST_TOOL_ID,
        agl_tools::FS_SEARCH_TOOL_ID,
        agl_tools::FS_EDIT_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, core_tools.clone())
            .with_context(|| {
                format!("failed to register core filesystem tool handler {tool_id}")
            })?;
    }
    let notes_tools = agl_tools::NotesTools::new(store_root);
    for tool_id in [
        agl_tools::NOTES_ADD_TOOL_ID,
        agl_tools::NOTES_SEARCH_TOOL_ID,
        agl_tools::NOTES_SHOW_TOOL_ID,
        agl_tools::NOTES_UPDATE_TOOL_ID,
        agl_tools::NOTES_LINK_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, notes_tools.clone())
            .with_context(|| format!("failed to register builtin notes tool handler {tool_id}"))?;
    }
    Ok(runtime)
}
