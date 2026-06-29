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
        let mut tool_runtime = core_tool_runtime(
            &core_tools,
            session.store_root(),
            workspace_root.as_ref(),
            permission_runtime_status(&session),
        )?;
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
        let mut tool_runtime = core_tool_runtime(
            &core_tools,
            self.session.store_root(),
            workspace_root.as_ref(),
            permission_runtime_status(&self.session),
        )?;
        tool_runtime.set_allowed_tools(visible_tool_ids(self.session.turn_visible_tools())?);
        self.core_tools = core_tools;
        self.tool_runtime = tool_runtime;
        self.session
            .set_runtime_capability_workspace_root(self.core_tools.root())?;
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

fn core_tool_runtime(
    core_tools: &agl_tools::CoreTools,
    store_root: &Path,
    workspace_root: &Path,
    permission_status: agl_tools::PermissionRuntimeStatus,
) -> Result<ToolRuntime> {
    let mut runtime = ToolRuntime::new();
    runtime
        .register_provider(agl_tools::cron::declaration())
        .context("failed to register builtin cron tool provider")?;
    runtime
        .register_provider(agl_tools::fs::declaration())
        .context("failed to register core filesystem tool provider")?;
    runtime
        .register_provider(agl_tools::matrix::declaration())
        .context("failed to register builtin Matrix tool provider")?;
    runtime
        .register_provider(agl_tools::memory::declaration())
        .context("failed to register builtin memory tool provider")?;
    runtime
        .register_provider(agl_tools::notes::declaration())
        .context("failed to register builtin notes tool provider")?;
    runtime
        .register_provider(agl_tools::permissions::declaration())
        .context("failed to register builtin permission tool provider")?;
    runtime
        .register_provider(agl_tools::repo::declaration())
        .context("failed to register builtin repo tool provider")?;
    runtime
        .register_provider(agl_tools::store::declaration())
        .context("failed to register builtin store tool provider")?;
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
    let cron_tools = agl_tools::CronTools::new(store_root);
    for tool_id in [
        agl_tools::CRON_LIST_TOOL_ID,
        agl_tools::CRON_SHOW_TOOL_ID,
        agl_tools::CRON_HISTORY_TOOL_ID,
        agl_tools::CRON_PREFLIGHT_TOOL_ID,
        agl_tools::CRON_ADD_TOOL_ID,
        agl_tools::CRON_UPDATE_TOOL_ID,
        agl_tools::CRON_DELETE_TOOL_ID,
        agl_tools::CRON_ENABLE_TOOL_ID,
        agl_tools::CRON_DISABLE_TOOL_ID,
        agl_tools::CRON_RUN_TOOL_ID,
        agl_tools::CRON_TICK_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, cron_tools.clone())
            .with_context(|| format!("failed to register builtin cron tool handler {tool_id}"))?;
    }
    let matrix_tools = agl_tools::MatrixTools::new(store_root);
    for tool_id in [
        agl_tools::MATRIX_OUTBOX_STATUS_TOOL_ID,
        agl_tools::MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, matrix_tools.clone())
            .with_context(|| format!("failed to register builtin Matrix tool handler {tool_id}"))?;
    }
    let memory_tools = agl_tools::MemoryTools::new(store_root);
    for tool_id in [
        agl_tools::MEMORY_SEARCH_TOOL_ID,
        agl_tools::MEMORY_LIST_TOOL_ID,
        agl_tools::MEMORY_SUGGEST_TOOL_ID,
        agl_tools::MEMORY_ADD_TOOL_ID,
        agl_tools::MEMORY_APPROVE_TOOL_ID,
        agl_tools::MEMORY_REJECT_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, memory_tools.clone())
            .with_context(|| format!("failed to register builtin memory tool handler {tool_id}"))?;
    }
    let notes_tools = agl_tools::NotesTools::new(store_root);
    for tool_id in [
        agl_tools::NOTES_ADD_TOOL_ID,
        agl_tools::NOTES_SEARCH_TOOL_ID,
        agl_tools::NOTES_SHOW_TOOL_ID,
        agl_tools::NOTES_UPDATE_TOOL_ID,
        agl_tools::NOTES_LINK_TOOL_ID,
        agl_tools::NOTES_DELETE_TOOL_ID,
        agl_tools::NOTES_REMEMBER_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, notes_tools.clone())
            .with_context(|| format!("failed to register builtin notes tool handler {tool_id}"))?;
    }
    let permission_tools =
        agl_tools::PermissionTools::new(store_root).with_runtime_status(permission_status);
    for tool_id in [
        agl_tools::PERMISSIONS_STATUS_TOOL_ID,
        agl_tools::PERMISSIONS_REQUEST_TOOL_ID,
        agl_tools::PERMISSIONS_GRANT_TOOL_ID,
        agl_tools::PERMISSIONS_REVOKE_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, permission_tools.clone())
            .with_context(|| {
                format!("failed to register builtin permission tool handler {tool_id}")
            })?;
    }
    let repo_tools = agl_tools::RepoTools::new(workspace_root);
    for tool_id in [
        agl_tools::REPO_STATUS_TOOL_ID,
        agl_tools::REPO_EXPORT_PROFILE_TOOL_ID,
        agl_tools::REPO_HOOKS_STATUS_TOOL_ID,
        agl_tools::REPO_INIT_TOOL_ID,
        agl_tools::REPO_INSTALL_HOOKS_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, repo_tools.clone())
            .with_context(|| format!("failed to register builtin repo tool handler {tool_id}"))?;
    }
    let store_tools = agl_tools::StoreTools::new(store_root);
    for tool_id in [
        agl_tools::STORE_STATUS_TOOL_ID,
        agl_tools::STORE_EXPORT_TOOL_ID,
    ] {
        runtime
            .register_handler(ToolId::new(tool_id)?, store_tools.clone())
            .with_context(|| format!("failed to register builtin store tool handler {tool_id}"))?;
    }
    Ok(runtime)
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
        dynamic_grants: false,
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
