use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelConfig, ToolCallFormat, load_local_inference_config};
use agl_inference::evidence::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};
use agl_inference::{InferenceBackend, InferenceRequest, InferenceResponse, LlamaCppBackend};
use agl_memory::{MemoryEntry, MemoryRepository, MemoryScope, MemorySearchQuery};
use agl_oven::render_model_request;
use agl_runtime::{
    AgentLibreRuntimeConfig, RenderedRuntimeCapabilityContext, RuntimeCapabilityRenderOptions,
    render_runtime_capability_context,
};
use agl_skills::{
    SkillContextEvidence, SkillSource, build_verified_context_bundle, trusted_workspace_registry,
};
use agl_store::{AglStore, default_store_root};
use agl_tools::{HookEvent, HookId, SkillId, ToolCapability, ToolCatalog, ToolId};
use agl_turn::{ModelRequest, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result, bail, ensure};

use crate::{InferenceOptions, ToolAccessMode};

const CONFIG_ENV: &str = "AGL_LOCAL_INFERENCE_CONFIG";
const ARTIFACT_ROOT_ENV: &str = "AGL_INFERENCE_ARTIFACT_ROOT";
const MEMORY_CONTEXT_ENTRY_LIMIT: usize = 8;

pub struct InferenceSession {
    backend: LlamaCppBackend,
    model_config: ModelConfig,
    system_prompt: Option<String>,
    runtime_capability_context: Option<String>,
    runtime_capability_evidence: Option<agl_runtime::RuntimeCapabilityContextEvidence>,
    memory_context: Option<String>,
    skill_context: Option<String>,
    skill_hook_batches: Vec<TurnHookBatch>,
    visible_tools: Vec<VisibleTool>,
    tool_mode: ToolAccessMode,
    store_root: PathBuf,
    run_id: InferenceRunId,
    config_path: PathBuf,
    artifact_root: PathBuf,
}

impl InferenceSession {
    pub fn new(
        options: InferenceOptions,
        runtime: &AgentLibreRuntimeConfig,
        artifact_root_override: Option<PathBuf>,
    ) -> Result<Self> {
        let config_path = Self::resolve_config_path(&options, runtime);
        let artifact_root = artifact_root_override
            .or(options
                .artifact_root
                .clone()
                .or_else(|| env::var_os(ARTIFACT_ROOT_ENV).map(PathBuf::from)))
            .unwrap_or_else(|| Self::default_artifact_root(runtime));
        let store_root = default_store_root(&runtime.paths);

        tracing::info!(
            target: "agentlibre::app",
            config_path = %config_path.display(),
            artifact_root = %artifact_root.display(),
            "resolved inference session paths"
        );

        if !config_path.is_file() {
            bail!(
                "local inference config not found: {}\nCreate this file or pass --config PATH.\nRun `agl config paths` to see default locations.\nModel setup/download commands are planned but not implemented in this alpha; point [backend].model at an existing local GGUF file.",
                config_path.display()
            );
        }

        let config = load_local_inference_config(&config_path).with_context(|| {
            format!(
                "failed to load local inference config {}",
                config_path.display()
            )
        })?;
        let model_config = config.model.clone();
        let system_prompt = crate::prompt::resolve_system_prompt(config.prompt.system);
        let run_id = InferenceRunId::new(options.run_id.clone().unwrap_or_else(default_run_id))?;
        let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
        let tool_mode = options.tool_mode;
        let trust_store_path = runtime.paths.state_dir.join("skill-trust.toml");
        let skill_context = resolve_skill_context(
            &config.prompt.skills,
            &options.skills,
            tool_mode,
            &artifact_root,
            &run_id,
            &workspace_root,
            &trust_store_path,
        )?;
        let runtime_capabilities = build_runtime_capability_context(
            &workspace_root,
            tool_mode,
            &skill_context.visible_tools,
        );
        let memory_context = resolve_memory_context(MemoryContextRequest {
            enabled: options.memory,
            config_skills: &config.prompt.skills,
            option_skills: &options.skills,
            workspace_root: &workspace_root,
            trust_store_path: &trust_store_path,
            artifact_root: &artifact_root,
            run_id: &run_id,
            runtime,
        })?;
        let backend = LlamaCppBackend::new(config, InferenceArtifactRoot::new(&artifact_root))?
            .with_max_output_tokens(options.max_output_tokens);

        Ok(Self {
            backend,
            model_config,
            system_prompt,
            runtime_capability_context: Some(runtime_capabilities.content),
            runtime_capability_evidence: Some(runtime_capabilities.evidence),
            memory_context,
            skill_context: skill_context.context,
            skill_hook_batches: skill_context.hook_batches,
            visible_tools: skill_context.visible_tools,
            tool_mode,
            store_root,
            run_id,
            config_path,
            artifact_root,
        })
    }

