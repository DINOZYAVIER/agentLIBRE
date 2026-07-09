use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{
    ModelConfig, ToolCallFormat, load_local_inference_config, load_local_inference_config_from_str,
};
use agl_functions::{
    IdentityContractMode, RuntimeFunction, RuntimeIdentityContract, resolve_runtime_function,
    resolve_runtime_function_allow_missing_profile,
};
use agl_inference::evidence::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};
use agl_inference::{InferenceBackend, InferenceRequest, InferenceResponse, LlamaCppBackend};
use agl_memory::{MemoryEntry, MemoryRepository, MemoryScope, MemorySearchQuery};
use agl_oven::render_model_request;
use agl_runtime::{
    AgentLibreRuntimeConfig, RenderedRuntimeCapabilityContext, RuntimeCapabilityRenderOptions,
    render_runtime_capability_context,
};
use agl_skills::{
    SkillContextEvidence, SkillFolderCreateSituation, SkillFolderPrepareOptions,
    SkillFolderPrepareReport, build_verified_context_bundle,
    prepare_workspace_skill_artifact_write, prepare_workspace_skill_folders,
    trusted_workspace_registry,
};
use agl_store::{AglStore, PermissionGrantRecord};
use agl_tools::{HookEvent, HookId, SkillId, ToolCatalog, ToolId};
use agl_turn::{ModelRequest, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

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
    function_ref: Option<String>,
    function_profile_required: bool,
    runtime_function: Option<RuntimeFunction>,
    function_context: Option<String>,
    function_skills: Vec<String>,
    runtime_identity: Option<RuntimeIdentityEvidence>,
    identity_contract: Option<RuntimeIdentityContract>,
    skill_context: Option<String>,
    skill_hook_batches: Vec<TurnHookBatch>,
    visible_tools: Vec<VisibleTool>,
    permission_grants: RuntimePermissionGrantSnapshot,
    tool_mode: ToolAccessMode,
    store_root: PathBuf,
    config_dir: PathBuf,
    workspace_root: PathBuf,
    trust_store_path: PathBuf,
    config_skills: Vec<String>,
    option_skills: Vec<String>,
    selected_skills: Vec<SkillId>,
    run_id: InferenceRunId,
    config_path: PathBuf,
    artifact_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct RuntimeIdentityEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    function: Option<RuntimeIdentityFunction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_profile: Option<String>,
    skills: Vec<String>,
    subagents: Vec<String>,
    workspace_root: PathBuf,
    tool_mode: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RuntimeIdentityFunction {
    id: String,
    source: String,
    path: PathBuf,
}

impl InferenceSession {
    pub fn new(
        options: InferenceOptions,
        runtime: &AgentLibreRuntimeConfig,
        artifact_root_override: Option<PathBuf>,
    ) -> Result<Self> {
        let artifact_root = artifact_root_override
            .or(options
                .artifact_root
                .clone()
                .or_else(|| env::var_os(ARTIFACT_ROOT_ENV).map(PathBuf::from)))
            .unwrap_or_else(|| Self::default_artifact_root(runtime));
        let store_root = runtime.paths.store_root();
        let config_dir = runtime.paths.config_dir.clone();
        let run_id = InferenceRunId::new(options.run_id.clone().unwrap_or_else(default_run_id))?;
        let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
        let function_profile_required =
            options.config.is_none() && env::var_os(CONFIG_ENV).is_none();
        let runtime_function = resolve_session_function(
            options.function_ref.as_deref(),
            &workspace_root,
            &runtime.paths.config_dir,
            function_profile_required,
        )?;
        let function_config_path = runtime_function
            .as_ref()
            .and_then(|function| function.inference_config_path.as_deref());
        let function_embedded_config = runtime_function
            .as_ref()
            .and_then(|function| function.inference_config_toml.as_deref());
        let use_function_embedded_config = options.config.is_none()
            && env::var_os(CONFIG_ENV).is_none()
            && function_embedded_config.is_some();
        let config_path = Self::resolve_config_path(&options, runtime, function_config_path);

        tracing::info!(
            target: "agentlibre::app",
            config_path = %config_path.display(),
            artifact_root = %artifact_root.display(),
            "resolved inference session paths"
        );

        if !use_function_embedded_config && !config_path.is_file() {
            bail!(
                "local inference config not found: {}\nCreate this file or pass --config PATH.\nRun `agl config paths` to see default locations.\nModel setup/download commands are planned but not implemented in this alpha; point [backend].model at an existing local GGUF file.",
                config_path.display()
            );
        }

        let config = if use_function_embedded_config {
            load_local_inference_config_from_str(
                &config_path.display().to_string(),
                function_embedded_config.expect("checked above"),
            )
            .with_context(|| {
                format!(
                    "failed to load function inference config {}",
                    config_path.display()
                )
            })?
        } else {
            load_local_inference_config(&config_path).with_context(|| {
                format!(
                    "failed to load local inference config {}",
                    config_path.display()
                )
            })?
        };
        let model_config = config.model.clone();
        let system_prompt = crate::prompt::resolve_system_prompt(config.prompt.system);
        let tool_mode = options.tool_mode;
        let trust_store_path = runtime.paths.state_dir.join("skill-trust.toml");
        if let Some(function) = &runtime_function {
            write_function_evidence(&artifact_root, &run_id, function)?;
        }
        let function_skills = runtime_function
            .as_ref()
            .map(|function| function.skills.clone())
            .unwrap_or_default();
        let function_context = runtime_function
            .as_ref()
            .map(|function| function.context.clone());
        let skill_context = resolve_skill_context(SkillContextRequest {
            config_skills: &config.prompt.skills,
            function_skills: &function_skills,
            option_skills: &options.skills,
            tool_mode,
            artifact_root: &artifact_root,
            run_id: &run_id,
            workspace_root: &workspace_root,
            trust_store_path: &trust_store_path,
            store_root: &store_root,
        })?;
        let runtime_identity = runtime_function.as_ref().map(|function| {
            build_runtime_identity(
                function,
                &skill_context.selected_skills,
                &workspace_root,
                tool_mode,
            )
        });
        let identity_contract = effective_identity_contract(runtime_function.as_ref());
        if runtime_identity.is_some() || identity_contract.is_some() {
            write_identity_evidence(
                &artifact_root,
                &run_id,
                runtime_identity.as_ref(),
                identity_contract.as_ref(),
            )?;
        }
        let mut hook_batches = skill_context.hook_batches;
        add_identity_hook_batch(&mut hook_batches, identity_contract.as_ref())?;
        let runtime_capabilities = build_runtime_capability_context(
            &workspace_root,
            tool_mode,
            &skill_context.visible_tools,
        );
        let config_skills = config.prompt.skills.clone();
        let option_skills = options.skills.clone();
        let memory_context = resolve_memory_context(MemoryContextRequest {
            enabled: options.memory,
            config_skills: &config.prompt.skills,
            function_skills: &function_skills,
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
            function_ref: options.function_ref,
            function_profile_required,
            runtime_function,
            function_context,
            function_skills,
            runtime_identity,
            identity_contract,
            skill_context: skill_context.context,
            skill_hook_batches: hook_batches,
            visible_tools: skill_context.visible_tools,
            permission_grants: skill_context.permission_grants,
            tool_mode,
            store_root,
            config_dir,
            workspace_root,
            trust_store_path,
            config_skills,
            option_skills,
            selected_skills: skill_context.selected_skills,
            run_id,
            config_path,
            artifact_root,
        })
    }

    pub fn resolve_config_path(
        options: &InferenceOptions,
        runtime: &AgentLibreRuntimeConfig,
        function_config_path: Option<&std::path::Path>,
    ) -> PathBuf {
        options
            .config
            .clone()
            .or_else(|| env::var_os(CONFIG_ENV).map(PathBuf::from))
            .or_else(|| function_config_path.map(Path::to_path_buf))
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

    pub fn turn_hook_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::Map::new();
        if let Some(identity) = &self.runtime_identity {
            payload.insert(
                "runtime_identity".to_string(),
                serde_json::to_value(identity).expect("runtime identity serializes"),
            );
        }
        if let Some(contract) = &self.identity_contract {
            payload.insert(
                "identity_contract".to_string(),
                serde_json::to_value(contract).expect("identity contract serializes"),
            );
        }
        serde_json::Value::Object(payload)
    }

    pub fn max_hook_repair_attempts(&self) -> usize {
        self.identity_contract
            .as_ref()
            .filter(|contract| contract.repair)
            .map(|contract| contract.max_repair_attempts as usize)
            .unwrap_or(0)
    }

    pub fn turn_visible_tools(&self) -> &[VisibleTool] {
        &self.visible_tools
    }

    pub(crate) fn permission_grants(&self) -> &RuntimePermissionGrantSnapshot {
        &self.permission_grants
    }

    pub fn tool_mode(&self) -> ToolAccessMode {
        self.tool_mode
    }

    pub fn store_root(&self) -> &std::path::Path {
        &self.store_root
    }

    pub(crate) fn trust_store_path(&self) -> &std::path::Path {
        &self.trust_store_path
    }

    pub(crate) fn prepare_artifact_write_for_tool(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<()> {
        let Some(relative) =
            artifact_write_preflight_path_for_tool(tool_name, &self.selected_skills, arguments)?
        else {
            return Ok(());
        };
        let report = prepare_workspace_skill_artifact_write(
            &self.workspace_root,
            &self.trust_store_path,
            &self.selected_skills,
            &relative,
            &SkillFolderPrepareOptions {
                dry_run: false,
                situation: SkillFolderCreateSituation::ArtifactWrite,
                strict: true,
            },
        )
        .context("failed to prepare selected skill artifact-write folders")?;
        if !report.actions.is_empty() || report.has_errors() {
            write_skill_folder_prepare_evidence(
                &self.artifact_root,
                &self.run_id,
                "artifact-write",
                &report,
            )?;
        }
        ensure!(
            !report.has_errors(),
            "selected skill artifact-write preparation failed: {}",
            report.errors.join(", ")
        );
        Ok(())
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
            self.function_context.as_deref(),
            self.memory_context.as_deref(),
            self.skill_context.as_deref(),
        )?;
        self.backend.generate(request)
    }

    pub fn clear_context(&mut self) {
        self.backend.clear_context();
    }

    pub(crate) fn set_workspace_root_and_refresh(
        &mut self,
        workspace_root: &std::path::Path,
    ) -> Result<()> {
        self.workspace_root = workspace_root.to_path_buf();
        self.refresh_runtime_context()
    }

    pub(crate) fn refresh_runtime_context(&mut self) -> Result<()> {
        if let Some(reference) = &self.function_ref {
            let function = resolve_session_function(
                Some(reference),
                &self.workspace_root,
                &self.config_dir,
                self.function_profile_required,
            )?
            .expect("function ref is set");
            write_function_evidence(&self.artifact_root, &self.run_id, &function)?;
            self.function_context = Some(function.context.clone());
            self.function_skills = function.skills.clone();
            self.runtime_function = Some(function);
        }
        let skill_context = resolve_skill_context(SkillContextRequest {
            config_skills: &self.config_skills,
            function_skills: &self.function_skills,
            option_skills: &self.option_skills,
            tool_mode: self.tool_mode,
            artifact_root: &self.artifact_root,
            run_id: &self.run_id,
            workspace_root: &self.workspace_root,
            trust_store_path: &self.trust_store_path,
            store_root: &self.store_root,
        })?;
        self.runtime_identity = self.runtime_function.as_ref().map(|function| {
            build_runtime_identity(
                function,
                &skill_context.selected_skills,
                &self.workspace_root,
                self.tool_mode,
            )
        });
        self.identity_contract = effective_identity_contract(self.runtime_function.as_ref());
        if self.runtime_identity.is_some() || self.identity_contract.is_some() {
            write_identity_evidence(
                &self.artifact_root,
                &self.run_id,
                self.runtime_identity.as_ref(),
                self.identity_contract.as_ref(),
            )?;
        }
        self.skill_context = skill_context.context;
        let mut hook_batches = skill_context.hook_batches;
        add_identity_hook_batch(&mut hook_batches, self.identity_contract.as_ref())?;
        self.skill_hook_batches = hook_batches;
        self.visible_tools = skill_context.visible_tools;
        self.permission_grants = skill_context.permission_grants;
        self.selected_skills = skill_context.selected_skills;
        let runtime_capabilities = build_runtime_capability_context(
            &self.workspace_root,
            self.tool_mode,
            &self.visible_tools,
        );
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

fn resolve_session_function(
    reference: Option<&str>,
    workspace_root: &Path,
    config_dir: &Path,
    require_profile: bool,
) -> Result<Option<RuntimeFunction>> {
    reference
        .map(|reference| {
            if require_profile {
                resolve_runtime_function(reference, workspace_root, config_dir)
            } else {
                resolve_runtime_function_allow_missing_profile(
                    reference,
                    workspace_root,
                    config_dir,
                )
            }
            .with_context(|| format!("failed to resolve function `{reference}`"))
        })
        .transpose()
}

fn write_function_evidence(
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    function: &RuntimeFunction,
) -> Result<()> {
    let run_dir = InferenceArtifactRoot::new(artifact_root.to_path_buf()).run_dir(run_id);
    std::fs::create_dir_all(&run_dir).with_context(|| {
        format!(
            "failed to create function evidence directory {}",
            run_dir.display()
        )
    })?;

    let resolution_path = run_dir.join("function-resolution.json");
    let resolution_bytes = serde_json::to_vec_pretty(function).with_context(|| {
        format!(
            "failed to serialize function resolution evidence {}",
            resolution_path.display()
        )
    })?;
    std::fs::write(&resolution_path, resolution_bytes).with_context(|| {
        format!(
            "failed to write function resolution evidence {}",
            resolution_path.display()
        )
    })?;

    let context_path = run_dir.join("function-context.md");
    std::fs::write(&context_path, function.context.as_bytes()).with_context(|| {
        format!(
            "failed to write function context evidence {}",
            context_path.display()
        )
    })?;

    let registry_path = run_dir.join("subagent-registry.json");
    let registry_bytes = serde_json::to_vec_pretty(&function.subagents).with_context(|| {
        format!(
            "failed to serialize subagent registry evidence {}",
            registry_path.display()
        )
    })?;
    std::fs::write(&registry_path, registry_bytes).with_context(|| {
        format!(
            "failed to write subagent registry evidence {}",
            registry_path.display()
        )
    })?;

    Ok(())
}

fn build_runtime_identity(
    function: &RuntimeFunction,
    selected_skills: &[SkillId],
    workspace_root: &Path,
    tool_mode: ToolAccessMode,
) -> RuntimeIdentityEvidence {
    RuntimeIdentityEvidence {
        function: Some(RuntimeIdentityFunction {
            id: function.id.clone(),
            source: function.source.as_str().to_string(),
            path: function.path.clone(),
        }),
        model_profile: function.model_profile.clone(),
        skills: selected_skills
            .iter()
            .map(|skill| skill.as_str().to_string())
            .collect(),
        subagents: function
            .subagents
            .iter()
            .map(|subagent| subagent.id.clone())
            .collect(),
        workspace_root: workspace_root.to_path_buf(),
        tool_mode: tool_mode.as_str().to_string(),
    }
}

fn effective_identity_contract(
    function: Option<&RuntimeFunction>,
) -> Option<RuntimeIdentityContract> {
    let function = function?;
    let contract = function
        .identity_contract
        .clone()
        .unwrap_or_else(RuntimeIdentityContract::function_default);
    contract.is_enabled().then_some(contract)
}

fn add_identity_hook_batch(
    hook_batches: &mut Vec<TurnHookBatch>,
    contract: Option<&RuntimeIdentityContract>,
) -> Result<()> {
    let Some(contract) = contract.filter(|contract| contract.is_enabled()) else {
        return Ok(());
    };
    let hook_id = match contract.mode {
        IdentityContractMode::Off => return Ok(()),
        IdentityContractMode::ValidateClaims => {
            agl_tools::guards::RUNTIME_IDENTITY_VALIDATE_HOOK_ID
        }
        IdentityContractMode::Require => agl_tools::guards::RUNTIME_IDENTITY_REQUIRE_HOOK_ID,
    };
    let hook_id = HookId::new(hook_id)?;
    if let Some(batch) = hook_batches
        .iter_mut()
        .find(|batch| batch.event == HookEvent::ArtifactWrite)
    {
        if !batch.required_hooks.iter().any(|hook| hook == &hook_id) {
            batch.required_hooks.push(hook_id);
        }
    } else {
        hook_batches.push(TurnHookBatch::new(HookEvent::ArtifactWrite).with_required_hook(hook_id));
    }
    Ok(())
}

fn write_identity_evidence(
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    identity: Option<&RuntimeIdentityEvidence>,
    contract: Option<&RuntimeIdentityContract>,
) -> Result<()> {
    let run_dir = InferenceArtifactRoot::new(artifact_root.to_path_buf()).run_dir(run_id);
    std::fs::create_dir_all(&run_dir).with_context(|| {
        format!(
            "failed to create runtime identity evidence directory {}",
            run_dir.display()
        )
    })?;
    if let Some(identity) = identity {
        let path = run_dir.join("runtime-identity.json");
        let bytes = serde_json::to_vec_pretty(identity)
            .with_context(|| format!("failed to serialize runtime identity {}", path.display()))?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write runtime identity {}", path.display()))?;
    }
    if let Some(contract) = contract {
        let path = run_dir.join("identity-contract.json");
        let bytes = serde_json::to_vec_pretty(contract)
            .with_context(|| format!("failed to serialize identity contract {}", path.display()))?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write identity contract {}", path.display()))?;
    }
    Ok(())
}

fn build_inference_request(
    run_id: InferenceRunId,
    request: ModelRequest,
    model_config: &ModelConfig,
    system_prompt: Option<&str>,
    runtime_capability_context: Option<&str>,
    function_context: Option<&str>,
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
                function_context
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
    if let Some(function_context) = function_context.filter(|context| !context.trim().is_empty()) {
        request_messages.push(TurnMessage::System {
            content: function_context.to_string(),
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
        request_messages.push(TurnMessage::System {
            content: render_tool_context(&request.visible_tools, model_config.tool_call_format)?,
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

fn render_tool_context(tools: &[VisibleTool], format: ToolCallFormat) -> Result<String> {
    match format {
        ToolCallFormat::HermesJson => Ok(render_hermes_tool_context(tools)),
        ToolCallFormat::GemmaFunctionCall => Ok(render_gemma_tool_context(tools)),
        ToolCallFormat::StructuredToolCalls => {
            bail!("visible CLI tools are not supported for structured tool-call rendering")
        }
    }
}

fn render_hermes_tool_context(tools: &[VisibleTool]) -> String {
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

fn render_gemma_tool_context(tools: &[VisibleTool]) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_tool_context>\n");
    content.push_str("# GEMMA NATIVE TOOL CALLING\n\n");
    content.push_str("For Gemma 4 models, use the native Gemma tool-call syntax only.\n\n");
    content.push_str("Rules:\n");
    content.push_str("- Do not use `<tool>`, `</tool>`, `<answer>`, `</answer>`, or Hermes `<tool_call>...</tool_call>`.\n");
    content.push_str("- Do not call an `answer` tool.\n");
    content.push_str("- To call a tool, output exactly one native block:\n");
    content.push_str("  `<|tool_call>call:TOOL_NAME{key:<|\"|>value<|\"|>,...}<tool_call|>`\n");
    content.push_str("- Use the exact tool name listed below.\n");
    content.push_str("- Wrap string argument values with `<|\"|>` delimiters. Numbers, booleans, and null are unquoted.\n");
    content.push_str("- Do not put prose before or after the native tool-call block.\n");
    content.push_str(
        "- After a tool result, either emit another native tool call or answer in plain text.\n",
    );
    content.push_str("- Final answers must be plain text only.\n");
    content.push_str(
        "- Do not emit tool-response wrappers yourself; the runtime provides tool responses.\n",
    );
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
    permission_grants: RuntimePermissionGrantSnapshot,
    selected_skills: Vec<SkillId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimePermissionGrantSnapshot {
    admitted: Vec<AdmittedPermissionGrant>,
    ignored: Vec<IgnoredPermissionGrant>,
}

impl RuntimePermissionGrantSnapshot {
    pub(crate) fn granted_visible_tools(&self) -> Vec<String> {
        self.admitted
            .iter()
            .map(|grant| grant.tool_id.clone())
            .collect()
    }

    pub(crate) fn ignored_grants(&self) -> Vec<String> {
        self.ignored
            .iter()
            .map(|grant| format!("{}:{}:{}", grant.grant_id, grant.tool_id, grant.reason))
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AdmittedPermissionGrant {
    grant_id: String,
    tool_id: String,
    max_operation_kind: String,
    duration: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IgnoredPermissionGrant {
    grant_id: String,
    tool_id: String,
    reason: String,
}

struct MemoryContextRequest<'a> {
    enabled: bool,
    config_skills: &'a [String],
    function_skills: &'a [String],
    option_skills: &'a [String],
    workspace_root: &'a std::path::Path,
    trust_store_path: &'a std::path::Path,
    artifact_root: &'a std::path::Path,
    run_id: &'a InferenceRunId,
    runtime: &'a AgentLibreRuntimeConfig,
}

struct SkillContextRequest<'a> {
    config_skills: &'a [String],
    function_skills: &'a [String],
    option_skills: &'a [String],
    tool_mode: ToolAccessMode,
    artifact_root: &'a std::path::Path,
    run_id: &'a InferenceRunId,
    workspace_root: &'a std::path::Path,
    trust_store_path: &'a std::path::Path,
    store_root: &'a std::path::Path,
}

fn resolve_memory_context(request: MemoryContextRequest<'_>) -> Result<Option<String>> {
    if !request.enabled {
        return Ok(None);
    }
    ensure_memory_context_allowed_for_skills(
        request.config_skills,
        request.function_skills,
        request.option_skills,
        request.workspace_root,
        request.trust_store_path,
    )?;
    let store = AglStore::open_at(request.runtime.paths.store_root())
        .context("failed to open memory store")?;
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
    function_skills: &[String],
    option_skills: &[String],
    workspace_root: &std::path::Path,
    trust_store_path: &std::path::Path,
) -> Result<()> {
    let selected_skills = selected_skill_ids(config_skills, function_skills, option_skills)?;
    if selected_skills.is_empty() {
        return Ok(());
    }
    let skill_registry = trusted_workspace_registry(workspace_root, trust_store_path)
        .context("failed to load skill registry for memory context")?;
    for skill_id in selected_skills {
        let skill = skill_registry.resolve_for_context_injection(&skill_id)?;
        if skill.harness.source.is_external_skill_source() {
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

fn resolve_skill_context(request: SkillContextRequest<'_>) -> Result<ResolvedSkillContext> {
    let selected_skills = selected_skill_ids(
        request.config_skills,
        request.function_skills,
        request.option_skills,
    )?;
    let skill_registry =
        trusted_workspace_registry(request.workspace_root, request.trust_store_path)
            .context("failed to load skill registry")?;
    let tool_catalog = crate::tools::chat_extension_catalog()?;
    let (context, hook_batches) = if selected_skills.is_empty() {
        (None, Vec::new())
    } else {
        let folder_prepare = prepare_workspace_skill_folders(
            request.workspace_root,
            request.trust_store_path,
            &selected_skills,
            &SkillFolderPrepareOptions {
                dry_run: false,
                situation: SkillFolderCreateSituation::RuntimePrepare,
                strict: true,
            },
        )
        .context("failed to prepare selected skill runtime folders")?;
        write_skill_folder_prepare_evidence(
            request.artifact_root,
            request.run_id,
            "runtime-prepare",
            &folder_prepare,
        )?;
        ensure!(
            !folder_prepare.has_errors(),
            "selected skill runtime folder preparation failed: {}",
            folder_prepare.errors.join(", ")
        );
        let bundle =
            build_verified_context_bundle(&skill_registry, &tool_catalog, &selected_skills)
                .context("failed to build verified skill context")?;
        let hook_batches =
            selected_skill_hook_batches(&skill_registry, &tool_catalog, &selected_skills)?;
        write_skill_context_evidence(request.artifact_root, request.run_id, &bundle.evidence)?;
        (Some(bundle.content), hook_batches)
    };
    let (visible_tools, permission_grants) = selected_skill_visible_tools_with_dynamic_grants(
        &skill_registry,
        &tool_catalog,
        &selected_skills,
        request.tool_mode,
        request.store_root,
        request.workspace_root,
        request.run_id,
    )?;
    Ok(ResolvedSkillContext {
        context,
        hook_batches,
        visible_tools,
        permission_grants,
        selected_skills,
    })
}

fn selected_skill_ids(
    config_skills: &[String],
    function_skills: &[String],
    option_skills: &[String],
) -> Result<Vec<SkillId>> {
    let mut selected =
        Vec::with_capacity(config_skills.len() + function_skills.len() + option_skills.len());
    let mut seen = std::collections::BTreeSet::new();
    for skill in config_skills
        .iter()
        .chain(function_skills.iter())
        .chain(option_skills.iter())
    {
        let id = SkillId::new(skill.clone())
            .with_context(|| format!("selected skill id is invalid: {skill}"))?;
        if seen.insert(id.clone()) {
            selected.push(id);
        }
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
            let hook = tool_catalog.trusted_hook(hook_id).with_context(|| {
                format!("selected skill `{skill_id}` requires unavailable hook `{hook_id}`")
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

#[cfg(test)]
fn selected_skill_visible_tools(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
) -> Result<Vec<VisibleTool>> {
    let (tools, _) = selected_skill_visible_tools_with_grants(
        skill_registry,
        tool_catalog,
        selected_skills,
        tool_mode,
        RuntimePermissionGrantSnapshot::default(),
    )?;
    Ok(tools)
}

fn selected_skill_visible_tools_with_dynamic_grants(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
    store_root: &std::path::Path,
    workspace_root: &std::path::Path,
    run_id: &InferenceRunId,
) -> Result<(Vec<VisibleTool>, RuntimePermissionGrantSnapshot)> {
    let grant_snapshot = admit_dynamic_permission_grants(
        skill_registry,
        tool_catalog,
        selected_skills,
        store_root,
        workspace_root,
        run_id,
    )?;
    selected_skill_visible_tools_with_grants(
        skill_registry,
        tool_catalog,
        selected_skills,
        tool_mode,
        grant_snapshot,
    )
}

fn selected_skill_visible_tools_with_grants(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
    grant_snapshot: RuntimePermissionGrantSnapshot,
) -> Result<(Vec<VisibleTool>, RuntimePermissionGrantSnapshot)> {
    let mut tool_ids = if selected_skills.is_empty() {
        core_tool_ids()?
    } else {
        BTreeSet::new()
    };
    let mut denied_tool_ids = BTreeSet::new();
    for skill_id in selected_skills {
        skill_registry.verify_allowed_tools(skill_id, tool_catalog)?;
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        tool_ids.extend(skill.harness.allowed_tools.iter().cloned());
        denied_tool_ids.extend(skill.harness.denied_tools.iter().cloned());
    }
    tool_ids.retain(|tool_id| !denied_tool_ids.contains(tool_id));
    let granted_tool_ids = grant_snapshot
        .admitted
        .iter()
        .map(|grant| ToolId::new(grant.tool_id.clone()))
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .context("admitted permission grant tool id is invalid")?;
    tool_ids.extend(granted_tool_ids.iter().cloned());

    let visible_tools = tool_ids
        .into_iter()
        .map(|tool_id| {
            let declaration = tool_catalog
                .executable_tool(&tool_id)
                .with_context(|| format!("selected skill requires missing tool `{tool_id}`"))?;
            if !granted_tool_ids.contains(&tool_id)
                && !tool_mode_allows_declaration(tool_mode, declaration)
            {
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
        .collect::<Result<Vec<_>>>()?;
    Ok((visible_tools, grant_snapshot))
}

fn admit_dynamic_permission_grants(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    store_root: &std::path::Path,
    workspace_root: &std::path::Path,
    run_id: &InferenceRunId,
) -> Result<RuntimePermissionGrantSnapshot> {
    let store = AglStore::open_at(store_root)
        .with_context(|| format!("failed to open permission store {}", store_root.display()))?;
    let grants = store.active_permission_grants()?;
    let policy = selected_skill_grant_policy(skill_registry, selected_skills)?;
    let mut snapshot = RuntimePermissionGrantSnapshot::default();

    for grant in grants {
        match evaluate_permission_grant(&grant, tool_catalog, &policy, workspace_root, run_id) {
            Ok(tool_id) => {
                if grant.duration != "one_turn" {
                    snapshot.ignored.push(IgnoredPermissionGrant {
                        grant_id: grant.id,
                        tool_id: grant.tool_id,
                        reason: format!("unsupported_duration_{}", grant.duration),
                    });
                    continue;
                }
                let admitted = store.admit_permission_grant(&grant.id, run_id.as_str())?;
                snapshot.admitted.push(AdmittedPermissionGrant {
                    grant_id: admitted.id,
                    tool_id: tool_id.as_str().to_string(),
                    max_operation_kind: admitted.max_operation_kind,
                    duration: admitted.duration,
                });
            }
            Err(reason) => snapshot.ignored.push(IgnoredPermissionGrant {
                grant_id: grant.id,
                tool_id: grant.tool_id,
                reason,
            }),
        }
    }

    Ok(snapshot)
}

#[derive(Default)]
struct SelectedSkillGrantPolicy {
    selected: BTreeSet<SkillId>,
    allowed_or_requestable: BTreeMap<SkillId, BTreeSet<ToolId>>,
    denied_tools: BTreeSet<ToolId>,
}

fn selected_skill_grant_policy(
    skill_registry: &agl_skills::SkillRegistry,
    selected_skills: &[SkillId],
) -> Result<SelectedSkillGrantPolicy> {
    let mut policy = SelectedSkillGrantPolicy::default();
    for skill_id in selected_skills {
        policy.selected.insert(skill_id.clone());
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        let mut routed = BTreeSet::new();
        routed.extend(skill.harness.allowed_tools.iter().cloned());
        routed.extend(skill.harness.requestable_tools.iter().cloned());
        policy
            .denied_tools
            .extend(skill.harness.denied_tools.iter().cloned());
        policy
            .allowed_or_requestable
            .insert(skill_id.clone(), routed);
    }
    Ok(policy)
}

fn evaluate_permission_grant(
    grant: &PermissionGrantRecord,
    tool_catalog: &ToolCatalog,
    policy: &SelectedSkillGrantPolicy,
    workspace_root: &std::path::Path,
    run_id: &InferenceRunId,
) -> std::result::Result<ToolId, String> {
    let tool_id = ToolId::new(grant.tool_id.clone()).map_err(|_| "invalid_tool_id".to_string())?;
    if let Some(workspace) = grant
        .scope
        .get("workspace_root")
        .and_then(|value| value.as_str())
        && workspace != workspace_root.display().to_string()
    {
        return Err("workspace_scope_mismatch".to_string());
    }
    if let Some(scoped_run_id) = grant.scope.get("run_id").and_then(|value| value.as_str())
        && scoped_run_id != run_id.as_str()
    {
        return Err("run_scope_mismatch".to_string());
    }
    if policy.denied_tools.contains(&tool_id) {
        return Err("denied_by_selected_skill".to_string());
    }
    if !policy.selected.is_empty()
        && !policy
            .allowed_or_requestable
            .values()
            .any(|tools| tools.contains(&tool_id))
    {
        return Err("not_routed_by_selected_skill".to_string());
    }
    if let Some(skill) = grant.scope.get("skill_id").and_then(|value| value.as_str()) {
        let skill_id =
            SkillId::new(skill.to_string()).map_err(|_| "invalid_skill_scope".to_string())?;
        if !policy.selected.contains(&skill_id) {
            return Err("skill_scope_not_selected".to_string());
        }
        if !policy
            .allowed_or_requestable
            .get(&skill_id)
            .is_some_and(|tools| tools.contains(&tool_id))
        {
            return Err("skill_scope_not_routed".to_string());
        }
    }
    let declaration = tool_catalog
        .executable_tool(&tool_id)
        .map_err(|_| "tool_unavailable".to_string())?;
    let max_operation_kind = agl_tools::ToolOperationKind::parse(&grant.max_operation_kind)
        .ok_or_else(|| "invalid_operation_kind".to_string())?;
    if !max_operation_kind.permits(declaration.operation_kind) {
        return Err("operation_ceiling_denied".to_string());
    }
    if !grant.state_effects.is_empty() {
        let granted_effects = grant.state_effects.iter().collect::<BTreeSet<_>>();
        for effect in &declaration.state_effects {
            let effect = effect.as_str().to_string();
            if !granted_effects.contains(&effect) {
                return Err("state_effect_denied".to_string());
            }
        }
    }
    Ok(tool_id)
}

fn core_tool_ids() -> Result<BTreeSet<ToolId>> {
    [
        agl_tools::FS_READ_TOOL_ID,
        agl_tools::FS_LIST_TOOL_ID,
        agl_tools::FS_SEARCH_TOOL_ID,
        agl_tools::FS_EDIT_TOOL_ID,
        agl_tools::PERMISSIONS_STATUS_TOOL_ID,
        agl_tools::PERMISSIONS_REQUEST_TOOL_ID,
        agl_tools::PERMISSIONS_GRANT_TOOL_ID,
        agl_tools::PERMISSIONS_REVOKE_TOOL_ID,
        agl_tools::SKILL_LIST_TOOL_ID,
        agl_tools::SKILL_INSPECT_TOOL_ID,
        agl_tools::SKILL_STATUS_TOOL_ID,
        agl_tools::SKILL_VERIFY_TOOL_ID,
    ]
    .into_iter()
    .map(ToolId::new)
    .collect::<std::result::Result<BTreeSet<_>, _>>()
    .context("builtin core tool id is invalid")
}

fn tool_mode_allows_declaration(
    mode: ToolAccessMode,
    declaration: &agl_tools::ToolDeclaration,
) -> bool {
    if declaration.visible_in_read_only {
        return true;
    }
    mode.operation_ceiling()
        .is_some_and(|ceiling| ceiling.permits(declaration.operation_kind))
}

fn normalize_agl_artifact_write_path(arguments: &serde_json::Value) -> Result<Option<PathBuf>> {
    let Some(raw) = arguments.get("path").and_then(|value| value.as_str()) else {
        return Ok(None);
    };
    if !raw.starts_with(".agl/") && raw != ".agl" {
        return Ok(None);
    }
    ensure!(!raw.trim().is_empty(), "repository path cannot be blank");
    ensure!(!raw.contains('\0'), "repository path contains NUL");
    ensure!(
        !raw.contains('\\'),
        "repository path must use forward slashes"
    );

    let path = std::path::Path::new(raw);
    ensure!(!path.is_absolute(), "repository path cannot be absolute");

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                ensure!(segment != ".git", "repository path cannot enter .git");
                normalized.push(segment);
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("repository path cannot contain parent traversal")
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("repository path cannot be absolute")
            }
        }
    }
    Ok(Some(normalized))
}

fn artifact_write_preflight_path_for_tool(
    tool_name: &str,
    selected_skills: &[SkillId],
    arguments: &serde_json::Value,
) -> Result<Option<PathBuf>> {
    if tool_name != agl_tools::FS_EDIT_TOOL_ID || selected_skills.is_empty() {
        return Ok(None);
    }

    normalize_agl_artifact_write_path(arguments)
}

fn write_skill_folder_prepare_evidence(
    artifact_root: &std::path::Path,
    run_id: &InferenceRunId,
    label: &str,
    report: &SkillFolderPrepareReport,
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join(format!("skill-folder-{label}.json"));
    let parent = path.parent().with_context(|| {
        format!(
            "skill folder prepare evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create skill folder prepare evidence directory {}",
            parent.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(report).with_context(|| {
        format!(
            "failed to serialize skill folder prepare evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes).with_context(|| {
        format!(
            "failed to write skill folder prepare evidence {}",
            path.display()
        )
    })
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
mod tests;
