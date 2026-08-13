use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use agl_content::Content;
use agl_function::{
    FunctionToolMode, RuntimeDelegationPlan, RuntimeFunction, RuntimeIdentityValidation,
    RuntimeSubagentSpec,
};
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use agl_inference::evidence::InferenceArtifactRoot;
use agl_inference::{
    ArtifactFileHandle, InferenceCancellation, InferenceOutputSink, InferencePlanRejectionEvidence,
    InferenceRequest, InferenceResponse, ModelManagerError, ResolvedMediaAttachment,
    ResourceAdmissionDetails,
};
use agl_kernel::ToolCatalog;
use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionId, ExtensionSource, ExtensionTrust, HookEvent, HookId,
    OperationKind, SensitiveInput, SkillId, ToolGrantProvenance, ToolId, render_canonical_json,
};
use agl_kernel::{
    EffectiveToolSet, FunctionToolPolicy, SkillToolPolicy, ToolGrant, ToolPolicyInput,
};
use agl_kernel::{ModelRequest, TurnHookBatch, TurnMessage, VisibleTool};
use agl_memory::{MemoryEntry, MemoryRepository, MemoryScope, MemorySearchQuery};
use agl_model::{
    ModelArtifactRole, ModelExecutionPlan, ModelPlanRejection, ResolvedFunctionPlanInput,
    ResolvedModelPlanInput,
};
use agl_oven::render_engine_request;
use agl_package::PackageSourceTier;
use agl_runtime::{
    AgentLibrePaths, AgentLibreRuntimeConfig, PackageComposition, RenderedRuntimeFeatureContext,
    ResolvedRuntimeBundle, RuntimeFeatureRenderOptions, render_runtime_feature_context,
};
use agl_skill::{
    RegisteredSkill, SkillContextEvidence, SkillRegistry, SkillToolRouting, SkillToolRoutingView,
    SkillTrustState, build_verified_context_bundle,
};
use agl_store::{AglStore, PermissionGrantRecord};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::{
    ChatInferenceJob, ChatPlanRejection, InferenceClientHandle, InferenceOptions, ToolAccessMode,
};

const ARTIFACT_ROOT_ENV: &str = "AGL_INFERENCE_ARTIFACT_ROOT";
const MEMORY_CONTEXT_ENTRY_LIMIT: usize = 8;

#[derive(Clone)]
pub struct InferenceSession {
    inference_client: InferenceClientHandle,
    inference_plan: std::result::Result<agl_model::ModelExecutionPlan, ModelPlanRejection>,
    inference_artifacts: Vec<ArtifactFileHandle>,
    function_plan_input: ResolvedFunctionPlanInput,
    model_plan_input: ResolvedModelPlanInput,
    session_id: SessionId,
    scope_session_id: Option<SessionId>,
    system_prompt: Option<String>,
    runtime_feature_context: Option<String>,
    runtime_feature_evidence: Option<agl_runtime::RuntimeFeatureContextEvidence>,
    memory_context: Option<String>,
    function_ref: Option<String>,
    function_profile_required: bool,
    runtime_bundle: Option<ResolvedRuntimeBundle>,
    runtime_function: Option<RuntimeFunction>,
    function_context: Option<String>,
    function_skills: Vec<String>,
    function_extensions: Vec<ExtensionId>,
    extension_bindings: BTreeMap<String, RuntimeExtensionExtensionBinding>,
    runtime_identity: Option<RuntimeIdentityEvidence>,
    runtime_identity_validation: Option<RuntimeIdentityValidation>,
    skill_context: Option<String>,
    skill_tool_routing: SkillToolRoutingView,
    skill_hook_batches: Vec<TurnHookBatch>,
    visible_tools: Vec<VisibleTool>,
    effective_capabilities: EffectiveToolSet,
    permission_grants: RuntimePermissionGrantSnapshot,
    tool_mode: ToolAccessMode,
    store_root: PathBuf,
    runtime_paths: AgentLibrePaths,
    workspace_root: PathBuf,
    trust_store_path: PathBuf,
    config_skills: Vec<String>,
    option_skills: Vec<String>,
    selected_skills: Vec<SkillId>,
    memory_enabled: bool,
    model_profile_id: String,
    artifact_root: PathBuf,
    delegation_plan: Option<RuntimeDelegationPlan>,
    delegation_children: Vec<String>,
    delegation_authority_ceiling: BTreeSet<ToolId>,
    authority_ceiling: Option<BTreeSet<ToolId>>,
    allow_dynamic_grants: bool,
    tool_policy_override: Option<FunctionToolPolicy>,
}

pub(crate) struct SubagentSessionConfig {
    pub function_plan_input: ResolvedFunctionPlanInput,
    pub model_plan_input: ResolvedModelPlanInput,
    pub spec: RuntimeSubagentSpec,
    pub delegation_plan: RuntimeDelegationPlan,
    pub authority_ceiling: BTreeSet<ToolId>,
    pub artifact_root: PathBuf,
    pub workspace_root: PathBuf,
    pub execution_session_id: SessionId,
}