    pub fn resolve_config_path(
        options: &InferenceOptions,
        runtime: &AgentLibreRuntimeConfig,
    ) -> PathBuf {
        options
            .config
            .clone()
            .or_else(|| env::var_os(CONFIG_ENV).map(PathBuf::from))
            .unwrap_or_else(|| runtime.paths.default_local_inference_config())
    }

    pub fn resolve_artifact_root(options: &InferenceOptions) -> Option<PathBuf> {
        options
            .artifact_root
            .clone()
            .or_else(|| env::var_os(ARTIFACT_ROOT_ENV).map(PathBuf::from))
    }

    pub fn default_artifact_root(runtime: &AgentLibreRuntimeConfig) -> PathBuf {
        runtime.paths.default_artifact_root()
    }

    pub fn run_id(&self) -> &InferenceRunId {
        &self.run_id
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn artifact_root(&self) -> &std::path::Path {
        &self.artifact_root
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.backend_name()
    }

    pub fn event_stream_path(&self) -> PathBuf {
        agent_event_stream_path(&self.artifact_root, &self.run_id)
    }

    pub fn turn_hook_batches(&self) -> &[TurnHookBatch] {
        &self.skill_hook_batches
    }

    pub fn turn_visible_tools(&self) -> &[VisibleTool] {
        &self.visible_tools
    }

    pub fn store_root(&self) -> &std::path::Path {
        &self.store_root
    }

    pub(crate) fn generate(&mut self, request: ModelRequest) -> Result<InferenceResponse> {
        if let Some(evidence) = &self.runtime_capability_evidence {
            write_runtime_capability_context_evidence(&self.artifact_root, &self.run_id, evidence)?;
        }
        let request = build_inference_request(
            self.run_id.clone(),
            request,
            &self.model_config,
            self.system_prompt.as_deref(),
            self.runtime_capability_context.as_deref(),
            self.memory_context.as_deref(),
            self.skill_context.as_deref(),
        )?;
        self.backend.generate(request)
    }

    pub fn clear_context(&mut self) {
        self.backend.clear_context();
    }

    pub fn set_runtime_capability_workspace_root(
        &mut self,
        workspace_root: &std::path::Path,
    ) -> Result<()> {
        let runtime_capabilities =
            build_runtime_capability_context(workspace_root, self.tool_mode, &self.visible_tools);
        self.runtime_capability_context = Some(runtime_capabilities.content);
        self.runtime_capability_evidence = Some(runtime_capabilities.evidence);
        Ok(())
    }
}

fn agent_event_stream_path(artifact_root: &std::path::Path, run_id: &InferenceRunId) -> PathBuf {
    InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("agent-events.jsonl")
}

fn build_inference_request(
    run_id: InferenceRunId,
    request: ModelRequest,
    model_config: &ModelConfig,
    system_prompt: Option<&str>,
    runtime_capability_context: Option<&str>,
    memory_context: Option<&str>,
    skill_context: Option<&str>,
) -> Result<InferenceRequest> {
    let request_index = request.request_index;
    let mut request_messages = Vec::with_capacity(
        request.messages.len()
            + usize::from(
                system_prompt
                    .map(|prompt| !prompt.trim().is_empty())
                    .unwrap_or(false),
            )
            + usize::from(
                runtime_capability_context
                    .map(|context| !context.trim().is_empty())
                    .unwrap_or(false),
            )
            + usize::from(
                memory_context
                    .map(|context| !context.trim().is_empty())
                    .unwrap_or(false),
            )
            + usize::from(
                skill_context
                    .map(|context| !context.trim().is_empty())
                    .unwrap_or(false),
            ),
    );
    if let Some(system_prompt) = system_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        request_messages.push(TurnMessage::System {
            content: system_prompt.to_string(),
        });
    }
    if let Some(runtime_capability_context) =
        runtime_capability_context.filter(|context| !context.trim().is_empty())
    {
        request_messages.push(TurnMessage::System {
            content: runtime_capability_context.to_string(),
        });
    }
    if let Some(memory_context) = memory_context.filter(|context| !context.trim().is_empty()) {
        request_messages.push(TurnMessage::System {
            content: memory_context.to_string(),
        });
    }
    if let Some(skill_context) = skill_context.filter(|context| !context.trim().is_empty()) {
        request_messages.push(TurnMessage::System {
            content: skill_context.to_string(),
        });
    }
    if !request.visible_tools.is_empty() {
        ensure!(
            model_config.tool_call_format == ToolCallFormat::HermesJson,
            "visible CLI tools currently require tool_call_format=hermes_json"
        );
        request_messages.push(TurnMessage::System {
            content: render_tool_context(&request.visible_tools),
        });
    }
    request_messages.extend(request.messages);

