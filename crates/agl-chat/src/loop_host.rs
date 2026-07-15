use std::path::Path;

use agl_events::{AgentEvent, RuntimeEventWriter};
use agl_loop::{
    AgentLoopHost, ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse,
    TurnMessage, TurnTransitionRecord, VisibleTool,
};
use agl_tools::{
    HookBatchRequest, HookBatchResult, HookInput, HookMessage, HookResult, HookStatus, ToolId,
    ToolInput, ToolRuntime,
};
use anyhow::{Context, Result};

use crate::session::InferenceSession;
use crate::tools::{ChatToolRuntimeConfig, chat_tool_runtime};

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
        let mut tool_runtime = chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools: &core_tools,
            store_root: session.store_root(),
            trust_store_path: session.trust_store_path(),
            workspace_root: workspace_root.as_ref(),
            permission_status: permission_runtime_status(&session),
        })?;
        tool_runtime.set_allowed_tools(visible_tool_ids(session.turn_visible_tools())?);
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
        self.session
            .set_runtime_capability_workspace_root(workspace_root.as_ref())?;
        self.session.refresh_runtime_context()?;
        let mut tool_runtime = chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools: &core_tools,
            store_root: self.session.store_root(),
            trust_store_path: self.session.trust_store_path(),
            workspace_root: workspace_root.as_ref(),
            permission_status: permission_runtime_status(&self.session),
        })?;
        tool_runtime.set_allowed_tools(visible_tool_ids(self.session.turn_visible_tools())?);
        self.core_tools = core_tools;
        self.tool_runtime = tool_runtime;
        Ok(())
    }

    pub fn refresh_runtime_context(&mut self) -> Result<()> {
        self.session.refresh_runtime_context()?;
        let mut tool_runtime = chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools: &self.core_tools,
            store_root: self.session.store_root(),
            trust_store_path: self.session.trust_store_path(),
            workspace_root: self.core_tools.root(),
            permission_status: permission_runtime_status(&self.session),
        })?;
        tool_runtime.set_allowed_tools(visible_tool_ids(self.session.turn_visible_tools())?);
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

fn permission_runtime_status(
    session: &crate::InferenceSession,
) -> agl_tools::PermissionRuntimeStatus {
    agl_tools::PermissionRuntimeStatus {
        current_mode: session.tool_mode().as_str().to_string(),
        visible_tools: session
            .turn_visible_tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect(),
        dynamic_grants: true,
        granted_visible_tools: session.permission_grants().granted_visible_tools(),
        ignored_grants: session.permission_grants().ignored_grants(),
    }
}

fn visible_tool_ids(visible_tools: &[VisibleTool]) -> Result<Vec<ToolId>> {
    visible_tools
        .iter()
        .map(|tool| {
            ToolId::new(tool.name.clone())
                .with_context(|| format!("visible tool id is invalid: {}", tool.name))
        })
        .collect()
}