pub(crate) struct InferenceExecutionControl {
    pub cancellation: InferenceCancellation,
    pub deadline: Option<Instant>,
    pub output_sink: Arc<dyn InferenceOutputSink>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeExtensionExtensionBinding {
    artifact_reference: String,
    artifact_version: String,
    package_tree_digest: String,
    source_tier: PackageSourceTier,
    source_id: String,
    api_major: u32,
    declaration_digest: String,
    extension_version: String,
    runtime_generation_id: String,
    runtime_executable_digest: String,
    tools: Vec<String>,
    effects: Vec<EffectDeclaration>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResolutionWorkspaceIdentity {
    root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_tree: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResolutionFunctionPolicy {
    reference: String,
    max_output_tokens: Option<u32>,
    max_tool_calls: Option<u32>,
    tool_mode: Option<FunctionToolMode>,
    tool_policy: Option<FunctionToolPolicy>,
    delegation: Option<agl_function::FunctionDelegationBudget>,
    runtime_identity_validation: Option<RuntimeIdentityValidation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResolutionSkillIdentity {
    id: String,
    node_key: String,
    package_tree_digest: String,
    source_tier: PackageSourceTier,
    source_id: String,
    trust: SkillTrustState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResolutionAdmissionPhase {
    status: String,
    fallback_allowed: bool,
    model_load_started: bool,
    tool_effect_started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RuntimeResolutionAdmissionError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant: Option<ResourceAdmissionDetails>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResolutionAdmissionError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<ResourceAdmissionDetails>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResolutionRecord<'a> {
    schema: &'static str,
    run_id: &'a RunId,
    session_id: &'a SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<&'a TurnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<&'a AttemptId>,
    target_workspace: RuntimeResolutionWorkspaceIdentity,
    client_runtime: &'a agl_runtime::CurrentRuntimeIdentity,
    daemon_runtime: &'a agl_runtime::CurrentRuntimeIdentity,
    artifacts: agl_runtime::RuntimeBundleIdentity,
    function_policy: RuntimeResolutionFunctionPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_plan: Option<&'a ModelExecutionPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_plan_rejection: Option<&'a ModelPlanRejection>,
    extension_catalog_digest: agl_kernel::CatalogDigest,
    extension_bindings: &'a BTreeMap<String, RuntimeExtensionExtensionBinding>,
    skills: Vec<RuntimeResolutionSkillIdentity>,
    admission: RuntimeResolutionAdmissionPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_reuse_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_reuse_key: Option<String>,
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
        let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
        let function_profile_required = true;
        let (artifact_composition, mut runtime_bundle) = resolve_session_bundle(
            options.function_ref.as_deref(),
            &runtime.paths,
            &workspace_root,
            function_profile_required,
            &options.skills,
        )?;
        let runtime_function = options.function_ref.as_ref().and_then(|_| {
            runtime_bundle
                .as_ref()
                .map(|bundle| bundle.function.clone())
        });
        let extension_bindings = if options.function_ref.is_some() {
            runtime_bundle
                .as_ref()
                .map(bind_runtime_extensions)
                .transpose()?
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        let bundle = runtime_bundle
            .as_ref()
            .context("inference requires a resolved package-bound Function")?;
        bundle
            .model
            .as_ref()
            .context("inference Function has no resolved Model package")?;
        bundle
            .function
            .model_profile
            .as_deref()
            .context("inference Function has no selected Model profile")?;
        let config_skills = Vec::new();
        extend_runtime_bundle_skills(
            &mut runtime_bundle,
            artifact_composition.as_ref(),
            &workspace_root,
            &config_skills,
        )?;
        let system_prompt = None;
        let tool_mode = options.tool_mode;
        let trust_store_path = runtime.paths.state_dir.join("skill-trust.toml");
        let function_skills = runtime_function
            .as_ref()
            .map(|function| function.skills.clone())
            .unwrap_or_default();
        let function_context = runtime_function
            .as_ref()
            .map(|function| function.context.clone());
        let function_extensions = runtime_function
            .as_ref()
            .map(runtime_function_extensions)
            .transpose()?
            .unwrap_or_default();
        let delegation_plan = runtime_function
            .as_ref()
            .and_then(RuntimeFunction::delegation_plan);
        let delegation_children = delegation_plan
            .as_ref()
            .map(|plan| plan.root_subagents.clone())
            .unwrap_or_default();
        let skill_context = resolve_skill_context(SkillContextRequest {
            config_skills: &config_skills,
            function_skills: &function_skills,
            option_skills: &options.skills,
            function_policy: runtime_function
                .as_ref()
                .and_then(|function| function.tool_policy.as_ref()),
            selected_extensions: &function_extensions,
            tool_mode,
            artifact_root: &artifact_root,
            run_id: None,
            session_id: Some(&session_id),
            workspace_root: &workspace_root,
            runtime_paths: &runtime.paths,
            trust_store_path: &trust_store_path,
            store_root: &store_root,
            authority_ceiling: None,
            delegation_enabled: !delegation_children.is_empty(),
            allow_dynamic_grants: true,
            runtime_bundle: runtime_bundle.as_ref(),
        })?;
        let runtime_identity = runtime_function.as_ref().map(|function| {
            build_runtime_identity(
                function,
                &skill_context.selected_skills,
                &workspace_root,
                tool_mode,
            )
        });
        let runtime_identity_validation =
            effective_runtime_identity_validation(runtime_function.as_ref());
        let delegation_authority_ceiling =
            delegable_tool_ids(&skill_context.effective_capabilities);
        let mut hook_batches = skill_context.hook_batches;
        add_identity_hook_batch(&mut hook_batches, runtime_identity_validation.as_ref())?;
        let runtime_features = build_runtime_feature_context(
            &workspace_root,
            tool_mode,
            &skill_context.visible_tools,
        )?;
        let option_skills = options.skills.clone();
        let memory_context = resolve_memory_context(MemoryContextRequest {
            enabled: options.memory,
            config_skills: &config_skills,
            function_skills: &function_skills,
            option_skills: &options.skills,
            workspace_root: &workspace_root,
            runtime_paths: &runtime.paths,
            trust_store_path: &trust_store_path,
            artifact_root: &artifact_root,
            run_id: None,
            store_root: &store_root,
            runtime_bundle: runtime_bundle.as_ref(),
        })?;
        let visible_tools_value = serde_json::to_value(&skill_context.visible_tools)?;
        let visible_tools_digest = sha256_text(&render_canonical_json(&visible_tools_value));
        let (function_input, model_input) = runtime_bundle
            .as_ref()
            .expect("package-bound bundle was checked above")
            .model_execution_inputs(visible_tools_digest)?
            .context("selected Function has no Model execution inputs")?;
        let model_profile_id = function_input.selected_profile_id.clone();
        let (inference_plan, inference_artifacts) = resolve_session_plan(
            &function_input,
            &model_input,
            &inference_client,
            &runtime.paths,
        )?;
        Ok(Self {
            inference_client,
            inference_plan,
            inference_artifacts,
            function_plan_input: function_input,
            model_plan_input: model_input,
            scope_session_id: Some(session_id.clone()),
            session_id,
            system_prompt,
            runtime_feature_context: Some(runtime_features.content),
            runtime_feature_evidence: Some(runtime_features.evidence),
            memory_context,
            function_ref: options.function_ref,
            function_profile_required,
            runtime_function,
            function_context,
            function_skills,
            function_extensions,
            extension_bindings,
            runtime_identity,
            runtime_identity_validation,
            skill_context: skill_context.context,
            skill_tool_routing: skill_context.tool_routing,
            skill_hook_batches: hook_batches,
            visible_tools: skill_context.visible_tools,
            effective_capabilities: skill_context.effective_capabilities,
            permission_grants: skill_context.permission_grants,
            tool_mode,
            store_root,
            runtime_paths: runtime.paths.clone(),
            workspace_root,
            trust_store_path,
            config_skills,
            option_skills,
            selected_skills: skill_context.selected_skills,
            memory_enabled: options.memory,
            model_profile_id,
            artifact_root,
            delegation_plan,
            delegation_children,
            delegation_authority_ceiling,
            authority_ceiling: None,
            allow_dynamic_grants: true,
            tool_policy_override: None,
            runtime_bundle,
        })
    }

    pub(crate) fn new_subagent(
        config: SubagentSessionConfig,
        runtime: &AgentLibreRuntimeConfig,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        let SubagentSessionConfig {
            function_plan_input,
            model_plan_input,
            spec,
            delegation_plan,
            authority_ceiling,
            artifact_root,
            workspace_root,
            execution_session_id,
        } = config;
        let store_root = runtime.paths.store_root();
        let trust_store_path = runtime.paths.state_dir.join("skill-trust.toml");
        let function_skills = spec.skills.clone();
        let function_extensions = extensions_for_tools(&authority_ceiling)?;
        let config_skills = Vec::new();
        let option_skills = Vec::new();
        let tool_mode = subagent_tool_mode(spec.tool_mode);
        let tool_policy = subagent_tool_policy(&spec)?;
        let delegation_children = spec.children.clone();
        let skill_context = resolve_skill_context(SkillContextRequest {
            config_skills: &config_skills,
            function_skills: &function_skills,
            option_skills: &option_skills,
            function_policy: Some(&tool_policy),
            selected_extensions: &function_extensions,
            tool_mode,
            artifact_root: &artifact_root,
            run_id: None,
            session_id: None,
            workspace_root: &workspace_root,
            runtime_paths: &runtime.paths,
            trust_store_path: &trust_store_path,
            store_root: &store_root,
            authority_ceiling: Some(&authority_ceiling),
            delegation_enabled: !delegation_children.is_empty(),
            allow_dynamic_grants: false,
            runtime_bundle: None,
        })?;
        let memory_enabled = spec
            .memory
            .as_ref()
            .is_some_and(|memory| !memory.read.is_empty());
        let memory_context = resolve_memory_context(MemoryContextRequest {
            enabled: memory_enabled,
            config_skills: &config_skills,
            function_skills: &function_skills,
            option_skills: &option_skills,
            workspace_root: &workspace_root,
            runtime_paths: &runtime.paths,
            trust_store_path: &trust_store_path,
            artifact_root: &artifact_root,
            run_id: None,
            store_root: &store_root,
            runtime_bundle: None,
        })?;
        let runtime_features = build_runtime_feature_context(
            &workspace_root,
            tool_mode,
            &skill_context.visible_tools,
        )?;
        let delegation_authority_ceiling =
            delegable_tool_ids(&skill_context.effective_capabilities);
        let model_profile_id = function_plan_input.selected_profile_id.clone();
        let (inference_plan, inference_artifacts) = resolve_session_plan(
            &function_plan_input,
            &model_plan_input,
            &inference_client,
            &runtime.paths,
        )?;
        Ok(Self {
            inference_client,
            inference_plan,
            inference_artifacts,
            function_plan_input,
            model_plan_input,
            session_id: execution_session_id,
            scope_session_id: None,
            system_prompt: Some(spec.system_body.clone()),
            runtime_feature_context: Some(runtime_features.content),
            runtime_feature_evidence: Some(runtime_features.evidence),
            memory_context,
            function_ref: None,
            function_profile_required: false,
            runtime_bundle: None,
            runtime_function: None,
            function_context: None,
            function_skills,
            function_extensions,
            extension_bindings: BTreeMap::new(),
            runtime_identity: None,
            runtime_identity_validation: None,
            skill_context: skill_context.context,
            skill_tool_routing: skill_context.tool_routing,
            skill_hook_batches: skill_context.hook_batches,
            visible_tools: skill_context.visible_tools,
            effective_capabilities: skill_context.effective_capabilities,
            permission_grants: skill_context.permission_grants,
            tool_mode,
            store_root,
            runtime_paths: runtime.paths.clone(),
            workspace_root,
            trust_store_path,
            config_skills,
            option_skills,
            selected_skills: skill_context.selected_skills,
            memory_enabled,
            model_profile_id,
            artifact_root,
            delegation_plan: Some(delegation_plan),
            delegation_children,
            delegation_authority_ceiling,
            authority_ceiling: Some(authority_ceiling),
            allow_dynamic_grants: false,
            tool_policy_override: Some(tool_policy),
        })
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

    pub fn model_profile_id(&self) -> &str {
        &self.model_profile_id
    }

    pub fn artifact_root(&self) -> &std::path::Path {
        &self.artifact_root
    }

    pub(crate) fn delegation_plan(&self) -> Option<&RuntimeDelegationPlan> {
        self.delegation_plan.as_ref()
    }

    pub(crate) fn delegation_children(&self) -> &[String] {
        &self.delegation_children
    }

    pub(crate) fn delegation_authority_ceiling(&self) -> &BTreeSet<ToolId> {
        &self.delegation_authority_ceiling
    }

    pub(crate) fn model_plan_inputs(
        &self,
    ) -> Result<(ResolvedFunctionPlanInput, ResolvedModelPlanInput)> {
        let visible_tools = serde_json::to_value(&self.visible_tools)?;
        let visible_tools_digest = sha256_text(&render_canonical_json(&visible_tools));
        self.runtime_bundle
            .as_ref()
            .context("delegation requires the admitted runtime bundle")?
            .model_execution_inputs(visible_tools_digest)?
            .context("delegation requires package-bound Model inputs")
    }

    pub(crate) fn runtime_bundle(&self) -> Option<&ResolvedRuntimeBundle> {
        self.runtime_bundle.as_ref()
    }

    pub(crate) fn install_root_delegation_plan(&mut self, plan: Option<RuntimeDelegationPlan>) {
        self.delegation_children = plan
            .as_ref()
            .map(|plan| plan.root_subagents.clone())
            .unwrap_or_default();
        self.delegation_plan = plan;
    }

    pub(crate) fn freeze_delegation_authority(&mut self, persisted: Option<BTreeSet<ToolId>>) {
        self.delegation_authority_ceiling =
            persisted.unwrap_or_else(|| delegable_tool_ids(&self.effective_capabilities));
    }

    pub(crate) fn dynamic_grants_enabled(&self) -> bool {
        self.allow_dynamic_grants
    }

    pub fn backend_name(&self) -> &'static str {
        "llama_cpp"
    }

    pub(crate) fn repair_malformed_tool_calls(&self) -> bool {
        self.function_plan_input
            .generation_policy
            .repair_malformed_tool_calls()
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
        if let Some(validation) = &self.runtime_identity_validation {
            payload.insert(
                "runtime_identity_validation".to_string(),
                serde_json::to_value(validation).expect("identity validation serializes"),
            );
        }
        serde_json::Value::Object(payload)
    }

    pub fn turn_visible_tools(&self) -> &[VisibleTool] {
        &self.visible_tools
    }

    pub(crate) fn effective_capabilities(&self) -> &EffectiveToolSet {
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

    pub(crate) fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    pub(crate) fn runtime_paths(&self) -> &AgentLibrePaths {
        &self.runtime_paths
    }

    pub(crate) fn prepare_artifact_write_for_tool(
        &self,
        _run_id: &RunId,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<()> {
        Ok(())
    }

    pub(crate) fn generate(
        &mut self,
        request: ModelRequest,
        attempt_id: AttemptId,
        session_id: Option<SessionId>,
        request_id: Option<RequestId>,
        effective_capabilities: &EffectiveToolSet,
        control: InferenceExecutionControl,
    ) -> Result<InferenceResponse> {
        if let Some(session_id) = &session_id {
            ensure!(
                session_id == &self.session_id,
                "inference request session does not match its managed context"
            );
        }
        let product_resolution = self.write_runtime_resolution(
            &request.run_id,
            Some(&request.turn_id),
            Some(&attempt_id),
            None,
            None,
        )?;
        let evidence_root =
            InferenceArtifactRoot::new(self.artifact_root.clone()).run_dir(&request.run_id);
        let evidence_run_id = request.run_id.clone();
        let evidence_turn_id = request.turn_id.clone();
        let evidence_attempt_id = attempt_id.clone();
        if let Some(evidence) = &self.runtime_feature_evidence {
            write_runtime_feature_context_evidence(&self.artifact_root, &request.run_id, evidence)?;
        }
        let request = build_inference_request(
            request,
            attempt_id,
            InferenceRequestContexts {
                session_id: Some(&self.session_id),
                request_id: request_id.as_ref(),
                system_prompt: self.system_prompt.as_deref(),
                runtime_feature_context: self.runtime_feature_context.as_deref(),
                function_context: self.function_context.as_deref(),
                memory_context: self.memory_context.as_deref(),
                skill_context: self.skill_context.as_deref(),
                skill_tool_routing: Some(&self.skill_tool_routing),
                effective_capabilities: Some(effective_capabilities),
            },
        )?;
        let plan = match &self.inference_plan {
            Ok(plan) => plan.clone(),
            Err(rejection) => {
                self.inference_client
                    .record_plan_rejection(ChatPlanRejection {
                        request,
                        rejection: InferencePlanRejectionEvidence::new(
                            &self.function_plan_input,
                            &self.model_plan_input,
                            rejection.clone(),
                            Some(product_resolution),
                        ),
                        evidence_root: Some(evidence_root),
                    })?;
                return Err(rejection.clone().into());
            }
        };
        let media = resolve_request_media(&request, &self.store_root)?;
        let result = self.inference_client.generate(ChatInferenceJob {
            plan,
            artifacts: self.inference_artifacts.clone(),
            media,
            session_id: self.session_id.clone(),
            request,
            cancellation: control.cancellation,
            deadline: control.deadline,
            output_sink: control.output_sink,
            evidence_root: Some(evidence_root),
            product_resolution: Some(product_resolution),
        });
        match &result {
            Ok(response) => {
                if let Some(grant) = response.metadata.resource_admission.as_ref() {
                    self.write_runtime_resolution(
                        &evidence_run_id,
                        Some(&evidence_turn_id),
                        Some(&evidence_attempt_id),
                        None,
                        Some(grant),
                    )?;
                }
            }
            Err(error) => {
                if let Some(manager_error) = error.downcast_ref::<ModelManagerError>()
                    && manager_error.resource_admission_details().is_some()
                {
                    self.write_runtime_resolution(
                        &evidence_run_id,
                        Some(&evidence_turn_id),
                        Some(&evidence_attempt_id),
                        Some(manager_error),
                        None,
                    )?;
                }
            }
        }
        result
    }

    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn clear_context(&self) -> Result<()> {
        let Ok(plan) = &self.inference_plan else {
            return Ok(());
        };
        let context = plan.context_key(self.session_id.as_str());
        self.inference_client
            .clear_context(&context)
            .context("failed to clear managed inference context")
    }

    pub(crate) fn selected_model_id(&self) -> Option<String> {
        self.model_plan_input
            .model
            .artifacts
            .iter()
            .find(|artifact| artifact.role == ModelArtifactRole::Main)
            .map(|artifact| artifact.model_id.to_string())
    }

    pub(crate) fn function_ref(&self) -> Option<String> {
        self.function_ref.clone()
    }

    pub(crate) fn function_max_tool_calls(&self) -> Option<u32> {
        self.runtime_function
            .as_ref()
            .and_then(|function| function.max_tool_calls)
    }

    pub(crate) fn context_limit_tokens(&self) -> u32 {
        self.model_plan_input
            .model
            .profiles
            .iter()
            .find(|profile| profile.id == self.function_plan_input.selected_profile_id)
            .map_or(u32::MAX, |profile| profile.context_tokens)
    }

    pub(crate) fn select_model(&mut self, model_id: &str, model_path: PathBuf) -> Result<()> {
        let _ = (model_id, model_path);
        anyhow::bail!("live model mutation was removed; select a different package-bound Function")
    }

    pub(crate) fn select_operation_mode(&mut self, mode: ToolAccessMode) -> Result<()> {
        let previous = self.tool_mode;
        self.tool_mode = mode;
        if let Err(error) = self.refresh_runtime_context(None, None) {
            self.tool_mode = previous;
            self.refresh_runtime_context(None, None)
                .context("failed to restore runtime context after mode rejection")?;
            return Err(error).context("selected operation mode is not admitted");
        }
        Ok(())
    }

    pub(crate) fn select_skills(&mut self, skill_ids: Vec<String>) -> Result<()> {
        let previous = std::mem::replace(&mut self.option_skills, skill_ids);
        if let Err(error) = self.refresh_runtime_context(None, None) {
            self.option_skills = previous;
            self.refresh_runtime_context(None, None)
                .context("failed to restore runtime context after skill rejection")?;
            return Err(error).context("selected skills are not admitted");
        }
        Ok(())
    }

    pub(crate) fn selected_skill_ids(&self) -> Vec<String> {
        self.option_skills.clone()
    }

    pub fn release_context(&self) -> Result<()> {
        let Ok(plan) = &self.inference_plan else {
            return Ok(());
        };
        let context = plan.context_key(self.session_id.as_str());
        self.inference_client
            .release_context(&context)
            .map_err(|error| {
                anyhow::anyhow!("failed to release managed inference context: {error:#}")
            })
    }

    pub(crate) fn set_workspace_root_and_refresh(
        &mut self,
        workspace_root: &std::path::Path,
    ) -> Result<()> {
        self.workspace_root = workspace_root.to_path_buf();
        self.refresh_runtime_context(None, None)
    }

    pub(crate) fn refresh_runtime_context(
        &mut self,
        run_id: Option<&RunId>,
        turn_id: Option<&TurnId>,
    ) -> Result<()> {
        if let Some(reference) = self.function_ref.clone() {
            let mut selected_skills = self.config_skills.clone();
            selected_skills.extend(self.option_skills.iter().cloned());
            let (_, bundle) = resolve_session_bundle(
                Some(&reference),
                &self.runtime_paths,
                &self.workspace_root,
                self.function_profile_required,
                &selected_skills,
            )?;
            let bundle = bundle.expect("function ref produces a runtime bundle");
            let function = bundle.function.clone();
            self.function_context = Some(function.context.clone());
            self.function_skills = function.skills.clone();
            self.function_extensions = runtime_function_extensions(&function)?;
            self.extension_bindings = bind_runtime_extensions(&bundle)?;
            self.runtime_function = Some(function);
            self.runtime_bundle = Some(bundle);
        } else {
            let mut selected_skills = self.config_skills.clone();
            selected_skills.extend(self.option_skills.iter().cloned());
            let (_, bundle) = resolve_session_bundle(
                None,
                &self.runtime_paths,
                &self.workspace_root,
                self.function_profile_required,
                &selected_skills,
            )?;
            self.runtime_bundle = bundle;
            self.extension_bindings.clear();
        }
        let delegation_enabled = self.delegation_available(run_id)?;
        let skill_context = resolve_skill_context(SkillContextRequest {
            config_skills: &self.config_skills,
            function_skills: &self.function_skills,
            option_skills: &self.option_skills,
            function_policy: self.tool_policy_override.as_ref().or_else(|| {
                self.runtime_function
                    .as_ref()
                    .and_then(|function| function.tool_policy.as_ref())
            }),
            selected_extensions: &self.function_extensions,
            tool_mode: self.tool_mode,
            artifact_root: &self.artifact_root,
            run_id,
            session_id: self.scope_session_id.as_ref(),
            workspace_root: &self.workspace_root,
            runtime_paths: &self.runtime_paths,
            trust_store_path: &self.trust_store_path,
            store_root: &self.store_root,
            authority_ceiling: self.authority_ceiling.as_ref(),
            delegation_enabled,
            allow_dynamic_grants: self.allow_dynamic_grants,
            runtime_bundle: self.runtime_bundle.as_ref(),
        })?;
        self.runtime_identity = self.runtime_function.as_ref().map(|function| {
            build_runtime_identity(
                function,
                &skill_context.selected_skills,
                &self.workspace_root,
                self.tool_mode,
            )
        });
        self.runtime_identity_validation =
            effective_runtime_identity_validation(self.runtime_function.as_ref());
        self.skill_context = skill_context.context;
        self.skill_tool_routing = skill_context.tool_routing;
        let mut hook_batches = skill_context.hook_batches;
        add_identity_hook_batch(&mut hook_batches, self.runtime_identity_validation.as_ref())?;
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
            runtime_paths: &self.runtime_paths,
            trust_store_path: &self.trust_store_path,
            artifact_root: &self.artifact_root,
            run_id,
            store_root: &self.store_root,
            runtime_bundle: self.runtime_bundle.as_ref(),
        })?;
        let visible_tools_value = serde_json::to_value(&self.visible_tools)?;
        let visible_tools_digest = sha256_text(&render_canonical_json(&visible_tools_value));
        let (function_plan_input, model_plan_input) = if let Some(bundle) = &self.runtime_bundle {
            bundle
                .model_execution_inputs(visible_tools_digest)?
                .context("selected Function has no Model execution inputs")?
        } else {
            let mut function = self.function_plan_input.clone();
            function.visible_tools_digest = visible_tools_digest;
            (function, self.model_plan_input.clone())
        };
        let (inference_plan, inference_artifacts) = resolve_session_plan(
            &function_plan_input,
            &model_plan_input,
            &self.inference_client,
            &self.runtime_paths,
        )?;
        self.model_profile_id = function_plan_input.selected_profile_id.clone();
        self.function_plan_input = function_plan_input;
        self.model_plan_input = model_plan_input;
        self.inference_plan = inference_plan;
        self.inference_artifacts = inference_artifacts;
        let runtime_features = build_runtime_feature_context(
            &self.workspace_root,
            self.tool_mode,
            &self.visible_tools,
        )?;
        self.runtime_feature_context = Some(runtime_features.content);
        self.runtime_feature_evidence = Some(runtime_features.evidence);
        if let Some(run_id) = run_id {
            self.write_runtime_resolution(run_id, turn_id, None, None, None)?;
        }
        Ok(())
    }

    fn write_runtime_resolution(
        &self,
        run_id: &RunId,
        turn_id: Option<&TurnId>,
        attempt_id: Option<&AttemptId>,
        admission_error: Option<&ModelManagerError>,
        admission_grant: Option<&ResourceAdmissionDetails>,
    ) -> Result<serde_json::Value> {
        ensure!(
            admission_error.is_none() || admission_grant.is_none(),
            "runtime resolution admission cannot be both granted and rejected"
        );
        let Some(bundle) = self.runtime_bundle.as_ref() else {
            return Ok(serde_json::Value::Null);
        };
        let function = &bundle.function;
        let canonical_root = self
            .workspace_root
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_root.clone());
        let workspace_git = agl_repo::git_source_provenance(&canonical_root).ok();
        let selected_skills = selected_skill_ids(
            &self.config_skills,
            &self.function_skills,
            &self.option_skills,
        )?;
        let skill_registry = composed_skill_registry(
            &self.runtime_paths,
            &self.workspace_root,
            &self.trust_store_path,
            &selected_skills,
            Some(bundle),
        )?;
        let skills = selected_skills
            .iter()
            .map(|skill_id| {
                let admitted = bundle.skills.get(skill_id.as_str()).with_context(|| {
                    format!("selected Skill `{skill_id}` is absent from runtime evidence")
                })?;
                let node = bundle
                    .graph
                    .nodes
                    .get(&admitted.node_key)
                    .with_context(|| format!("selected Skill `{skill_id}` graph node is absent"))?;
                let trust = skill_registry
                    .get(skill_id)
                    .context("selected Skill trust result is absent")?
                    .trust;
                Ok(RuntimeResolutionSkillIdentity {
                    id: skill_id.as_str().to_owned(),
                    node_key: admitted.node_key.clone(),
                    package_tree_digest: node.package_tree_digest.to_string(),
                    source_tier: node.candidate.tier,
                    source_id: node.candidate.source_id.to_string(),
                    trust,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let model_reuse_key = self
            .inference_plan
            .as_ref()
            .ok()
            .map(ModelExecutionPlan::model_key);
        let context_reuse_key = self
            .inference_plan
            .as_ref()
            .ok()
            .map(|plan| plan.context_key(self.session_id.as_str()));
        let admission_error = admission_error.map(|error| RuntimeResolutionAdmissionError {
            code: error.code().to_owned(),
            message: error.to_string(),
            details: error.resource_admission_details().cloned(),
        });
        let admission_details = admission_error
            .as_ref()
            .and_then(|error| error.details.as_ref())
            .or(admission_grant);
        let record = RuntimeResolutionRecord {
            schema: "agentlibre.runtime-resolution/v1",
            run_id,
            session_id: &self.session_id,
            turn_id,
            attempt_id,
            target_workspace: RuntimeResolutionWorkspaceIdentity {
                root: canonical_root,
                git_revision: workspace_git
                    .as_ref()
                    .map(|identity| identity.revision.clone()),
                git_tree: workspace_git.map(|identity| identity.tree),
            },
            // First-party session creation has already required exact client /
            // daemon identity equality at the v8 handshake boundary.
            client_runtime: &bundle.runtime,
            daemon_runtime: &bundle.runtime,
            artifacts: bundle.identity(),
            function_policy: RuntimeResolutionFunctionPolicy {
                reference: bundle.graph.root.clone(),
                max_output_tokens: function.max_output_tokens,
                max_tool_calls: function.max_tool_calls,
                tool_mode: function.tool_mode,
                tool_policy: function.tool_policy.clone(),
                delegation: function.delegation.clone(),
                runtime_identity_validation: function.runtime_identity_validation.clone(),
            },
            model_plan: self.inference_plan.as_ref().ok(),
            model_plan_rejection: self.inference_plan.as_ref().err(),
            extension_catalog_digest: agl_kernel::CatalogDigest::from_admitted(
                crate::tools::chat_product_factories()
                    .iter()
                    .map(|factory| factory.definition())
                    .collect::<Vec<_>>()
                    .iter()
                    .map(agl_extension::ExtensionDefinition::descriptor),
            ),
            extension_bindings: &self.extension_bindings,
            skills,
            admission: RuntimeResolutionAdmissionPhase {
                status: if admission_error.is_some() {
                    "rejected".to_owned()
                } else if admission_grant.is_some() {
                    "granted".to_owned()
                } else {
                    "preview_non_authoritative".to_owned()
                },
                fallback_allowed: admission_details.is_some_and(|details| details.fallback_allowed),
                model_load_started: admission_grant.is_some()
                    || admission_details.is_some_and(|details| details.model_load_started),
                tool_effect_started: admission_details
                    .is_some_and(|details| details.tool_effect_started),
                error: admission_error,
                grant: admission_grant.cloned(),
            },
            model_reuse_key: model_reuse_key.map(|key| key.as_str().to_owned()),
            context_reuse_key: context_reuse_key.map(|key| key.as_str().to_owned()),
        };
        let value = serde_json::to_value(&record)
            .context("failed to serialize runtime resolution projection context")?;
        let run_dir = InferenceArtifactRoot::new(self.artifact_root.clone()).run_dir(run_id);
        std::fs::create_dir_all(&run_dir).with_context(|| {
            format!(
                "failed to create runtime resolution directory {}",
                run_dir.display()
            )
        })?;
        write_function_content_evidence(&run_dir, function)?;
        let path = run_dir.join("runtime-resolution.json");
        let temporary = run_dir.join("runtime-resolution.json.tmp");
        let mut bytes = serde_json::to_vec_pretty(&record)
            .context("failed to serialize canonical runtime resolution")?;
        bytes.push(b'\n');
        std::fs::write(&temporary, bytes).with_context(|| {
            format!(
                "failed to write temporary runtime resolution {}",
                temporary.display()
            )
        })?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("failed to commit runtime resolution {}", path.display()))?;
        Ok(value)
    }

    fn delegation_available(&self, run_id: Option<&RunId>) -> Result<bool> {
        if self.delegation_children.is_empty() {
            return Ok(false);
        }
        let Some(run_id) = run_id else {
            return Ok(true);
        };
        let plan = self
            .delegation_plan
            .as_ref()
            .context("delegation children require a persisted plan")?;
        let store = AglStore::open_current_at(&self.store_root)
            .context("failed to inspect delegation budget")?;
        let run = store
            .run(run_id)?
            .with_context(|| format!("run {run_id} disappeared during context refresh"))?;
        let root = store
            .run(&run.root_run_id)?
            .context("delegation root run disappeared")?;
        let now_ms = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(i64::MAX);
        let timeout_ms = plan.budget.timeout_seconds.saturating_mul(1_000);
        let deadline_ms = root
            .created_at_ms
            .saturating_add(i64::try_from(timeout_ms).unwrap_or(i64::MAX));
        let child_count = store.run_children(run_id)?.len();
        let tree_output_remaining = plan
            .budget
            .max_total_output_tokens
            .saturating_sub(root.delegation_used_output_tokens)
            .saturating_sub(root.delegation_reserved_output_tokens);
        let run_output_remaining = run
            .budget
            .model_output_tokens
            .saturating_sub(run.usage.model_output_tokens)
            .saturating_sub(run.delegation_used_output_tokens)
            .saturating_sub(run.delegation_reserved_output_tokens);
        Ok(run.cancellation_requested_at_ms.is_none()
            && run.depth < plan.budget.max_depth
            && child_count < plan.budget.max_children_per_run as usize
            && root.delegation_reserved_descendants < plan.budget.max_descendants
            && now_ms < deadline_ms
            && tree_output_remaining > 0
            && run_output_remaining > 0
            && run.usage.wall_time_ms < run.budget.wall_time_ms
            && run.usage.model_input_tokens < run.budget.model_input_tokens
            && run.usage.model_attempts < run.budget.model_attempts
            && run.usage.tool_calls < run.budget.tool_calls)
    }
}

fn agent_event_stream_path(artifact_root: &std::path::Path, run_id: &RunId) -> PathBuf {
    InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("events.jsonl")
}

fn resolve_session_bundle(
    reference: Option<&str>,
    paths: &AgentLibrePaths,
    workspace_root: &Path,
    require_profile: bool,
    additional_skills: &[String],
) -> Result<(Option<PackageComposition>, Option<ResolvedRuntimeBundle>)> {
    let reference = match reference {
        Some(reference) => reference,
        None if additional_skills.is_empty() => return Ok((None, None)),
        None => agl_repo::DEFAULT_FUNCTION,
    };
    let composition = agl_runtime::compose_packages(paths, workspace_root)?;
    let bundle = composition
        .resolve_runtime_bundle(
            workspace_root,
            &paths.config_dir,
            reference,
            require_profile,
            additional_skills,
        )
        .with_context(|| format!("failed to resolve function `{reference}`"))?;
    Ok((Some(composition), Some(bundle)))
}

fn extend_runtime_bundle_skills(
    bundle: &mut Option<ResolvedRuntimeBundle>,
    composition: Option<&PackageComposition>,
    workspace_root: &Path,
    selected_skills: &[String],
) -> Result<()> {
    let Some(current) = bundle.take() else {
        return Ok(());
    };
    let composition = composition.context("runtime bundle has no artifact composition")?;
    *bundle = Some(current.with_selected_skills(composition, workspace_root, selected_skills)?);
    Ok(())
}

fn write_function_content_evidence(run_dir: &Path, function: &RuntimeFunction) -> Result<()> {
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

fn effective_runtime_identity_validation(
    function: Option<&RuntimeFunction>,
) -> Option<RuntimeIdentityValidation> {
    function?.runtime_identity_validation.clone()
}

fn add_identity_hook_batch(
    hook_batches: &mut Vec<TurnHookBatch>,
    validation: Option<&RuntimeIdentityValidation>,
) -> Result<()> {
    let Some(validation) = validation else {
        return Ok(());
    };
    let hook_id = if validation.required {
        agl_core_tools::guards::RUNTIME_IDENTITY_REQUIRE_HOOK_ID
    } else {
        agl_core_tools::guards::RUNTIME_IDENTITY_VALIDATE_HOOK_ID
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

fn build_inference_request(
    request: ModelRequest,
    attempt_id: AttemptId,
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
        .context("effective tool set is missing from inference request context")?;
    ensure_visible_tool_parity(&request.visible_tools, effective_capabilities)?;
    if non_empty_context(contexts.skill_context)
        .is_some_and(|context| context.contains("<agentlibre_skill_context>"))
    {
        let skill_tool_routing = contexts
            .skill_tool_routing
            .context("skill tool routing is missing from inference request context")?;
        ensure_skill_tool_routing_parity(skill_tool_routing, effective_capabilities)?;
    }
    request_messages.extend(request.messages);

    let model_request = ModelRequest {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        request_index,
        messages: request_messages,
        visible_tools: request.visible_tools,
    };
    let rendered = render_engine_request(&model_request)?;
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
) -> Result<RenderedRuntimeFeatureContext> {
    let available_model_tools = visible_tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<Vec<_>>();
    let catalog = crate::tools::chat_extension_catalog()?;
    Ok(render_runtime_feature_context(
        RuntimeFeatureRenderOptions {
            version: env!("CARGO_PKG_VERSION"),
            workspace_root: Some(workspace_root),
            tool_mode: tool_mode.as_str(),
            available_model_tools: &available_model_tools,
            extension_descriptors: catalog.extensions(),
            char_cap: agl_runtime::DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP,
        },
    ))
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
    skill_tool_routing: Option<&'a SkillToolRoutingView>,
    effective_capabilities: Option<&'a EffectiveToolSet>,
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

fn ensure_visible_tool_parity(
    visible_tools: &[VisibleTool],
    tools: &EffectiveToolSet,
) -> Result<()> {
    let visible = visible_tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<BTreeSet<_>>();
    let effective = tools
        .tools()
        .map(|tool| tool.declaration().id.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        visible == effective,
        "model-visible tools differ from the effective tool set"
    );
    Ok(())
}

fn ensure_skill_tool_routing_parity(
    routing: &SkillToolRoutingView,
    tools: &EffectiveToolSet,
) -> Result<()> {
    for (skill_id, route) in routing.routes() {
        let expected = route
            .declared_tools()
            .into_iter()
            .filter(|tool| tools.contains(tool))
            .collect::<BTreeSet<_>>();
        ensure!(
            &expected == route.callable_tools(),
            "skill `{skill_id}` callable routing differs from the effective tool set"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSkillContext {
    context: Option<String>,
    tool_routing: SkillToolRoutingView,
    hook_batches: Vec<TurnHookBatch>,
    visible_tools: Vec<VisibleTool>,
    effective_capabilities: EffectiveToolSet,
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
            .map(|grant| grant.tool_id.as_str().to_string())
            .collect()
    }

    pub(crate) fn ignored_grants(&self) -> Vec<String> {
        self.ignored
            .iter()
            .map(|grant| format!("{}:{}:{}", grant.grant_id, grant.tool_id, grant.reason))
            .collect()
    }

    fn tool_grants(&self) -> Vec<ToolGrant> {
        self.admitted
            .iter()
            .map(|grant| {
                ToolGrant::new(grant.tool_id.clone(), grant.max_operation_kind)
                    .with_state_effects(grant.state_effects.iter().cloned())
                    .with_sensitive_inputs(grant.sensitive_inputs.iter().copied())
                    .with_provenance(ToolGrantProvenance::new(
                        grant.grant_id.clone(),
                        grant.duration.clone(),
                        grant.admitted_scope.clone(),
                        grant.scope_digest.clone(),
                    ))
            })
            .collect()
    }

    pub(crate) fn sensitive_input_run(
        &self,
        tool_id: &ToolId,
        input: SensitiveInput,
    ) -> Option<&RunId> {
        self.admitted
            .iter()
            .find(|grant| &grant.tool_id == tool_id && grant.sensitive_inputs.contains(&input))
            .map(|grant| &grant.run_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AdmittedPermissionGrant {
    grant_id: String,
    tool_id: ToolId,
    max_operation_kind: OperationKind,
    state_effects: BTreeSet<EffectId>,
    sensitive_inputs: BTreeSet<SensitiveInput>,
    run_id: RunId,
    duration: String,
    admitted_scope: String,
    scope_digest: String,
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
    runtime_paths: &'a AgentLibrePaths,
    trust_store_path: &'a std::path::Path,
    artifact_root: &'a std::path::Path,
    run_id: Option<&'a RunId>,
    store_root: &'a std::path::Path,
    runtime_bundle: Option<&'a ResolvedRuntimeBundle>,
}

struct SkillContextRequest<'a> {
    config_skills: &'a [String],
    function_skills: &'a [String],
    option_skills: &'a [String],
    function_policy: Option<&'a FunctionToolPolicy>,
    selected_extensions: &'a [ExtensionId],
    tool_mode: ToolAccessMode,
    artifact_root: &'a std::path::Path,
    run_id: Option<&'a RunId>,
    session_id: Option<&'a SessionId>,
    workspace_root: &'a std::path::Path,
    runtime_paths: &'a AgentLibrePaths,
    trust_store_path: &'a std::path::Path,
    store_root: &'a std::path::Path,
    authority_ceiling: Option<&'a BTreeSet<ToolId>>,
    delegation_enabled: bool,
    allow_dynamic_grants: bool,
    runtime_bundle: Option<&'a ResolvedRuntimeBundle>,
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
        request.runtime_paths,
        request.trust_store_path,
        request.runtime_bundle,
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
    runtime_paths: &AgentLibrePaths,
    trust_store_path: &std::path::Path,
    runtime_bundle: Option<&ResolvedRuntimeBundle>,
) -> Result<()> {
    let selected_skills = selected_skill_ids(config_skills, function_skills, option_skills)?;
    if selected_skills.is_empty() {
        return Ok(());
    }
    let skill_registry = composed_skill_registry(
        runtime_paths,
        workspace_root,
        trust_store_path,
        &selected_skills,
        runtime_bundle,
    )
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

fn composed_skill_registry(
    _runtime_paths: &AgentLibrePaths,
    _workspace_root: &Path,
    _trust_store_path: &Path,
    selected_skills: &[SkillId],
    runtime_bundle: Option<&ResolvedRuntimeBundle>,
) -> Result<SkillRegistry> {
    let mut registry = SkillRegistry::new();
    for skill_id in selected_skills {
        let bundle = runtime_bundle
            .context("selected Skills require the session's admitted runtime package bundle")?;
        let skill = bundle.skills.get(skill_id.as_str()).with_context(|| {
            format!("selected Skill `{skill_id}` is absent from the admitted runtime bundle")
        })?;
        let node = bundle
            .graph
            .nodes
            .get(&skill.node_key)
            .context("admitted Skill node is absent from the runtime graph")?;
        let (harness, tier) = (skill.harness.clone(), node.candidate.tier);
        let trust = if tier == PackageSourceTier::Builtin {
            SkillTrustState::TrustedByBinary
        } else {
            SkillTrustState::Unknown
        };
        registry.register(RegisteredSkill { harness, trust })?;
    }
    Ok(registry)
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
    let skill_registry = composed_skill_registry(
        request.runtime_paths,
        request.workspace_root,
        request.trust_store_path,
        &selected_skills,
        request.runtime_bundle,
    )
    .context("failed to load skill registry")?;
    let tool_catalog = crate::tools::chat_extension_catalog()?;
    let hook_batches = if selected_skills.is_empty() {
        Vec::new()
    } else {
        selected_skill_hook_batches(&skill_registry, &tool_catalog, &selected_skills)?
    };
    let mut permission_grants = if request.allow_dynamic_grants
        && let Some(run_id) = request.run_id
    {
        admit_dynamic_permission_grants(
            &skill_registry,
            &tool_catalog,
            &selected_skills,
            request.store_root,
            request.workspace_root,
            run_id,
            request.session_id,
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
        RuntimeToolBoundary {
            authority_ceiling: request.authority_ceiling,
            delegation_enabled: request.delegation_enabled,
            selected_extensions: request.selected_extensions.iter().cloned().collect(),
        },
    )?;
    if let Some(run_id) = request.run_id {
        if request.allow_dynamic_grants {
            finalize_permission_grants(
                request.store_root,
                run_id,
                &effective_capabilities,
                &mut permission_grants,
            )?;
        }
        write_tool_policy_evidence(request.artifact_root, run_id, &effective_capabilities)?;
    }
    let tool_routing =
        derive_skill_tool_routing(&skill_registry, &selected_skills, &effective_capabilities)?;
    let context = if selected_skills.is_empty() {
        None
    } else {
        let bundle = build_verified_context_bundle(
            &skill_registry,
            &tool_catalog,
            &selected_skills,
            &tool_routing,
        )
        .context("failed to build verified skill context")?;
        if let Some(run_id) = request.run_id {
            write_skill_context_evidence(request.artifact_root, run_id, &bundle.evidence)?;
        }
        Some(bundle.content)
    };
    let visible_tools = visible_tools_from_effective(&effective_capabilities);
    Ok(ResolvedSkillContext {
        context,
        tool_routing,
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

fn subagent_tool_mode(mode: FunctionToolMode) -> ToolAccessMode {
    match mode {
        FunctionToolMode::ReadOnly => ToolAccessMode::ReadOnly,
        FunctionToolMode::Write => ToolAccessMode::Write,
        FunctionToolMode::Execute => ToolAccessMode::Execute,
        FunctionToolMode::Approve => ToolAccessMode::Approve,
        FunctionToolMode::Admin => ToolAccessMode::Admin,
    }
}

fn delegable_tool_ids(effective: &EffectiveToolSet) -> BTreeSet<ToolId> {
    let mut tools = effective
        .tools()
        .map(|tool| tool.declaration().id.clone())
        .collect::<BTreeSet<_>>();
    for tool in [
        agl_core_tools::PERMISSIONS_REQUEST_TOOL_ID,
        agl_core_tools::PERMISSIONS_GRANT_TOOL_ID,
        agl_core_tools::PERMISSIONS_REVOKE_TOOL_ID,
    ] {
        tools.remove(&ToolId::new(tool).expect("builtin permission tool IDs remain valid"));
    }
    tools
}

fn subagent_tool_policy(spec: &RuntimeSubagentSpec) -> Result<FunctionToolPolicy> {
    let mut policy = spec.tool_policy.clone();
    let memory = spec.memory.as_ref();
    if memory.is_none_or(|memory| memory.read.is_empty()) {
        for tool in [
            agl_core_tools::MEMORY_SEARCH_TOOL_ID,
            agl_core_tools::MEMORY_LIST_TOOL_ID,
        ] {
            policy.deny.insert(ToolId::new(tool)?);
        }
    }
    if memory.is_none_or(|memory| memory.write.is_empty()) {
        for tool in [
            agl_core_tools::MEMORY_SUGGEST_TOOL_ID,
            agl_core_tools::MEMORY_ADD_TOOL_ID,
            agl_core_tools::MEMORY_APPROVE_TOOL_ID,
            agl_core_tools::MEMORY_REJECT_TOOL_ID,
        ] {
            policy.deny.insert(ToolId::new(tool)?);
        }
    }
    for tool in [
        agl_core_tools::PERMISSIONS_REQUEST_TOOL_ID,
        agl_core_tools::PERMISSIONS_GRANT_TOOL_ID,
        agl_core_tools::PERMISSIONS_REVOKE_TOOL_ID,
    ] {
        policy.deny.insert(ToolId::new(tool)?);
    }
    Ok(policy)
}

pub(crate) fn resolve_subagent_effective_capabilities(
    spec: &RuntimeSubagentSpec,
    authority_ceiling: &BTreeSet<ToolId>,
    runtime_paths: &AgentLibrePaths,
    workspace_root: &Path,
    trust_store_path: &Path,
    runtime_bundle: Option<&ResolvedRuntimeBundle>,
) -> Result<EffectiveToolSet> {
    let selected_skills = selected_skill_ids(&[], &spec.skills, &[])?;
    let skill_registry = composed_skill_registry(
        runtime_paths,
        workspace_root,
        trust_store_path,
        &selected_skills,
        runtime_bundle,
    )
    .context("failed to load subagent skill registry")?;
    let tool_catalog = crate::tools::chat_extension_catalog()?;
    let selected_extensions = extensions_for_tools(authority_ceiling)?;
    resolve_effective_capabilities(
        &skill_registry,
        &tool_catalog,
        &selected_skills,
        subagent_tool_mode(spec.tool_mode),
        &RuntimePermissionGrantSnapshot::default(),
        Some(subagent_tool_policy(spec)?),
        RuntimeToolBoundary {
            authority_ceiling: Some(authority_ceiling),
            delegation_enabled: !spec.children.is_empty(),
            selected_extensions: selected_extensions.into_iter().collect(),
        },
    )
}

fn selected_skill_hook_batches(
    skill_registry: &agl_skill::SkillRegistry,
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
    skill_registry: &agl_skill::SkillRegistry,
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
        RuntimeToolBoundary::default(),
    )?;
    Ok(visible_tools_from_effective(&effective))
}

#[cfg(test)]
fn selected_skill_visible_tools_with_dynamic_grants(
    skill_registry: &agl_skill::SkillRegistry,
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
        None,
    )?;
    let effective = resolve_effective_capabilities(
        skill_registry,
        tool_catalog,
        selected_skills,
        tool_mode,
        &grant_snapshot,
        None,
        RuntimeToolBoundary::default(),
    )?;
    finalize_permission_grants(store_root, run_id, &effective, &mut grant_snapshot)?;
    Ok((visible_tools_from_effective(&effective), grant_snapshot))
}

#[derive(Clone, Debug)]
struct RuntimeToolBoundary<'a> {
    authority_ceiling: Option<&'a BTreeSet<ToolId>>,
    delegation_enabled: bool,
    selected_extensions: BTreeSet<ExtensionId>,
}

impl Default for RuntimeToolBoundary<'_> {
    fn default() -> Self {
        Self {
            authority_ceiling: None,
            delegation_enabled: false,
            selected_extensions: [
                ExtensionId::new(agl_core_tools::fs::EXTENSION_ID)
                    .expect("core.workspace Extension ID is valid"),
                ExtensionId::new(agl_core_tools::process::EXTENSION_ID)
                    .expect("core.process Extension ID is valid"),
                ExtensionId::new(agl_core_tools::cron::EXTENSION_ID)
                    .expect("core.cron Extension ID is valid"),
                ExtensionId::new(agl_core_tools::memory::EXTENSION_ID)
                    .expect("core.memory Extension ID is valid"),
                ExtensionId::new(agl_core_tools::notes::EXTENSION_ID)
                    .expect("core.note Extension ID is valid"),
                ExtensionId::new(agl_core_tools::permissions::EXTENSION_ID)
                    .expect("core.permission Extension ID is valid"),
                ExtensionId::new(agl_core_tools::repo::EXTENSION_ID)
                    .expect("core.repo Extension ID is valid"),
                ExtensionId::new(agl_core_tools::store::EXTENSION_ID)
                    .expect("core.store Extension ID is valid"),
                ExtensionId::new(agl_core_tools::skills::EXTENSION_ID)
                    .expect("core.skill Extension ID is valid"),
            ]
            .into_iter()
            .collect(),
        }
    }
}

fn resolve_effective_capabilities(
    skill_registry: &agl_skill::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    tool_mode: ToolAccessMode,
    grant_snapshot: &RuntimePermissionGrantSnapshot,
    function_policy: Option<FunctionToolPolicy>,
    boundary: RuntimeToolBoundary<'_>,
) -> Result<EffectiveToolSet> {
    let RuntimeToolBoundary {
        authority_ceiling,
        delegation_enabled,
        selected_extensions,
    } = boundary;
    let mut baseline = core_tool_ids(&selected_extensions)?;
    if delegation_enabled {
        let delegation_id = ToolId::new(crate::delegation_contract::AGENT_DELEGATE_TOOL_ID)?;
        baseline.insert(delegation_id);
    }
    let mut skill_policies = Vec::with_capacity(selected_skills.len());
    for skill_id in selected_skills {
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        let allowed = skill
            .harness
            .allowed_tools
            .iter()
            .filter(|tool_id| core_extension_is_selected(tool_id, &selected_extensions))
            .cloned();
        let requestable = skill
            .harness
            .requestable_tools
            .iter()
            .filter(|tool_id| core_extension_is_selected(tool_id, &selected_extensions))
            .cloned();
        skill_policies.push(
            SkillToolPolicy::new(skill_id.clone(), allowed)
                .with_requestable(requestable)
                .with_denied(skill.harness.denied_tools.iter().cloned()),
        );
    }
    let mut input = ToolPolicyInput::new(
        tool_catalog.extensions().iter().cloned(),
        baseline,
        tool_mode,
    )
    .with_selected_skills(skill_policies)
    .with_grants(grant_snapshot.tool_grants());
    let mut unavailable = BTreeSet::new();
    if !delegation_enabled {
        unavailable.insert(ToolId::new(
            crate::delegation_contract::AGENT_DELEGATE_TOOL_ID,
        )?);
    }
    if !agl_host_tools::screen::extension_available() {
        unavailable.insert(ToolId::new(agl_host_tools::SCREEN_CAPTURE_TOOL_ID)?);
    }
    if !unavailable.is_empty() {
        input = input.with_unavailable_capabilities(unavailable);
    }
    if let Some(function_policy) = function_policy {
        input = input.with_function_policy(function_policy);
    }
    if let Some(authority_ceiling) = authority_ceiling {
        input = input.with_authority_ceiling(authority_ceiling.iter().cloned());
    }
    input.resolve().context("failed to resolve tool policy")
}

pub(crate) fn visible_tools_from_effective(effective: &EffectiveToolSet) -> Vec<VisibleTool> {
    effective
        .tools()
        .map(|tool| VisibleTool::from_declaration(tool.declaration()))
        .collect()
}

fn derive_skill_tool_routing(
    skill_registry: &agl_skill::SkillRegistry,
    selected_skills: &[SkillId],
    effective: &EffectiveToolSet,
) -> Result<SkillToolRoutingView> {
    let permission_request_id = ToolId::new(agl_core_tools::PERMISSIONS_REQUEST_TOOL_ID)?;
    let request_path_effective = effective.contains(&permission_request_id);
    let mut routes = Vec::with_capacity(selected_skills.len());

    for skill_id in selected_skills {
        let skill = skill_registry.resolve_for_context_injection(skill_id)?;
        let requestable_declarations = skill
            .harness
            .requestable_tools
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let declared = skill
            .harness
            .allowed_tools
            .iter()
            .chain(&skill.harness.requestable_tools)
            .chain(&skill.harness.denied_tools)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut callable = BTreeSet::new();
        let mut requestable = BTreeSet::new();
        let mut unavailable = BTreeMap::new();

        for tool_id in declared {
            if effective.contains(&tool_id) {
                callable.insert(tool_id);
                continue;
            }
            let reason = effective
                .exclusion(&tool_id)
                .with_context(|| {
                    format!(
                        "selected skill `{skill_id}` tool `{tool_id}` has no effective-policy outcome"
                    )
                })?
                .reason;
            if request_path_effective
                && requestable_declarations.contains(&tool_id)
                && reason.is_grant_resolvable()
            {
                requestable.insert(tool_id);
            } else {
                unavailable.insert(tool_id, reason);
            }
        }
        routes.push((
            skill_id.clone(),
            SkillToolRouting::new(callable, requestable, unavailable),
        ));
    }

    SkillToolRoutingView::new(routes).context("failed to construct skill tool routing")
}

fn admit_dynamic_permission_grants(
    skill_registry: &agl_skill::SkillRegistry,
    tool_catalog: &ToolCatalog,
    selected_skills: &[SkillId],
    store_root: &std::path::Path,
    workspace_root: &std::path::Path,
    run_id: &RunId,
    session_id: Option<&SessionId>,
) -> Result<RuntimePermissionGrantSnapshot> {
    let store = AglStore::open_at(store_root)
        .with_context(|| format!("failed to open permission store {}", store_root.display()))?;
    let grants = store.active_permission_grants()?;
    let policy = selected_skill_grant_policy(skill_registry, selected_skills)?;
    let mut snapshot = RuntimePermissionGrantSnapshot::default();

    for grant in grants {
        match evaluate_permission_grant(
            &grant,
            tool_catalog,
            &policy,
            workspace_root,
            run_id,
            session_id,
        ) {
            Ok(tool_grant) => {
                let admitted_scope = render_canonical_json(&grant.scope);
                let scope_digest = sha256_text(&admitted_scope);
                snapshot.admitted.push(AdmittedPermissionGrant {
                    grant_id: grant.id,
                    tool_id: tool_grant.tool_id,
                    max_operation_kind: tool_grant.max_operation_kind,
                    state_effects: tool_grant.state_effects,
                    sensitive_inputs: tool_grant.sensitive_inputs,
                    run_id: run_id.clone(),
                    duration: grant.duration,
                    admitted_scope,
                    scope_digest,
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
    effective: &EffectiveToolSet,
    snapshot: &mut RuntimePermissionGrantSnapshot,
) -> Result<()> {
    let store = AglStore::open_at(store_root)
        .with_context(|| format!("failed to open permission store {}", store_root.display()))?;
    let mut admitted = Vec::new();
    for grant in std::mem::take(&mut snapshot.admitted) {
        if effective.contains(&grant.tool_id) {
            store.admit_permission_grant(&grant.grant_id, run_id.as_str())?;
            admitted.push(grant);
        } else {
            let reason = effective
                .exclusion(&grant.tool_id)
                .map(|exclusion| exclusion.reason.code())
                .unwrap_or("tool_not_effective")
                .to_string();
            snapshot.ignored.push(IgnoredPermissionGrant {
                grant_id: grant.grant_id,
                tool_id: grant.tool_id.as_str().to_string(),
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
    allowed_or_requestable: BTreeMap<SkillId, BTreeSet<ToolId>>,
    denied_tools: BTreeSet<ToolId>,
}

fn selected_skill_grant_policy(
    skill_registry: &agl_skill::SkillRegistry,
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
    session_id: Option<&SessionId>,
) -> std::result::Result<ToolGrant, String> {
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
    match grant.duration.as_str() {
        "one_turn" => {
            if let Some(scoped_session_id) = grant
                .scope
                .get("session_id")
                .and_then(|value| value.as_str())
                && session_id.map(SessionId::as_str) != Some(scoped_session_id)
            {
                return Err("session_scope_mismatch".to_string());
            }
        }
        "session" => {
            let current_session = session_id.ok_or_else(|| "session_scope_required".to_string())?;
            let scoped_session = grant
                .scope
                .get("session_id")
                .and_then(|value| value.as_str())
                .ok_or_else(|| "session_scope_required".to_string())?;
            if scoped_session != current_session.as_str() {
                return Err("session_scope_mismatch".to_string());
            }
            if grant.scope.get("run_id").is_some() {
                return Err("session_grant_cannot_have_run_scope".to_string());
            }
        }
        duration => return Err(format!("unsupported_duration_{duration}")),
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
    let tool_grant =
        ToolGrant::new(tool_id, max_operation_kind).with_sensitive_inputs(granted_sensitive_inputs);
    if !grant.state_effects.is_empty() || !declaration.sensitive_inputs.is_empty() {
        let granted_effects = parse_state_effects(&grant.state_effects)?;
        for effect in &declaration.state_effects {
            if !granted_effects.contains(effect) {
                return Err("state_effect_denied".to_string());
            }
        }
        return Ok(tool_grant.with_state_effects(granted_effects));
    }
    Ok(tool_grant)
}

fn parse_operation_kind(value: &str) -> std::result::Result<OperationKind, String> {
    match value {
        "read" => Ok(OperationKind::Read),
        "request" => Ok(OperationKind::Request),
        "write" => Ok(OperationKind::Write),
        "execute" => Ok(OperationKind::Execute),
        "approve" => Ok(OperationKind::Approve),
        "admin" => Ok(OperationKind::Admin),
        _ => Err("invalid_operation_kind".to_string()),
    }
}

fn parse_state_effects(values: &[String]) -> std::result::Result<BTreeSet<EffectId>, String> {
    values
        .iter()
        .map(|value| match value.as_str() {
            "host_screen_capture" => Ok(EffectId::host_screen_capture()),
            "spawn_subagent" => Ok(EffectId::spawn_subagent()),
            "session_working_directory" => Ok(EffectId::session_working_directory()),
            "spawn_process" => Ok(EffectId::spawn_process()),
            "control_process" => Ok(EffectId::control_process()),
            "host_process_execution" => Ok(EffectId::host_process_execution()),
            "shell_login_startup" => Ok(EffectId::shell_login_startup()),
            "repo_files" => Ok(EffectId::repo_files()),
            "repo_workspace" => Ok(EffectId::repo_workspace()),
            "repo_hooks" => Ok(EffectId::repo_hooks()),
            "store_memory_entries" => Ok(EffectId::store_memory_entries()),
            "store_memory_suggestions" => Ok(EffectId::store_memory_suggestions()),
            "store_notes" => Ok(EffectId::store_notes()),
            "store_note_links" => Ok(EffectId::store_note_links()),
            "store_cron" => Ok(EffectId::store_cron()),
            "store_schema" => Ok(EffectId::store_schema()),
            "matrix_outbox" => Ok(EffectId::matrix_outbox()),
            "store_idempotency" => Ok(EffectId::store_idempotency()),
            "store_permission_requests" => Ok(EffectId::store_permission_requests()),
            "store_permission_grants" => Ok(EffectId::store_permission_grants()),
            "skill_trust" => Ok(EffectId::skill_trust()),
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

fn sha256_text(value: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    let digest = Sha256::digest(value.as_bytes());
    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in digest {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn resolve_session_plan(
    function: &ResolvedFunctionPlanInput,
    model: &ResolvedModelPlanInput,
    client: &InferenceClientHandle,
    paths: &AgentLibrePaths,
) -> Result<(
    std::result::Result<ModelExecutionPlan, ModelPlanRejection>,
    Vec<ArtifactFileHandle>,
)> {
    let plan = agl_model::resolve_execution_plan(function, model, &client.static_capabilities()?);
    let artifacts = match &plan {
        Ok(plan) => resolve_plan_artifact_handles(plan, paths)?,
        Err(_) => Vec::new(),
    };
    Ok((plan, artifacts))
}

fn resolve_plan_artifact_handles(
    plan: &ModelExecutionPlan,
    paths: &AgentLibrePaths,
) -> Result<Vec<ArtifactFileHandle>> {
    Ok(agl_model::resolve_installed_plan_files(
        plan,
        &agl_model::ModelInstallStore::new(paths.model_install_root()),
    )?
    .into_iter()
    .map(|file| ArtifactFileHandle {
        role: file.role,
        basename: file.basename,
        path: file.path,
    })
    .collect())
}

fn resolve_request_media(
    request: &InferenceRequest,
    store_root: &Path,
) -> Result<Vec<ResolvedMediaAttachment>> {
    let references = request
        .rendered
        .messages
        .iter()
        .filter_map(|message| message.content.as_ref())
        .flat_map(Content::attachments)
        .cloned()
        .collect::<Vec<_>>();
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let store = AglStore::open_current_at(store_root)
        .context("failed to open content attachment repository for inference")?;
    references
        .into_iter()
        .map(|reference| {
            let resolved = store
                .resolve_content_attachment(&request.run_id, &reference)
                .context("failed to resolve inference content attachment")?;
            ResolvedMediaAttachment::new(resolved.reference, resolved.bytes)
                .map_err(anyhow::Error::from)
        })
        .collect()
}

fn core_tool_ids(selected_extensions: &BTreeSet<ExtensionId>) -> Result<BTreeSet<ToolId>> {
    let mut ids = Vec::new();
    if selected_extensions
        .iter()
        .any(|id| id.as_str() == agl_core_tools::fs::EXTENSION_ID)
    {
        ids.extend([
            agl_core_tools::FS_READ_TOOL_ID,
            agl_core_tools::FS_LIST_TOOL_ID,
            agl_core_tools::FS_SEARCH_TOOL_ID,
            agl_core_tools::FS_APPLY_PATCH_TOOL_ID,
        ]);
    }
    if selected_extensions
        .iter()
        .any(|id| id.as_str() == agl_core_tools::process::EXTENSION_ID)
    {
        ids.extend([
            agl_core_tools::PROCESS_EXEC_TOOL_ID,
            agl_core_tools::SHELL_EXEC_TOOL_ID,
        ]);
    }
    ids.into_iter()
        .map(ToolId::new)
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .context("builtin core tool id is invalid")
}

fn core_extension_is_selected(
    tool_id: &ToolId,
    selected_extensions: &BTreeSet<ExtensionId>,
) -> bool {
    let Some((namespace, _)) = tool_id.as_str().split_once(':') else {
        return true;
    };
    !namespace.starts_with("core.")
        || selected_extensions
            .iter()
            .any(|extension_id| extension_id.as_str() == namespace)
}

fn runtime_function_extensions(function: &RuntimeFunction) -> Result<Vec<ExtensionId>> {
    function
        .extensions
        .iter()
        .map(|id| ExtensionId::new(id).map_err(Into::into))
        .collect()
}

fn bind_runtime_extensions(
    bundle: &ResolvedRuntimeBundle,
) -> Result<BTreeMap<String, RuntimeExtensionExtensionBinding>> {
    let factories = crate::tools::chat_product_factories()
        .into_iter()
        .map(|factory| (factory.key().extension_id.as_str().to_owned(), factory))
        .collect::<BTreeMap<_, _>>();
    let mut bindings = BTreeMap::new();
    for (extension_id, extension) in &bundle.extensions {
        let node = bundle
            .graph
            .nodes
            .get(&extension.node_key)
            .context("selected Extension node is absent from the runtime graph")?;
        ensure!(
            node.candidate.tier == PackageSourceTier::Builtin
                && node.candidate.kind == agl_package::PackageSourceKind::Embedded,
            "Extension `{extension_id}` from source `{}` ({:?}) cannot bind to a builtin extension; no Tool effect occurred",
            node.candidate.source_id,
            node.candidate.tier
        );
        let package_definition = extension.package.definition()?;
        let factory = factories.get(extension_id).with_context(|| {
            format!(
                "Extension `{extension_id}` has no compiled product factory; no Tool effect occurred"
            )
        })?;
        ensure!(
            package_definition.id.as_str() == extension_id
                && package_definition.api_major == factory.key().api_major
                && package_definition.digest() == factory.key().declaration_digest,
            "Extension `{extension_id}` package has no exact compiled factory key; no Tool effect occurred"
        );
        let authored = factory.definition();
        ensure!(
            authored.descriptor().source == ExtensionSource::Builtin
                && authored.descriptor().trust == ExtensionTrust::TrustedByBinary,
            "Extension `{extension_id}` extension is not trusted by the active binary; no Tool effect occurred"
        );
        let mut tools = authored
            .descriptor()
            .tools
            .iter()
            .map(|tool| tool.id.as_str().to_owned())
            .collect::<Vec<_>>();
        tools.sort();
        let mut effects = authored.descriptor().effects.clone();
        effects.sort_by(|left, right| left.id.cmp(&right.id));
        bindings.insert(
            extension_id.clone(),
            RuntimeExtensionExtensionBinding {
                artifact_reference: node.key(),
                artifact_version: node.envelope.version.to_string(),
                package_tree_digest: node.package_tree_digest.to_string(),
                source_tier: node.candidate.tier,
                source_id: node.candidate.source_id.to_string(),
                api_major: authored.api_major,
                declaration_digest: authored.digest().to_string(),
                extension_version: authored.version.clone(),
                runtime_generation_id: bundle.runtime.generation_id.clone(),
                runtime_executable_digest: bundle.runtime.executable_digest.clone(),
                tools,
                effects,
            },
        );
    }
    Ok(bindings)
}

fn extensions_for_tools(tools: &BTreeSet<ToolId>) -> Result<Vec<ExtensionId>> {
    tools
        .iter()
        .filter_map(|tool_id| {
            tool_id
                .as_str()
                .split_once(':')
                .map(|(namespace, _)| namespace)
        })
        .filter(|namespace| namespace.starts_with("core."))
        .map(ExtensionId::new)
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .map(|extensions| extensions.into_iter().collect())
        .map_err(Into::into)
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

fn write_tool_policy_evidence(
    artifact_root: &std::path::Path,
    run_id: &RunId,
    effective: &EffectiveToolSet,
) -> Result<()> {
    let path = InferenceArtifactRoot::new(artifact_root.to_path_buf())
        .run_dir(run_id)
        .join("tool-policy.json");
    let parent = path.parent().with_context(|| {
        format!(
            "tool policy evidence path has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create tool policy evidence directory {}",
            parent.display()
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(effective).with_context(|| {
        format!(
            "failed to serialize tool policy evidence {}",
            path.display()
        )
    })?;
    bytes.push(b'\n');
    std::fs::write(&path, bytes)
        .with_context(|| format!("failed to write tool policy evidence {}", path.display()))
}

#[cfg(test)]
mod tests;