    let model_request = ModelRequest {
        turn_id: request.turn_id,
        request_index,
        messages: request_messages,
        visible_tools: request.visible_tools,
    };
    let rendered = render_model_request(&model_request, model_config)?;
    Ok(InferenceRequest {
        run_id,
        attempt_id: InferenceAttemptId::new(format!("attempt-{request_index:04}"))?,
        rendered,
    })
}

fn build_runtime_capability_context(
    workspace_root: &std::path::Path,
    tool_mode: ToolAccessMode,
    visible_tools: &[VisibleTool],
) -> RenderedRuntimeCapabilityContext {
    let available_model_tools = visible_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    render_runtime_capability_context(RuntimeCapabilityRenderOptions {
        version: env!("CARGO_PKG_VERSION"),
        workspace_root: Some(workspace_root),
        tool_mode: tool_mode.as_str(),
        available_model_tools: &available_model_tools,
        char_cap: agl_runtime::DEFAULT_RUNTIME_CAPABILITY_CONTEXT_CHAR_CAP,
    })
}

fn render_tool_context(tools: &[VisibleTool]) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_tool_context>\n");
    content.push_str(
        "You may call exactly one available tool by responding with only this Hermes JSON form:\n",
    );
    content.push_str(
        "<tool_call>{\"name\":\"TOOL_NAME\",\"arguments\":{\"arg\":\"value\"}}</tool_call>\n",
    );
    content.push_str("Use only the listed tools. Do not use markdown around tool calls.\n");
    content.push_str("\nAvailable tools:\n");
    for tool in tools {
        content.push_str("- ");
        content.push_str(&tool.name);
        if !tool.description.trim().is_empty() {
            content.push_str(": ");
            content.push_str(tool.description.trim());
        }
        if !tool.required_arguments.is_empty() {
            content.push_str(" Required arguments: ");
            content.push_str(&tool.required_arguments.join(", "));
            content.push('.');
        }
        content.push('\n');
    }
    content.push_str("</agentlibre_tool_context>\n");
    content
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResolvedSkillContext {
    context: Option<String>,
    hook_batches: Vec<TurnHookBatch>,
    visible_tools: Vec<VisibleTool>,
}

struct MemoryContextRequest<'a> {
    enabled: bool,
    config_skills: &'a [String],
    option_skills: &'a [String],
    workspace_root: &'a std::path::Path,
    trust_store_path: &'a std::path::Path,
    artifact_root: &'a std::path::Path,
    run_id: &'a InferenceRunId,
    runtime: &'a AgentLibreRuntimeConfig,
}

