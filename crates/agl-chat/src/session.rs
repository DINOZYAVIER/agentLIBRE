use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

use agl_capabilities::{
    CapabilityGrant, CapabilityId, CapabilityPolicyInput, EffectiveCapabilitySet,
    FunctionToolPolicy, HookEvent, HookId, OperationKind, SensitiveInput, SkillCapabilityPolicy,
    SkillId, StateEffect, render_canonical_json,
};
use agl_config::{
    LocalInferenceConfig, ModelConfig, ToolCallFormat, load_local_inference_config,
    load_local_inference_config_from_str,
};
use agl_content::Content;
use agl_functions::{
    IdentityContractMode, RuntimeFunction, RuntimeIdentityContract, resolve_runtime_function,
    resolve_runtime_function_allow_missing_profile,
};
use agl_ids::{AttemptId, RequestId, RunId, SessionId};
use agl_inference::evidence::InferenceArtifactRoot;
use agl_inference::{InferenceCancellation, InferenceRequest, InferenceResponse};
use agl_memory::{MemoryEntry, MemoryRepository, MemoryScope, MemorySearchQuery};
use agl_oven::render_model_request;
use agl_runtime::{
    AgentLibreRuntimeConfig, RenderedRuntimeFeatureContext, RuntimeFeatureRenderOptions,
    render_runtime_feature_context,
};
use agl_skills::{
    SkillContextEvidence, SkillFolderCreateSituation, SkillFolderPrepareOptions,
    SkillFolderPrepareReport, build_verified_context_bundle,
    prepare_workspace_skill_artifact_write, prepare_workspace_skill_folders,
    trusted_workspace_registry,
};
use agl_store::{AglStore, PermissionGrantRecord};
use agl_tools::ToolCatalog;
use agl_turn::{ModelRequest, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::{ChatInferenceJob, InferenceClientHandle, InferenceOptions, ToolAccessMode};

const CONFIG_ENV: &str = "AGL_LOCAL_INFERENCE_CONFIG";
const ARTIFACT_ROOT_ENV: &str = "AGL_INFERENCE_ARTIFACT_ROOT";
const MEMORY_CONTEXT_ENTRY_LIMIT: usize = 8;

pub struct InferenceSession {
    inference_client: InferenceClientHandle,
    inference_config: LocalInferenceConfig,
    session_id: SessionId,
    max_output_tokens: u32,
    model_config: ModelConfig,
    system_prompt: Option<String>,
    runtime_feature_context: Option<String>,
    runtime_feature_evidence: Option<agl_runtime::RuntimeFeatureContextEvidence>,
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
    effective_capabilities: EffectiveCapabilitySet,
    permission_grants: RuntimePermissionGrantSnapshot,
    tool_mode: ToolAccessMode,
    store_root: PathBuf,
    config_dir: PathBuf,
    workspace_root: PathBuf,
    trust_store_path: PathBuf,
    config_skills: Vec<String>,
    option_skills: Vec<String>,
    selected_skills: Vec<SkillId>,
    memory_enabled: bool,
    config_path: PathBuf,
    artifact_root: PathBuf,
}

pub(crate) struct InferenceExecutionControl {
    pub cancellation: InferenceCancellation,
    pub deadline: Option<Instant>,
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
        session_id: SessionId,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        let artifact_root = artifact_root_override
            .or(options
                .artifact_root
                .clone()
                .or_else(|| env::var_os(ARTIFACT_ROOT_ENV).map(PathBuf::from)))
            .unwrap_or_else(|| Self::default_artifact_root(runtime));
        let store_root = runtime.paths.store_root();
        let config_dir = runtime.paths.config_dir.clone();
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
            function_policy: runtime_function
                .as_ref()
                .and_then(|function| function.tool_policy.as_ref()),
            tool_mode,
            artifact_root: &artifact_root,
            run_id: None,
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
        let mut hook_batches = skill_context.hook_batches;
        add_identity_hook_batch(&mut hook_batches, identity_contract.as_ref())?;
        let runtime_features =
            build_runtime_feature_context(&workspace_root, tool_mode, &skill_context.visible_tools);
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
            run_id: None,
            store_root: &store_root,
        })?;
        Ok(Self {
            inference_client,
            inference_config: config,
            session_id,
            max_output_tokens: options.max_output_tokens,
            model_config,
            system_prompt,
            runtime_feature_context: Some(runtime_features.content),
            runtime_feature_evidence: Some(runtime_features.evidence),
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
            effective_capabilities: skill_context.effective_capabilities,
            permission_grants: skill_context.permission_grants,
            tool_mode,
            store_root,
            config_dir,
            workspace_root,
            trust_store_path,
            config_skills,
            option_skills,
            selected_skills: skill_context.selected_skills,
            memory_enabled: options.memory,
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

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn artifact_root(&self) -> &std::path::Path {
        &self.artifact_root
    }

    pub fn backend_name(&self) -> &'static str {
        self.inference_config.backend.kind.as_str()
    }

    pub fn event_stream_path(&self, run_id: &RunId) -> PathBuf {
        agent_event_stream_path(&self.artifact_root, run_id)
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

    pub(crate) fn effective_capabilities(&self) -> &EffectiveCapabilitySet {
        &self.effective_capabilities
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
        run_id: &RunId,
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
                run_id,
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

    pub(crate) fn generate(
        &mut self,
        request: ModelRequest,
        attempt_id: AttemptId,
        session_id: Option<SessionId>,
        request_id: Option<RequestId>,
        effective_capabilities: &EffectiveCapabilitySet,
        control: InferenceExecutionControl,
    ) -> Result<InferenceResponse> {
        ensure!(
            session_id.as_ref() == Some(&self.session_id),
            "inference request session does not match its managed context"
        );
        if let Some(evidence) = &self.runtime_feature_evidence {
            write_runtime_feature_context_evidence(&self.artifact_root, &request.run_id, evidence)?;
        }
        let request = build_inference_request(
            request,
            attempt_id,
            &self.model_config,
            InferenceRequestContexts {
                session_id: session_id.as_ref(),
                request_id: request_id.as_ref(),
                system_prompt: self.system_prompt.as_deref(),
                runtime_feature_context: self.runtime_feature_context.as_deref(),
                function_context: self.function_context.as_deref(),
                memory_context: self.memory_context.as_deref(),
                skill_context: self.skill_context.as_deref(),
                effective_capabilities: Some(effective_capabilities),
            },
        )?;
        self.inference_client.generate(ChatInferenceJob {
            config: self.inference_config.clone(),
            artifact_root: InferenceArtifactRoot::new(self.artifact_root.clone()),
            content_store_root: self.store_root.clone(),
            max_output_tokens: self.max_output_tokens,
            session_id: self.session_id.clone(),
            request,
            cancellation: control.cancellation,
            deadline: control.deadline,
        })
    }

    pub fn clear_context(&self) -> Result<()> {
        self.inference_client
            .clear_context(&self.inference_config, &self.session_id)
            .context("failed to clear managed inference context")
    }

    pub fn release_context(&self) -> Result<()> {
        self.inference_client
            .release_context(&self.inference_config, &self.session_id)
            .context("failed to release managed inference context")
    }

    pub(crate) fn set_workspace_root_and_refresh(
        &mut self,
        workspace_root: &std::path::Path,
    ) -> Result<()> {
        self.workspace_root = workspace_root.to_path_buf();
        self.refresh_runtime_context(None)
    }

    pub(crate) fn refresh_runtime_context(&mut self, run_id: Option<&RunId>) -> Result<()> {
        if let Some(reference) = &self.function_ref {
            let function = resolve_session_function(
                Some(reference),
                &self.workspace_root,
                &self.config_dir,
                self.function_profile_required,
            )?
            .expect("function ref is set");
            self.function_context = Some(function.context.clone());
            self.function_skills = function.skills.clone();
            self.runtime_function = Some(function);
        }
        if let (Some(run_id), Some(function)) = (run_id, self.runtime_function.as_ref()) {
            write_function_evidence(&self.artifact_root, run_id, function)?;
        }
        let skill_context = resolve_skill_context(SkillContextRequest {
            config_skills: &self.config_skills,
            function_skills: &self.function_skills,
            option_skills: &self.option_skills,
            function_policy: self
                .runtime_function
                .as_ref()
                .and_then(|function| function.tool_policy.as_ref()),
            tool_mode: self.tool_mode,
            artifact_root: &self.artifact_root,
            run_id,
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
        if let Some(run_id) = run_id
            && (self.runtime_identity.is_some() || self.identity_contract.is_some())
        {
            write_identity_evidence(
                &self.artifact_root,
                run_id,
                self.runtime_identity.as_ref(),
                self.identity_contract.as_ref(),
            )?;
        }
        self.skill_context = skill_context.context;
        let mut hook_batches = skill_context.hook_batches;
        add_identity_hook_batch(&mut hook_batches, self.identity_contract.as_ref())?;
        self.skill_hook_batches = hook_batches;
        self.visible_tools = skill_context.visible_tools;
        self.effective_capabilities = skill_context.effective_capabilities;
        self.permission_grants = skill_context.permission_grants;
        self.selected_skills = skill_context.selected_skills;
        self.memory_context = resolve_memory_context(MemoryContextRequest {
            enabled: self.memory_enabled,
            config_skills: &self.config_skills,
            function_skills: &self.function_skills,
            option_skills: &self.option_skills,
            workspace_root: &self.workspace_root,
            trust_store_path: &self.trust_store_path,
            artifact_root: &self.artifact_root,
            run_id,
            store_root: &self.store_root,
        })?;
        let runtime_features = build_runtime_feature_context(
            &self.workspace_root,
            self.tool_mode,
            &self.visible_tools,
        );
        self.runtime_feature_context = Some(runtime_features.content);
        self.runtime_feature_evidence = Some(runtime_features.evidence);
        Ok(())
    }
}

fn agent_event_stream_path(artifact_root: &std::path::Path, run_id: &RunId) -> PathBuf {
    InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("events.jsonl")
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
    run_id: &RunId,
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
    run_id: &RunId,
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
    request: ModelRequest,
    attempt_id: AttemptId,
    model_config: &ModelConfig,
    contexts: InferenceRequestContexts<'_>,
) -> Result<InferenceRequest> {
    let run_id = request.run_id.clone();
    let turn_id = request.turn_id.clone();
    let request_index = request.request_index;
    let mut request_messages =
        Vec::with_capacity(request.messages.len() + contexts.non_empty_count());
    if let Some(system_prompt) = non_empty_context(contexts.system_prompt) {
        request_messages.push(TurnMessage::System {
            content: Content::text(system_prompt)?,
        });
    }
    if let Some(runtime_feature_context) = non_empty_context(contexts.runtime_feature_context) {
        request_messages.push(TurnMessage::System {
            content: Content::text(runtime_feature_context)?,
        });
    }
    if let Some(function_context) = non_empty_context(contexts.function_context) {
        request_messages.push(TurnMessage::System {
            content: Content::text(function_context)?,
        });
    }
    if let Some(memory_context) = non_empty_context(contexts.memory_context) {
        request_messages.push(TurnMessage::System {
            content: Content::text(memory_context)?,
        });
    }
    if let Some(skill_context) = non_empty_context(contexts.skill_context) {
        request_messages.push(TurnMessage::System {
            content: Content::text(skill_context)?,
        });
    }
    let effective_capabilities = contexts
        .effective_capabilities
        .context("effective capability set is missing from inference request context")?;
    ensure_visible_tool_parity(&request.visible_tools, effective_capabilities)?;
    if effective_capabilities.capabilities().len() != 0 {
        request_messages.push(TurnMessage::System {
            content: Content::text(render_tool_context(
                effective_capabilities,
                model_config.tool_call_format,
            )?)?,
        });
    }
    request_messages.extend(request.messages);

    let model_request = ModelRequest {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        request_index,
        messages: request_messages,
        visible_tools: request.visible_tools,
    };
    let rendered = render_model_request(&model_request, model_config)?;
    Ok(InferenceRequest {
        run_id,
        turn_id,
        attempt_id,
        session_id: contexts.session_id.cloned(),
        request_id: contexts.request_id.cloned(),
        rendered,
    })
}

fn build_runtime_feature_context(
    workspace_root: &std::path::Path,
    tool_mode: ToolAccessMode,
    visible_tools: &[VisibleTool],
) -> RenderedRuntimeFeatureContext {
    let available_model_tools = visible_tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<Vec<_>>();
    render_runtime_feature_context(RuntimeFeatureRenderOptions {
        version: env!("CARGO_PKG_VERSION"),
        workspace_root: Some(workspace_root),
        tool_mode: tool_mode.as_str(),
        available_model_tools: &available_model_tools,
        char_cap: agl_runtime::DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct InferenceRequestContexts<'a> {
    session_id: Option<&'a SessionId>,
    request_id: Option<&'a RequestId>,
    system_prompt: Option<&'a str>,
    runtime_feature_context: Option<&'a str>,
    function_context: Option<&'a str>,
    memory_context: Option<&'a str>,
    skill_context: Option<&'a str>,
    effective_capabilities: Option<&'a EffectiveCapabilitySet>,
}

impl InferenceRequestContexts<'_> {
    fn non_empty_count(&self) -> usize {
        [
            self.system_prompt,
            self.runtime_feature_context,
            self.function_context,
            self.memory_context,
            self.skill_context,
        ]
        .into_iter()
        .filter(|context| context.is_some_and(|content| !content.trim().is_empty()))
        .count()
    }
}

fn non_empty_context(context: Option<&str>) -> Option<&str> {
    context.filter(|content| !content.trim().is_empty())
}

fn render_tool_context(
    capabilities: &EffectiveCapabilitySet,
    format: ToolCallFormat,
) -> Result<String> {
    match format {
        ToolCallFormat::HermesJson => Ok(render_hermes_tool_context(capabilities)),
        ToolCallFormat::GemmaFunctionCall => Ok(render_gemma_tool_context(capabilities)),
        ToolCallFormat::StructuredToolCalls => {
            bail!("visible CLI tools are not supported for structured tool-call rendering")
        }
    }
}

fn render_hermes_tool_context(capabilities: &EffectiveCapabilitySet) -> String {
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
    for capability in capabilities.capabilities() {
        content.push_str(&render_action_schema(capability.declaration()));
        content.push('\n');
    }
    content.push_str("</agentlibre_tool_context>\n");
    content
}

fn render_gemma_tool_context(capabilities: &EffectiveCapabilitySet) -> String {
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
    for capability in capabilities.capabilities() {
        content.push_str(&render_action_schema(capability.declaration()));
        content.push('\n');
    }
    content.push_str("</agentlibre_tool_context>\n");
    content
}

fn render_action_schema(declaration: &agl_capabilities::ActionDeclaration) -> String {
    render_canonical_json(&serde_json::json!({
        "name": declaration.id,
        "description": declaration.description,
        "input_schema": declaration.input_schema,
    }))
}

fn ensure_visible_tool_parity(
    visible_tools: &[VisibleTool],
    capabilities: &EffectiveCapabilitySet,
) -> Result<()> {
    let visible = visible_tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<BTreeSet<_>>();
    let effective = capabilities
        .capabilities()
        .map(|capability| capability.declaration().id.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        visible == effective,
        "model-visible tools differ from the effective capability set"
    );
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSkillContext {
    context: Option<String>,
    hook_batches: Vec<TurnHookBatch>,
    visible_tools: Vec<VisibleTool>,
    effective_capabilities: EffectiveCapabilitySet,
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
            .map(|grant| grant.capability_id.as_str().to_string())
            .collect()
    }

    pub(crate) fn ignored_grants(&self) -> Vec<String> {
        self.ignored
            .iter()
            .map(|grant| format!("{}:{}:{}", grant.grant_id, grant.tool_id, grant.reason))
            .collect()
    }

    fn capability_grants(&self) -> Vec<CapabilityGrant> {
        self.admitted
            .iter()
            .map(|grant| {
                CapabilityGrant::new(grant.capability_id.clone(), grant.max_operation_kind)
                    .with_state_effects(grant.state_effects.iter().copied())
                    .with_sensitive_inputs(grant.sensitive_inputs.iter().copied())
            })
            .collect()
    }

    pub(crate) fn sensitive_input_run(
        &self,
        capability_id: &CapabilityId,
        input: SensitiveInput,
    ) -> Option<&RunId> {
        self.admitted
            .iter()
            .find(|grant| {
                &grant.capability_id == capability_id && grant.sensitive_inputs.contains(&input)
            })
            .map(|grant| &grant.run_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AdmittedPermissionGrant {
    grant_id: String,
    capability_id: CapabilityId,
    max_operation_kind: OperationKind,
    state_effects: BTreeSet<StateEffect>,
    sensitive_inputs: BTreeSet<SensitiveInput>,
    run_id: RunId,
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
    run_id: Option<&'a RunId>,
    store_root: &'a std::path::Path,
}

struct SkillContextRequest<'a> {
    config_skills: &'a [String],
    function_skills: &'a [String],
    option_skills: &'a [String],
    function_policy: Option<&'a FunctionToolPolicy>,
    tool_mode: ToolAccessMode,
    artifact_root: &'a std::path::Path,
    run_id: Option<&'a RunId>,
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
    let store = AglStore::open_at(request.store_root).context("failed to open memory store")?;
    let memory = MemoryRepository::new(&store);
    let mut query = MemorySearchQuery::scoped(MemoryScope::user());
    query.limit = MEMORY_CONTEXT_ENTRY_LIMIT;
    let entries = memory
        .list(&query)
        .context("failed to load memory context")?;
    if entries.is_empty() {
        return Ok(None);
    }
    if let Some(run_id) = request.run_id {
        write_memory_context_evidence(request.artifact_root, run_id, &entries)?;
    }
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
        if let Some(run_id) = request.run_id {
            write_skill_folder_prepare_evidence(
                request.artifact_root,
                run_id,
                "runtime-prepare",
                &folder_prepare,
            )?;
        }
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
        if let Some(run_id) = request.run_id {
            write_skill_context_evidence(request.artifact_root, run_id, &bundle.evidence)?;
        }
        (Some(bundle.content), hook_batches)
    };
    let mut permission_grants = if let Some(run_id) = request.run_id {
        admit_dynamic_permission_grants(
            &skill_registry,
            &tool_catalog,
            &selected_skills,
            request.store_root,
            request.workspace_root,
            run_id,
        )?
    } else {
        RuntimePermissionGrantSnapshot::default()
    };
    let effective_capabilities = resolve_effective_capabilities(
        &skill_registry,
        &tool_catalog,
        &selected_skills,
        request.tool_mode,
        &permission_grants,
        request.function_policy.cloned(),
    )?;
    if let Some(run_id) = request.run_id {
        finalize_permission_grants(
            request.store_root,
            run_id,
            &effective_capabilities,
            &mut permission_grants,
        )?;
        write_capability_policy_evidence(request.artifact_root, run_id, &effective_capabilities)?;
    }
    let visible_tools = visible_tools_from_effective(&effective_capabilities);
    Ok(ResolvedSkillContext {
        context,
        hook_batches,
        visible_tools,
        effective_capabilities,
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
    let effective = resolve_effective_capabilities(
        skill_registry,
        tool_catalog,
        selected_skills,
        tool_mode,
        &RuntimePermissionGrantSnapshot::default(),
        None,
    )?;
    Ok(visible_tools_from_effective(&effective))
}

#[cfg(test)]
fn selected_skill_visible_tools_with_dynamic_grants(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
    store_root: &std::path::Path,
    workspace_root: &std::path::Path,
    run_id: &RunId,
) -> Result<(Vec<VisibleTool>, RuntimePermissionGrantSnapshot)> {
    let mut grant_snapshot = admit_dynamic_permission_grants(
        skill_registry,
        tool_catalog,
        selected_skills,
        store_root,
        workspace_root,
        run_id,
    )?;
    let effective = resolve_effective_capabilities(
        skill_registry,
        tool_catalog,
        selected_skills,
        tool_mode,
        &grant_snapshot,
        None,
    )?;
    finalize_permission_grants(store_root, run_id, &effective, &mut grant_snapshot)?;
    Ok((visible_tools_from_effective(&effective), grant_snapshot))
}

fn resolve_effective_capabilities(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
    grant_snapshot: &RuntimePermissionGrantSnapshot,
    function_policy: Option<FunctionToolPolicy>,
) -> Result<EffectiveCapabilitySet> {
    let baseline = if selected_skills.is_empty() {
        core_tool_ids()?
    } else {
        BTreeSet::new()
    };
    let mut skill_policies = Vec::with_capacity(selected_skills.len());
    for skill_id in selected_skills {
        skill_registry.verify_allowed_tools(skill_id, tool_catalog)?;
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        skill_policies.push(
            SkillCapabilityPolicy::new(
                skill_id.clone(),
                skill.harness.allowed_tools.iter().cloned(),
            )
            .with_denied(skill.harness.denied_tools.iter().cloned()),
        );
    }
    let mut input = CapabilityPolicyInput::new(
        tool_catalog.providers().iter().cloned(),
        baseline,
        tool_mode,
    )
    .with_selected_skills(skill_policies)
    .with_grants(grant_snapshot.capability_grants());
    if !agl_host_tools::screen::provider_available() {
        input = input.with_unavailable_capabilities([CapabilityId::new(
            agl_host_tools::SCREEN_CAPTURE_TOOL_ID,
        )?]);
    }
    if let Some(function_policy) = function_policy {
        input = input.with_function_policy(function_policy);
    }
    input
        .resolve()
        .context("failed to resolve capability policy")
}

fn visible_tools_from_effective(effective: &EffectiveCapabilitySet) -> Vec<VisibleTool> {
    effective
        .capabilities()
        .map(|capability| VisibleTool::from_declaration(capability.declaration()))
        .collect()
}

fn admit_dynamic_permission_grants(
    skill_registry: &agl_skills::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    store_root: &std::path::Path,
    workspace_root: &std::path::Path,
    run_id: &RunId,
) -> Result<RuntimePermissionGrantSnapshot> {
    let store = AglStore::open_at(store_root)
        .with_context(|| format!("failed to open permission store {}", store_root.display()))?;
    let grants = store.active_permission_grants()?;
    let policy = selected_skill_grant_policy(skill_registry, selected_skills)?;
    let mut snapshot = RuntimePermissionGrantSnapshot::default();

    for grant in grants {
        match evaluate_permission_grant(&grant, tool_catalog, &policy, workspace_root, run_id) {
            Ok(capability_grant) => {
                if grant.duration != "one_turn" {
                    snapshot.ignored.push(IgnoredPermissionGrant {
                        grant_id: grant.id,
                        tool_id: grant.tool_id,
                        reason: format!("unsupported_duration_{}", grant.duration),
                    });
                    continue;
                }
                snapshot.admitted.push(AdmittedPermissionGrant {
                    grant_id: grant.id,
                    capability_id: capability_grant.capability_id,
                    max_operation_kind: capability_grant.max_operation_kind,
                    state_effects: capability_grant.state_effects,
                    sensitive_inputs: capability_grant.sensitive_inputs,
                    run_id: run_id.clone(),
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

fn finalize_permission_grants(
    store_root: &std::path::Path,
    run_id: &RunId,
    effective: &EffectiveCapabilitySet,
    snapshot: &mut RuntimePermissionGrantSnapshot,
) -> Result<()> {
    let store = AglStore::open_at(store_root)
        .with_context(|| format!("failed to open permission store {}", store_root.display()))?;
    let mut admitted = Vec::new();
    for grant in std::mem::take(&mut snapshot.admitted) {
        if effective.contains(&grant.capability_id) {
            store.admit_permission_grant(&grant.grant_id, run_id.as_str())?;
            admitted.push(grant);
        } else {
            let reason = effective
                .exclusion(&grant.capability_id)
                .map(|exclusion| exclusion.reason.code())
                .unwrap_or("capability_not_effective")
                .to_string();
            snapshot.ignored.push(IgnoredPermissionGrant {
                grant_id: grant.grant_id,
                tool_id: grant.capability_id.as_str().to_string(),
                reason,
            });
        }
    }
    snapshot.admitted = admitted;
    Ok(())
}

#[derive(Default)]
struct SelectedSkillGrantPolicy {
    selected: BTreeSet<SkillId>,
    allowed_or_requestable: BTreeMap<SkillId, BTreeSet<CapabilityId>>,
    denied_tools: BTreeSet<CapabilityId>,
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
    run_id: &RunId,
) -> std::result::Result<CapabilityGrant, String> {
    let capability_id = CapabilityId::new(grant.tool_id.clone())
        .map_err(|_| "invalid_capability_id".to_string())?;
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
    if policy.denied_tools.contains(&capability_id) {
        return Err("denied_by_selected_skill".to_string());
    }
    if !policy.selected.is_empty()
        && !policy
            .allowed_or_requestable
            .values()
            .any(|tools| tools.contains(&capability_id))
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
            .is_some_and(|tools| tools.contains(&capability_id))
        {
            return Err("skill_scope_not_routed".to_string());
        }
    }
    let declaration = tool_catalog
        .executable_action(&capability_id)
        .map_err(|_| "capability_unavailable".to_string())?;
    let max_operation_kind = parse_operation_kind(&grant.max_operation_kind)?;
    if !max_operation_kind.permits(declaration.operation_kind) {
        return Err("operation_ceiling_denied".to_string());
    }
    let granted_sensitive_inputs = parse_sensitive_inputs(&grant.sensitive_inputs)?;
    for input in &declaration.sensitive_inputs {
        if !granted_sensitive_inputs.contains(input) {
            return Err("sensitive_input_denied".to_string());
        }
    }
    let capability_grant = CapabilityGrant::new(capability_id, max_operation_kind)
        .with_sensitive_inputs(granted_sensitive_inputs);
    if !grant.state_effects.is_empty() || !declaration.sensitive_inputs.is_empty() {
        let granted_effects = parse_state_effects(&grant.state_effects)?;
        for effect in &declaration.state_effects {
            if !granted_effects.contains(effect) {
                return Err("state_effect_denied".to_string());
            }
        }
        return Ok(capability_grant.with_state_effects(granted_effects));
    }
    Ok(capability_grant)
}

fn parse_operation_kind(value: &str) -> std::result::Result<OperationKind, String> {
    match value {
        "read" => Ok(OperationKind::Read),
        "write" => Ok(OperationKind::Write),
        "execute" => Ok(OperationKind::Execute),
        "approve" => Ok(OperationKind::Approve),
        "admin" => Ok(OperationKind::Admin),
        _ => Err("invalid_operation_kind".to_string()),
    }
}

fn parse_state_effects(values: &[String]) -> std::result::Result<BTreeSet<StateEffect>, String> {
    values
        .iter()
        .map(|value| match value.as_str() {
            "host_screen_capture" => Ok(StateEffect::HostScreenCapture),
            "spawn_subagent" => Ok(StateEffect::SpawnSubagent),
            "repo_files" => Ok(StateEffect::RepoFiles),
            "repo_workspace" => Ok(StateEffect::RepoWorkspace),
            "repo_hooks" => Ok(StateEffect::RepoHooks),
            "store_memory_entries" => Ok(StateEffect::StoreMemoryEntries),
            "store_memory_suggestions" => Ok(StateEffect::StoreMemorySuggestions),
            "store_notes" => Ok(StateEffect::StoreNotes),
            "store_note_links" => Ok(StateEffect::StoreNoteLinks),
            "store_cron" => Ok(StateEffect::StoreCron),
            "store_schema" => Ok(StateEffect::StoreSchema),
            "matrix_outbox" => Ok(StateEffect::MatrixOutbox),
            "store_idempotency" => Ok(StateEffect::StoreIdempotency),
            "store_permission_requests" => Ok(StateEffect::StorePermissionRequests),
            "store_permission_grants" => Ok(StateEffect::StorePermissionGrants),
            "skill_trust" => Ok(StateEffect::SkillTrust),
            _ => Err("invalid_state_effect".to_string()),
        })
        .collect()
}

fn parse_sensitive_inputs(
    values: &[String],
) -> std::result::Result<BTreeSet<SensitiveInput>, String> {
    values
        .iter()
        .map(|value| match value.as_str() {
            "screen_capture" => Ok(SensitiveInput::ScreenCapture),
            _ => Err("invalid_sensitive_input".to_string()),
        })
        .collect()
}

fn core_tool_ids() -> Result<BTreeSet<CapabilityId>> {
    [
        agl_host_tools::SCREEN_CAPTURE_TOOL_ID,
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
    .map(CapabilityId::new)
    .collect::<std::result::Result<BTreeSet<_>, _>>()
    .context("builtin core tool id is invalid")
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
    run_id: &RunId,
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
    run_id: &RunId,
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
    run_id: &RunId,
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

fn write_runtime_feature_context_evidence(
    artifact_root: &std::path::Path,
    run_id: &RunId,
    evidence: &agl_runtime::RuntimeFeatureContextEvidence,
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("runtime-features.json");
    let parent = path.parent().with_context(|| {
        format!(
            "runtime feature evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime feature evidence directory {}",
            parent.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(evidence).with_context(|| {
        format!(
            "failed to serialize runtime feature evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes).with_context(|| {
        format!(
            "failed to write runtime feature evidence {}",
            path.display()
        )
    })
}

fn write_capability_policy_evidence(
    artifact_root: &std::path::Path,
    run_id: &RunId,
    effective: &EffectiveCapabilitySet,
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("capability-policy.json");
    let parent = path.parent().with_context(|| {
        format!(
            "capability policy evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create capability policy evidence directory {}",
            parent.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(effective).with_context(|| {
        format!(
            "failed to serialize capability policy evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes).with_context(|| {
        format!(
            "failed to write capability policy evidence {}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests;