fn resolve_memory_context(request: MemoryContextRequest<'_>) -> Result<Option<String>> {
    if !request.enabled {
        return Ok(None);
    }
    ensure_memory_context_allowed_for_skills(
        request.config_skills,
        request.option_skills,
        request.workspace_root,
        request.trust_store_path,
    )?;
    let store =
        AglStore::open_default(&request.runtime.paths).context("failed to open memory store")?;
    let memory = MemoryRepository::new(&store);
    let mut query = MemorySearchQuery::scoped(MemoryScope::user());
    query.limit = MEMORY_CONTEXT_ENTRY_LIMIT;
    let entries = memory
        .list(&query)
        .context("failed to load memory context")?;
    if entries.is_empty() {
        return Ok(None);
    }
    write_memory_context_evidence(request.artifact_root, request.run_id, &entries)?;
    Ok(Some(render_memory_context(&entries)))
}

fn ensure_memory_context_allowed_for_skills(
    config_skills: &[String],
    option_skills: &[String],
    workspace_root: &std::path::Path,
    trust_store_path: &std::path::Path,
) -> Result<()> {
    let selected_skills = selected_skill_ids(config_skills, option_skills)?;
    if selected_skills.is_empty() {
        return Ok(());
    }
    let skill_registry = trusted_workspace_registry(workspace_root, trust_store_path)
        .context("failed to load skill registry for memory context")?;
    for skill_id in selected_skills {
        let skill = skill_registry.resolve_for_context_injection(&skill_id)?;
        if skill.harness.source == SkillSource::Workspace {
            ensure!(
                skill
                    .harness
                    .permissions
                    .memory
                    .read
                    .iter()
                    .any(|scope| scope.as_str() == "user"),
                "memory context for workspace skill `{skill_id}` requires permissions.memory.read to include user"
            );
        }
    }
    Ok(())
}

fn render_memory_context(entries: &[MemoryEntry]) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_memory>\n");
    content.push_str(
        "These are explicit local memories approved for this run. Use them only when relevant.\n",
    );
    for entry in entries {
        content.push_str("- [");
        content.push_str(entry.kind.as_str());
        content.push('/');
        content.push_str(entry.scope.kind.as_str());
        content.push_str("] ");
        content.push_str(entry.title.trim());
        content.push_str(": ");
        content.push_str(entry.body.trim());
        content.push('\n');
    }
    content.push_str("</agentlibre_memory>\n");
    content
}

fn resolve_skill_context(
    config_skills: &[String],
    option_skills: &[String],
    tool_mode: ToolAccessMode,
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    workspace_root: &std::path::Path,
    trust_store_path: &std::path::Path,
) -> Result<ResolvedSkillContext> {
    let selected_skills = selected_skill_ids(config_skills, option_skills)?;
    let skill_registry = trusted_workspace_registry(workspace_root, trust_store_path)
        .context("failed to load skill registry")?;
    let mut tool_catalog = ToolCatalog::new();
    agl_tools::guards::register(&mut tool_catalog)
        .context("failed to register builtin core guard provider")?;
    agl_tools::fs::register(&mut tool_catalog)
        .context("failed to register builtin core tool provider")?;
    agl_tools::memory::register(&mut tool_catalog)
        .context("failed to register builtin memory tool provider")?;
    agl_tools::notes::register(&mut tool_catalog)
        .context("failed to register builtin notes tool provider")?;
    let (context, hook_batches) = if selected_skills.is_empty() {
        (None, Vec::new())
    } else {
        let bundle =
            build_verified_context_bundle(&skill_registry, &tool_catalog, &selected_skills)
                .context("failed to build verified skill context")?;
        let hook_batches =
            selected_skill_hook_batches(&skill_registry, &tool_catalog, &selected_skills)?;
        write_skill_context_evidence(artifact_root, run_id, &bundle.evidence)?;
        (Some(bundle.content), hook_batches)
    };
    let visible_tools =
        selected_skill_visible_tools(&skill_registry, &tool_catalog, &selected_skills, tool_mode)?;
    Ok(ResolvedSkillContext {
        context,
        hook_batches,
        visible_tools,
    })
}

fn selected_skill_ids(config_skills: &[String], option_skills: &[String]) -> Result<Vec<SkillId>> {
    let mut selected = Vec::with_capacity(config_skills.len() + option_skills.len());
    let mut seen = std::collections::BTreeSet::new();
    for skill in config_skills.iter().chain(option_skills.iter()) {
        let id = SkillId::new(skill.clone())
            .with_context(|| format!("selected skill id is invalid: {skill}"))?;
        ensure!(
            seen.insert(id.clone()),
            "selected skill id is duplicated: {id}"
        );
        selected.push(id);
    }
    Ok(selected)
}

fn selected_skill_hook_batches(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
) -> Result<Vec<TurnHookBatch>> {
    let mut hooks_by_event: BTreeMap<HookEvent, BTreeSet<HookId>> = BTreeMap::new();
    for skill_id in selected_skills {
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        for hook_id in &skill.harness.required_hooks {
            let hook = tool_catalog.hook(hook_id).with_context(|| {
                format!("selected skill `{skill_id}` requires missing hook `{hook_id}`")
            })?;
            hooks_by_event
                .entry(hook.event)
                .or_default()
                .insert(hook_id.clone());
        }
    }

    Ok(hooks_by_event
        .into_iter()
        .map(|(event, hooks)| {
            let mut batch = TurnHookBatch::new(event);
            for hook in hooks {
                batch = batch.with_required_hook(hook);
            }
            batch
        })
        .collect())
}

fn selected_skill_visible_tools(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
) -> Result<Vec<VisibleTool>> {
    let mut tool_ids = core_tool_ids()?;
    for skill_id in selected_skills {
        skill_registry.verify_allowed_tools(skill_id, tool_catalog)?;
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        tool_ids.extend(skill.harness.allowed_tools.iter().cloned());
    }

    tool_ids
        .into_iter()
        .map(|tool_id| {
            let declaration = tool_catalog
                .executable_tool(&tool_id)
                .with_context(|| format!("selected skill requires missing tool `{tool_id}`"))?;
            if !tool_mode_allows_capability(tool_mode, declaration.capability) {
                return Ok(None);
            }
            let mut visible =
                VisibleTool::new(tool_id.as_str()).describe(declaration.description.clone());
            for argument in &declaration.required_arguments {
                visible = visible.require_argument(argument.clone());
            }
            Ok(Some(visible))
        })
        .filter_map(|result| match result {
            Ok(Some(tool)) => Some(Ok(tool)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        })
        .collect()
}

fn core_tool_ids() -> Result<BTreeSet<ToolId>> {
    [
        agl_tools::FS_READ_TOOL_ID,
        agl_tools::FS_LIST_TOOL_ID,
        agl_tools::FS_SEARCH_TOOL_ID,
        agl_tools::FS_EDIT_TOOL_ID,
    ]
    .into_iter()
    .map(ToolId::new)
    .collect::<std::result::Result<BTreeSet<_>, _>>()
    .context("builtin core tool id is invalid")
}

fn tool_mode_allows_capability(mode: ToolAccessMode, capability: ToolCapability) -> bool {
    match mode {
        ToolAccessMode::ReadOnly => capability.is_visible_in_read_only(),
        ToolAccessMode::Write => true,
    }
}

fn write_skill_context_evidence(
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    evidence: &[SkillContextEvidence],
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("skill-context.json");
    let parent = path.parent().with_context(|| {
        format!(
            "skill context evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create skill context evidence directory {}",
            parent.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(evidence).with_context(|| {
        format!(
            "failed to serialize skill context evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes)
        .with_context(|| format!("failed to write skill context evidence {}", path.display()))
}

fn write_memory_context_evidence(
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    entries: &[MemoryEntry],
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("memory-context.json");
    let parent = path.parent().with_context(|| {
        format!(
            "memory context evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create memory context evidence directory {}",
            parent.display()
        )
    })?;
    let evidence = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "scope": entry.scope.kind.as_str(),
                "scope_key": entry.scope.key,
                "kind": entry.kind.as_str(),
                "title": entry.title,
                "body_bytes": entry.body.len(),
                "source_ref": entry.source_ref,
                "confidence": entry.confidence,
            })
        })
        .collect::<Vec<_>>();
    let mut bytes = serde_json::to_vec_pretty(&evidence).with_context(|| {
        format!(
            "failed to serialize memory context evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes)
        .with_context(|| format!("failed to write memory context evidence {}", path.display()))
}

fn write_runtime_capability_context_evidence(
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    evidence: &agl_runtime::RuntimeCapabilityContextEvidence,
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("runtime-capabilities.json");
    let parent = path.parent().with_context(|| {
        format!(
            "runtime capability evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime capability evidence directory {}",
            parent.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(evidence).with_context(|| {
        format!(
            "failed to serialize runtime capability evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes).with_context(|| {
        format!(
            "failed to write runtime capability evidence {}",
            path.display()
        )
    })
}

pub fn default_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("manual-{millis}")
}

#[cfg(test)]
mod tests {
    use agl_config::{ModelDialect, ToolCallFormat};

    use super::*;

    #[test]
    fn build_request_uses_agentlibre_boundaries() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id.clone(),
            ModelRequest {
                turn_id: "manual-test".to_string(),
                request_index: 7,
                messages: vec![TurnMessage::User {
                    content: "hello".to_string(),
                }],
                visible_tools: Vec::new(),
            },
            &config,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(request.run_id, run_id);
        assert_eq!(request.attempt_id.as_str(), "attempt-0007");
        assert_eq!(request.rendered.turn_id, "manual-test");
        assert_eq!(request.rendered.request_index, 7);
        assert_eq!(request.rendered.messages.len(), 1);
        assert_eq!(request.rendered.dialect, ModelDialect::Qwen3);
        assert_eq!(
            request.rendered.tool_call_format,
            ToolCallFormat::HermesJson
        );
    }

    #[test]
    fn build_request_prepends_configured_system_prompt() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id,
            ModelRequest {
                turn_id: "manual-test".to_string(),
                request_index: 0,
                messages: vec![TurnMessage::User {
                    content: "hello".to_string(),
                }],
                visible_tools: Vec::new(),
            },
            &config,
            Some("demo system"),
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(request.rendered.messages.len(), 2);
        assert_eq!(
            request.rendered.messages[0].role,
            agl_oven::RenderedMessageRole::System
        );
        assert_eq!(request.rendered.messages[0].content, "demo system");
        assert_eq!(
            request.rendered.messages[1].role,
            agl_oven::RenderedMessageRole::User
        );
        assert_eq!(request.rendered.messages[1].content, "hello");
    }

    #[test]
    fn build_request_prepends_skill_context_after_system_prompt() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id,
            ModelRequest {
                turn_id: "manual-test".to_string(),
                request_index: 0,
                messages: vec![TurnMessage::User {
                    content: "hello".to_string(),
                }],
                visible_tools: Vec::new(),
            },
            &config,
            Some("system"),
            None,
            None,
            Some("skill context"),
        )
        .unwrap();

        assert_eq!(request.rendered.messages.len(), 3);
        assert_eq!(request.rendered.messages[0].content, "system");
        assert_eq!(request.rendered.messages[1].content, "skill context");
        assert_eq!(request.rendered.messages[2].content, "hello");
    }

    #[test]
    fn build_request_prepends_memory_context_before_skill_context() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id,
            ModelRequest {
                turn_id: "manual-test".to_string(),
                request_index: 0,
                messages: vec![TurnMessage::User {
                    content: "hello".to_string(),
                }],
                visible_tools: Vec::new(),
            },
            &config,
            Some("system"),
            None,
            Some("memory context"),
            Some("skill context"),
        )
        .unwrap();

        assert_eq!(request.rendered.messages.len(), 4);
        assert_eq!(request.rendered.messages[0].content, "system");
        assert_eq!(request.rendered.messages[1].content, "memory context");
        assert_eq!(request.rendered.messages[2].content, "skill context");
        assert_eq!(request.rendered.messages[3].content, "hello");
    }

    #[test]
    fn build_request_injects_runtime_capabilities_before_tools() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };
        let runtime_context = build_runtime_capability_context(
            std::path::Path::new("/repo"),
            ToolAccessMode::ReadOnly,
            &[
                VisibleTool::new("fs.list"),
                VisibleTool::new("fs.read"),
                VisibleTool::new("fs.search"),
            ],
        );

        let request = build_inference_request(
            run_id,
            ModelRequest {
                turn_id: "manual-test".to_string(),
                request_index: 0,
                messages: vec![TurnMessage::User {
                    content: "can you run cron jobs?".to_string(),
                }],
                visible_tools: vec![
                    VisibleTool::new("fs.list"),
                    VisibleTool::new("fs.read"),
                    VisibleTool::new("fs.search"),
                ],
            },
            &config,
            Some("system"),
            Some(&runtime_context.content),
            Some("memory context"),
            Some("skill context"),
        )
        .unwrap();

        assert_eq!(request.rendered.messages.len(), 6);
        assert_eq!(request.rendered.messages[0].content, "system");
        assert!(
            request.rendered.messages[1]
                .content
                .contains("<agentlibre_runtime_capabilities>")
        );
        assert!(request.rendered.messages[1].content.contains("- cron:"));
        assert!(
            request.rendered.messages[1]
                .content
                .contains("tool_mode: read-only")
        );
        assert!(
            request.rendered.messages[1]
                .content
                .contains("read-only: list, show, history, preflight")
        );
        assert!(
            request.rendered.messages[1]
                .content
                .contains("write: add, delete, run, tick")
        );
        assert_eq!(request.rendered.messages[2].content, "memory context");
        assert_eq!(request.rendered.messages[3].content, "skill context");
        assert!(
            request.rendered.messages[4]
                .content
                .contains("<agentlibre_tool_context>")
        );
        assert_eq!(
            request.rendered.messages[5].content,
            "can you run cron jobs?"
        );
    }

    #[test]
    fn build_request_injects_visible_tool_context_for_hermes() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id,
            ModelRequest {
                turn_id: "manual-test".to_string(),
                request_index: 0,
                messages: vec![TurnMessage::User {
                    content: "read README".to_string(),
                }],
                visible_tools: vec![
                    VisibleTool::new("fs.read")
                        .describe("Read a repository file")
                        .require_argument("path"),
                ],
            },
            &config,
            Some("system"),
            None,
            None,
            Some("skill context"),
        )
        .unwrap();

        assert_eq!(request.rendered.messages.len(), 4);
        assert_eq!(request.rendered.messages[0].content, "system");
        assert_eq!(request.rendered.messages[1].content, "skill context");
        assert!(request.rendered.messages[2].content.contains("fs.read"));
        assert!(request.rendered.messages[2].content.contains("<tool_call>"));
        assert_eq!(request.rendered.messages[3].content, "read README");
        assert_eq!(request.rendered.tools[0].name, "fs.read");
    }

    #[test]
    fn selected_skill_ids_rejects_duplicates_across_config_and_cli() {
        let err =
            selected_skill_ids(&["task-spec".to_string()], &["task-spec".to_string()]).unwrap_err();

        assert!(err.to_string().contains("selected skill id is duplicated"));
    }

    #[test]
    fn selected_skill_hook_batches_use_declared_hook_events() {
        let skill_registry = agl_skills::builtin_registry().unwrap();
        let mut extension_registry = ToolCatalog::new();
        agl_tools::guards::register(&mut extension_registry).unwrap();
        agl_tools::fs::register(&mut extension_registry).unwrap();

        let batches = selected_skill_hook_batches(
            &skill_registry,
            &extension_registry,
            &[SkillId::new("task-spec").unwrap()],
        )
        .unwrap();

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].event, HookEvent::ArtifactWrite);
        assert_eq!(
            batches[0]
                .required_hooks
                .iter()
                .map(HookId::as_str)
                .collect::<Vec<_>>(),
            vec!["repo_path.validate", "task_spec.validate"]
        );
        assert!(batches[0].optional_hooks.is_empty());
    }

    #[test]
    fn selected_skill_visible_tools_use_declared_tool_metadata() {
        let skill_registry = agl_skills::builtin_registry().unwrap();
        let mut extension_registry = ToolCatalog::new();
        agl_tools::guards::register(&mut extension_registry).unwrap();
        agl_tools::fs::register(&mut extension_registry).unwrap();

        let tools = selected_skill_visible_tools(
            &skill_registry,
            &extension_registry,
            &[SkillId::new("task-spec").unwrap()],
            ToolAccessMode::Write,
        )
        .unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.edit", "fs.list", "fs.read", "fs.search"]
        );
        assert_eq!(
            tools[0].required_arguments,
            vec!["path", "old_text", "new_text"]
        );
        assert!(tools[0].description.contains("exact text"));
    }

    #[test]
    fn visible_tools_include_read_only_core_tools_without_skills() {
        let skill_registry = agl_skills::builtin_registry().unwrap();
        let mut extension_registry = ToolCatalog::new();
        agl_tools::guards::register(&mut extension_registry).unwrap();
        agl_tools::fs::register(&mut extension_registry).unwrap();

        let tools = selected_skill_visible_tools(
            &skill_registry,
            &extension_registry,
            &[],
            ToolAccessMode::ReadOnly,
        )
        .unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.list", "fs.read", "fs.search"]
        );
    }

    #[test]
    fn visible_tools_include_edit_in_write_mode_without_skills() {
        let skill_registry = agl_skills::builtin_registry().unwrap();
        let mut extension_registry = ToolCatalog::new();
        agl_tools::guards::register(&mut extension_registry).unwrap();
        agl_tools::fs::register(&mut extension_registry).unwrap();

        let tools = selected_skill_visible_tools(
            &skill_registry,
            &extension_registry,
            &[],
            ToolAccessMode::Write,
        )
        .unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.edit", "fs.list", "fs.read", "fs.search"]
        );
    }

    #[test]
    fn selected_skill_visible_tools_hide_write_tools_in_read_only_mode() {
        let skill_registry = agl_skills::builtin_registry().unwrap();
        let mut extension_registry = ToolCatalog::new();
        agl_tools::guards::register(&mut extension_registry).unwrap();
        agl_tools::fs::register(&mut extension_registry).unwrap();

        let tools = selected_skill_visible_tools(
            &skill_registry,
            &extension_registry,
            &[SkillId::new("task-spec").unwrap()],
            ToolAccessMode::ReadOnly,
        )
        .unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.list", "fs.read", "fs.search"]
        );
    }

    #[test]
    fn selected_tool_smoke_skill_uses_read_only_core_tool_set() {
        let skill_registry = agl_skills::builtin_registry().unwrap();
        let mut extension_registry = ToolCatalog::new();
        agl_tools::guards::register(&mut extension_registry).unwrap();
        agl_tools::fs::register(&mut extension_registry).unwrap();

        let tools = selected_skill_visible_tools(
            &skill_registry,
            &extension_registry,
            &[SkillId::new("tool-smoke").unwrap()],
            ToolAccessMode::ReadOnly,
        )
        .unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.list", "fs.read", "fs.search"]
        );
    }

    #[test]
    fn resolves_default_paths_from_runtime_config() {
        let runtime = AgentLibreRuntimeConfig {
            paths: agl_runtime::AgentLibrePaths::from_agl_home("/tmp/agl-home"),
            logging: agl_runtime::AgentLibreLoggingConfig::default(),
            history: agl_runtime::AgentLibreHistoryConfig::default(),
            workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
        };
        let options = InferenceOptions::default();

        assert_eq!(
            InferenceSession::resolve_config_path(&options, &runtime),
            PathBuf::from("/tmp/agl-home/config/inference/local.toml")
        );
        assert_eq!(
            InferenceSession::default_artifact_root(&runtime),
            PathBuf::from("/tmp/agl-home/data/runs")
        );
    }

    #[test]
    fn agent_event_stream_is_separate_from_inference_evidence_events() {
        let run_id = InferenceRunId::new("run-001").unwrap();

        assert_eq!(
            agent_event_stream_path(std::path::Path::new("/tmp/artifacts"), &run_id),
            PathBuf::from("/tmp/artifacts/inference-runs/run-001/agent-events.jsonl")
        );
    }
}
