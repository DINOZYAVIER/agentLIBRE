use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::agent::AgentPackage;
use crate::extension::{ExtensionBindings, ExtensionPackage};
use crate::function::{
    AcceleratorPreference, DeviceSelector, FunctionInference, GpuLayers, KvCacheType, SplitMode,
};
use crate::inference::{
    InferenceConfig, InferenceGenerateRequest, InferenceRoute, InferenceService,
    InferenceServiceError, RestoredInferenceHealth,
};
use crate::model::{
    ImportedModel, ModelArtifactDigest, ModelManifest, fetch_model, open_managed_model,
};
use crate::package::{DirectoryPackageView, PackageId, PackageTreeDigest, PackageVersion};
use crate::skill::SkillPackage;
use agl_core::agent::{
    AbsolutePath, AdmittedTool, AgentDefinitionRef, AgentRunSnapshot, ExactPackageRef,
    ExtensionDefinitionDigest, ExtensionDefinitionRef, GenerationSettings, GpuLayerSelection,
    InstructionBlock, InstructionSet, InstructionSource, KvCacheType as RuntimeKvCacheType,
    ModelAdapterSelection, ModelArtifactKind, ModelArtifactRef, ModelDefinitionRef, ModelDialect,
    ModelGenerationResult, ModelLoadSelection, ModelRuntimeSelection, ModelSelection,
    ModelServiceSelection, PackageDigest, PhysicalDeviceSelector, RelativePath, SkillDefinitionRef,
    SpeculativeSelection, SplitMode as RuntimeSplitMode, ToolCallFormat, ToolDefinitionDigest,
    WorkspaceScope,
};
use agl_core::{AuthorityGrant, AuthorityGrantSet, CanonicalJson, ToolId};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::forge::{ResolvedEntity, ResolvedFunctionSource, resolve_function};

const DEFAULT_CONTEXT_TOKENS: u32 = 4_096;
const DEFAULT_BATCH_SIZE: u32 = 512;
const DEFAULT_UBATCH_SIZE: u32 = 128;
const DEFAULT_SERVICE_SLOTS: u32 = 4;
const DEFAULT_QUEUE_CAPACITY: u32 = 32;

pub struct RuntimeConfig {
    pub data_root: PathBuf,
    pub inference: InferenceConfig,
    pub extensions: Vec<ExtensionBindings>,
    pub max_resident_bytes: Option<u64>,
    pub health: crate::inference::InferenceHealthSink,
}

/// Return the live host budget after the selected safety reserve.
pub fn host_memory_budget(override_keep_free: Option<u64>) -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let total = system.total_memory();
    let available = system.available_memory();
    let automatic = (total / 10).clamp(4 * 1024 * 1024 * 1024, 16 * 1024 * 1024 * 1024);
    available.saturating_sub(override_keep_free.unwrap_or(automatic))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelServiceDisposition {
    New,
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationProgress {
    pub function: String,
    pub dependencies: String,
    pub model_artifact: String,
    pub model_service: String,
    pub runtime_profile: PackageDigest,
    pub active_slots: u32,
    pub continuous_batching: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivatedFunction {
    pub function_id: PackageId,
    pub function_version: PackageVersion,
    pub function_digest: PackageDigest,
    pub snapshot: AgentRunSnapshot,
    pub service: ModelServiceDisposition,
    pub progress: ActivationProgress,
}

pub struct RuntimeService {
    handle: RuntimeHandle,
    inference: Option<InferenceService>,
    eviction: Option<thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct RuntimeHandle {
    shared: Arc<RuntimeShared>,
}

struct RuntimeShared {
    data_root: PathBuf,
    inference: crate::inference::InferenceHandle,
    health: crate::inference::InferenceHealthSink,
    engine_build_digest: PackageDigest,
    extensions: Vec<ExtensionBindings>,
    acquisitions: Mutex<BTreeMap<PackageDigest, Arc<tokio::sync::Mutex<()>>>>,
    verified_models: Mutex<BTreeMap<PackageDigest, ImportedModel>>,
    service_changes: Mutex<()>,
    registry: Mutex<ModelServiceRegistry>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct ModelServiceRegistry {
    entries: BTreeMap<PackageDigest, ModelServiceEntry>,
    max_resident_bytes: u64,
}

struct ModelServiceEntry {
    runtime: ModelRuntimeSelection,
    active: u32,
    last_used_ms: i64,
    keep_warm_until_ms: i64,
    admitted_bytes: u64,
}

struct ModelServiceLease {
    shared: Arc<RuntimeShared>,
    key: PackageDigest,
    idle_timeout_ms: u64,
}

impl Drop for ModelServiceLease {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.shared.registry.lock()
            && let Some(entry) = registry.entries.get_mut(&self.key)
        {
            entry.active = entry.active.saturating_sub(1);
            entry.last_used_ms = unix_ms();
            entry.keep_warm_until_ms = entry.keep_warm_until_ms.max(
                entry
                    .last_used_ms
                    .saturating_add(self.idle_timeout_ms.min(i64::MAX as u64) as i64),
            );
        }
    }
}

impl RuntimeService {
    pub fn start(
        config: RuntimeConfig,
        restored_health: RestoredInferenceHealth,
    ) -> Result<(Self, RuntimeHandle)> {
        ensure!(
            config.data_root.is_absolute(),
            "runtime data root must be absolute"
        );
        std::fs::create_dir_all(&config.data_root)?;
        let data_root = config.data_root.canonicalize()?;
        let engine_build_digest =
            PackageDigest::from_bytes(*config.inference.engine_build_digest().as_bytes());
        let (inference_service, inference) =
            InferenceService::start(config.inference, restored_health)?;
        validate_bindings(&config.extensions)?;
        let max_resident_bytes = config
            .max_resident_bytes
            .unwrap_or_else(|| host_memory_budget(None));
        ensure!(max_resident_bytes > 0, "runtime memory budget is empty");
        let shared = Arc::new(RuntimeShared {
            data_root,
            inference,
            health: config.health,
            engine_build_digest,
            extensions: config.extensions,
            acquisitions: Mutex::new(BTreeMap::new()),
            verified_models: Mutex::new(BTreeMap::new()),
            service_changes: Mutex::new(()),
            registry: Mutex::new(ModelServiceRegistry {
                entries: BTreeMap::new(),
                max_resident_bytes,
            }),
            stopped: AtomicBool::new(false),
        });
        let eviction_shared = Arc::clone(&shared);
        let eviction = thread::Builder::new()
            .name("agl-runtime-eviction".into())
            .spawn(move || eviction_loop(eviction_shared))?;
        let handle = RuntimeHandle { shared };
        Ok((
            Self {
                handle: handle.clone(),
                inference: Some(inference_service),
                eviction: Some(eviction),
            },
            handle,
        ))
    }

    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        if !self.handle.shared.stopped.swap(true, Ordering::AcqRel) {
            if let Some(worker) = self.eviction.take() {
                let _ = worker.join();
            }
            if let Some(inference) = self.inference.take() {
                inference.shutdown();
            }
        }
    }
}

impl Drop for RuntimeService {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

impl RuntimeHandle {
    pub async fn activate(
        &self,
        function_directory: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<ActivatedFunction> {
        ensure!(
            !self.shared.stopped.load(Ordering::Acquire),
            "runtime is stopped"
        );
        let function_directory = function_directory.as_ref().to_path_buf();
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let resolve_workspace = workspace_root.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            resolve_function(function_directory, resolve_workspace)
        })
        .await
        .context("Function source verification task failed")??;
        self.activate_resolved(resolved, workspace_root.as_ref())
            .await
    }

    fn ensure_service(
        &self,
        model: &ModelDefinitionRef,
        runtime: &ModelRuntimeSelection,
    ) -> Result<ModelServiceDisposition, InferenceServiceError> {
        let _change = self
            .shared
            .service_changes
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        let mut registry = self
            .shared
            .registry
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        let now = unix_ms();
        let key = runtime.service.key;
        if let Some(entry) = registry.entries.get_mut(&key) {
            if !same_service_plan(&entry.runtime, runtime) {
                return Err(InferenceServiceError::InvalidRequest);
            }
            entry.keep_warm_until_ms = entry.keep_warm_until_ms.max(
                now.saturating_add(runtime.service.idle_timeout_ms.min(i64::MAX as u64) as i64),
            );
            return Ok(ModelServiceDisposition::Reused);
        }
        drop(registry);
        let artifact = self
            .verified_runtime_artifact(&runtime.artifact)
            .map_err(|_| InferenceServiceError::InvalidRequest)?;
        let adapters = runtime
            .adapters
            .iter()
            .map(|adapter| self.verified_runtime_artifact(&adapter.artifact))
            .collect::<Result<Vec<_>>>()
            .map_err(|_| InferenceServiceError::InvalidRequest)?;
        let mut registry = self
            .shared
            .registry
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        let admitted_bytes = estimated_resident_bytes(runtime);
        let evicted = evict_for_capacity(&registry, admitted_bytes, now)?;
        let mut evicted = evicted
            .into_iter()
            .map(|key| {
                let entry = registry
                    .entries
                    .remove(&key)
                    .expect("capacity candidate came from the registry");
                (key, entry)
            })
            .collect::<VecDeque<_>>();
        drop(registry);
        while let Some((key, entry)) = evicted.pop_front() {
            if let Err(error) = self
                .shared
                .inference
                .unload_service(key, self.shared.health.clone())
            {
                let mut registry = self
                    .shared
                    .registry
                    .lock()
                    .map_err(|_| InferenceServiceError::Unavailable)?;
                registry.entries.insert(key, entry);
                registry.entries.extend(evicted);
                return Err(error);
            }
        }
        self.shared.inference.register_service(
            crate::inference::RegisteredModelService {
                model: model.clone(),
                runtime: runtime.clone(),
                artifact,
                adapters,
            },
            self.shared.health.clone(),
        )?;
        let now = unix_ms();
        let mut registry = self
            .shared
            .registry
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        registry.entries.insert(
            key,
            ModelServiceEntry {
                runtime: runtime.clone(),
                active: 0,
                last_used_ms: now,
                keep_warm_until_ms: now
                    .saturating_add(runtime.service.idle_timeout_ms.min(i64::MAX as u64) as i64),
                admitted_bytes,
            },
        );
        Ok(ModelServiceDisposition::New)
    }

    fn lease_service(
        &self,
        model: &ModelDefinitionRef,
        runtime: &ModelRuntimeSelection,
    ) -> Result<ModelServiceLease, InferenceServiceError> {
        loop {
            self.ensure_service(model, runtime)?;
            let mut registry = self
                .shared
                .registry
                .lock()
                .map_err(|_| InferenceServiceError::Unavailable)?;
            let Some(entry) = registry.entries.get_mut(&runtime.service.key) else {
                continue;
            };
            if !same_service_plan(&entry.runtime, runtime) {
                return Err(InferenceServiceError::InvalidRequest);
            }
            entry.active = entry.active.saturating_add(1);
            entry.last_used_ms = unix_ms();
            entry.keep_warm_until_ms = entry.keep_warm_until_ms.max(
                entry
                    .last_used_ms
                    .saturating_add(runtime.service.idle_timeout_ms.min(i64::MAX as u64) as i64),
            );
            drop(registry);
            return Ok(ModelServiceLease {
                shared: Arc::clone(&self.shared),
                key: runtime.service.key,
                idle_timeout_ms: runtime.service.idle_timeout_ms,
            });
        }
    }

    async fn activate_resolved(
        &self,
        resolved: ResolvedFunctionSource,
        workspace_root: &Path,
    ) -> Result<ActivatedFunction> {
        let workspace = canonical_workspace(workspace_root, &resolved.manifest.working_directory)?;
        let graph = ResolvedGraph::parse(&resolved)?;
        validate_artifact_kinds(&graph, &resolved.manifest.inference)?;
        validate_reasoning_selection(&resolved.manifest.inference, &graph.model.manifest)?;
        let model_artifact = self.acquire_model(&graph.model).await?;
        let mut adapter_artifacts = Vec::new();
        for adapter in &graph.adapters {
            adapter_artifacts.push(self.acquire_model(adapter).await?);
        }
        let model_definition = ModelDefinitionRef {
            id: graph.model.manifest.id.clone(),
            version: graph.model.manifest.version.clone(),
            digest: package_digest(&graph.model.entity.content_digest)?,
        };
        let runtime = realize_model_runtime(
            &resolved.manifest.inference,
            &graph.model,
            &graph.adapters,
            &adapter_artifacts,
            &[],
            self.shared.engine_build_digest,
        )?;
        ensure!(
            model_artifact.digest().to_string() == runtime.artifact.digest.to_string(),
            "acquired model digest differs from realized plan"
        );
        let service = self.ensure_service(&model_definition, &runtime)?;
        let invalid_model_output_recovery = if let (Some(config), Some(model)) = (
            resolved.manifest.recovery.invalid_model_output.as_ref(),
            graph.invalid_model_output_recovery.as_ref(),
        ) {
            validate_reasoning_selection(&config.inference, &model.manifest)?;
            let artifact = self.acquire_model(model).await?;
            let recovery_runtime = realize_model_runtime(
                &config.inference,
                model,
                &[],
                &[],
                &[],
                self.shared.engine_build_digest,
            )?;
            ensure!(
                artifact.digest().to_string() == recovery_runtime.artifact.digest.to_string(),
                "acquired recovery model digest differs from realized plan"
            );
            Some(agl_core::agent::InvalidModelOutputRecoverySelection {
                model: ModelSelection {
                    reasoning_efforts: model.manifest.reasoning.efforts.clone(),
                    model: ModelDefinitionRef {
                        id: model.manifest.id.clone(),
                        version: model.manifest.version.clone(),
                        digest: package_digest(&model.entity.content_digest)?,
                    },
                    runtime: recovery_runtime,
                },
                max_attempts: config.max_attempts,
            })
        } else {
            None
        };
        let snapshot = materialize_snapshot(
            &resolved,
            graph,
            workspace,
            runtime.clone(),
            invalid_model_output_recovery,
            &self.shared.extensions,
        )?;
        let function_digest = package_digest(&resolved.function_digest)?;
        let function = format!(
            "function:{}@={}#{}",
            resolved.manifest.id, resolved.manifest.version, function_digest
        );
        Ok(ActivatedFunction {
            function_id: resolved.manifest.id,
            function_version: resolved.manifest.version,
            function_digest,
            snapshot,
            service,
            progress: ActivationProgress {
                function,
                dependencies: "ready".into(),
                model_artifact: format!("{} ready", runtime.artifact.digest),
                model_service: match service {
                    ModelServiceDisposition::New => "new".into(),
                    ModelServiceDisposition::Reused => "reused".into(),
                },
                runtime_profile: runtime.service.key,
                active_slots: runtime.service.slots,
                continuous_batching: runtime.service.continuous_batching,
            },
        })
    }

    async fn acquire_model(&self, model: &ResolvedModel) -> Result<ImportedModel> {
        let artifact_key = PackageDigest::from_bytes(*model.manifest.artifact.sha256.as_bytes());
        let acquisition = self
            .shared
            .acquisitions
            .lock()
            .map_err(|_| anyhow::anyhow!("model acquisition registry is unavailable"))?
            .entry(artifact_key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _acquisition = acquisition.lock().await;
        let digest_name = model
            .manifest
            .artifact
            .sha256
            .to_string()
            .strip_prefix("sha256:")
            .expect("validated model digest")
            .to_owned();
        let destination = self
            .shared
            .data_root
            .join("models")
            .join(format!("{digest_name}.gguf"));
        if destination.exists() {
            return self.verified_model_at(
                artifact_key,
                &destination,
                model.manifest.artifact.sha256,
                model.manifest.artifact.bytes,
            );
        }
        let request = model.manifest.fetch_request(destination)?;
        let cancelled = AtomicBool::new(false);
        let imported = fetch_model(&request, &cancelled, |_| {}).await?;
        self.remember_verified_model(artifact_key, imported.clone())?;
        Ok(imported)
    }

    fn verified_runtime_artifact(&self, artifact: &ModelArtifactRef) -> Result<ImportedModel> {
        self.verified_model_at(
            artifact.digest,
            &model_cache_path(&self.shared.data_root, artifact),
            ModelArtifactDigest::parse(artifact.digest.to_string())?,
            artifact.bytes,
        )
    }

    fn verified_model_at(
        &self,
        key: PackageDigest,
        path: &Path,
        expected_digest: ModelArtifactDigest,
        expected_bytes: u64,
    ) -> Result<ImportedModel> {
        let cached = self
            .shared
            .verified_models
            .lock()
            .map_err(|_| anyhow::anyhow!("verified model registry is unavailable"))?
            .get(&key)
            .cloned();
        if let Some(cached) = cached
            && cached.digest() == expected_digest
            && cached.bytes() == expected_bytes
            && cached.path() == path
            && cached.verify_unchanged().is_ok()
        {
            return Ok(cached);
        }
        self.shared
            .verified_models
            .lock()
            .map_err(|_| anyhow::anyhow!("verified model registry is unavailable"))?
            .remove(&key);
        let imported = open_managed_model(path, expected_digest, expected_bytes)?;
        self.remember_verified_model(key, imported.clone())?;
        Ok(imported)
    }

    fn remember_verified_model(&self, key: PackageDigest, imported: ImportedModel) -> Result<()> {
        imported.verify_unchanged()?;
        self.shared
            .verified_models
            .lock()
            .map_err(|_| anyhow::anyhow!("verified model registry is unavailable"))?
            .insert(key, imported);
        Ok(())
    }
}

impl InferenceRoute for RuntimeHandle {
    fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(InferenceServiceError::Stopped);
        }
        let _lease = self.lease_service(&request.model, &request.runtime)?;
        self.shared.inference.measure(request)
    }

    fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<ModelGenerationResult, InferenceServiceError> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(InferenceServiceError::Stopped);
        }
        let _lease = self.lease_service(&request.model, &request.runtime)?;
        self.shared.inference.generate(request)
    }
}

struct ResolvedGraph {
    agent: (ResolvedEntity, AgentPackage),
    skills: Vec<(ResolvedEntity, SkillPackage)>,
    model: ResolvedModel,
    invalid_model_output_recovery: Option<ResolvedModel>,
    adapters: Vec<ResolvedModel>,
    extensions: Vec<(ResolvedEntity, ExtensionPackage)>,
}

struct ResolvedModel {
    entity: ResolvedEntity,
    manifest: ModelManifest,
}

impl ResolvedGraph {
    fn parse(source: &ResolvedFunctionSource) -> Result<Self> {
        let mut by_exact = source
            .entities
            .iter()
            .cloned()
            .map(|entity| {
                (
                    (
                        entity.kind.clone(),
                        entity.id.clone(),
                        entity.version.clone(),
                    ),
                    entity,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let take =
            |map: &mut BTreeMap<_, _>, kind: &str, reference: &crate::function::EntityRef| {
                map.remove(&(
                    kind.to_owned(),
                    reference.id.clone(),
                    reference.version.clone(),
                ))
                .with_context(|| format!("resolved graph is missing {}", reference.id))
            };
        let agent_entity = take(&mut by_exact, "agent", &source.manifest.agent)?;
        let agent = crate::agent::parse_package_view(&DirectoryPackageView::new(
            agent_entity.root.clone(),
        )?)?;
        let mut skills = Vec::new();
        for reference in &source.manifest.skills {
            let entity = take(&mut by_exact, "skill", reference)?;
            let package =
                crate::skill::parse_package_view(&DirectoryPackageView::new(entity.root.clone())?)?;
            skills.push((entity, package));
        }
        let model_entity = take(&mut by_exact, "model", &source.manifest.model)?;
        let model = ResolvedModel {
            manifest: crate::model::parse_package_view(&DirectoryPackageView::new(
                model_entity.root.clone(),
            )?)?,
            entity: model_entity,
        };
        let invalid_model_output_recovery = source
            .manifest
            .recovery
            .invalid_model_output
            .as_ref()
            .map(|recovery| {
                let entity = take(&mut by_exact, "model", &recovery.model)?;
                Ok::<_, anyhow::Error>(ResolvedModel {
                    manifest: crate::model::parse_package_view(&DirectoryPackageView::new(
                        entity.root.clone(),
                    )?)?,
                    entity,
                })
            })
            .transpose()?;
        let mut adapters = Vec::new();
        for adapter in &source.manifest.inference.adapters {
            let entity = take(&mut by_exact, "model", &adapter.model)?;
            adapters.push(ResolvedModel {
                manifest: crate::model::parse_package_view(&DirectoryPackageView::new(
                    entity.root.clone(),
                )?)?,
                entity,
            });
        }
        let mut extensions = Vec::new();
        for reference in &source.manifest.extensions {
            let entity = take(&mut by_exact, "extension", reference)?;
            let package = crate::extension::parse_package_view(&DirectoryPackageView::new(
                entity.root.clone(),
            )?)?;
            extensions.push((entity, package));
        }
        ensure!(
            by_exact.is_empty(),
            "resolved graph contains unused entities"
        );
        Ok(Self {
            agent: (agent_entity, agent),
            skills,
            model,
            invalid_model_output_recovery,
            adapters,
            extensions,
        })
    }
}

fn materialize_snapshot(
    source: &ResolvedFunctionSource,
    graph: ResolvedGraph,
    workspace: WorkspaceScope,
    runtime: ModelRuntimeSelection,
    invalid_model_output_recovery: Option<agl_core::agent::InvalidModelOutputRecoverySelection>,
    bindings: &[ExtensionBindings],
) -> Result<AgentRunSnapshot> {
    let agent_digest = package_digest(&graph.agent.0.content_digest)?;
    let agent = AgentDefinitionRef {
        id: graph.agent.1.manifest.id.clone(),
        version: graph.agent.1.manifest.version.clone(),
        digest: agent_digest,
    };
    let mut instructions = vec![InstructionBlock {
        source: InstructionSource::Agent,
        content: agl_core::Content::text(graph.agent.1.instructions.clone())?,
    }];
    let mut required_tools = graph
        .agent
        .1
        .manifest
        .required_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for (entity, skill) in &graph.skills {
        let skill_ref = SkillDefinitionRef {
            id: skill.manifest.id.clone(),
            version: skill.manifest.version.clone(),
            digest: package_digest(&entity.content_digest)?,
        };
        instructions.push(InstructionBlock {
            source: InstructionSource::Skill(skill_ref),
            content: agl_core::Content::text(skill.instructions.clone())?,
        });
        required_tools.extend(skill.manifest.required_tools.iter().cloned());
    }
    let instructions = InstructionSet::new(instructions).map_err(anyhow::Error::msg)?;
    let requirements = source.manifest.requirements()?;
    let admitted_ids = requirements.tools.iter().cloned().collect::<BTreeSet<_>>();
    ensure!(
        required_tools.is_subset(&admitted_ids),
        "Agent or Skill requires a Tool not admitted by the Function"
    );
    let mut declarations = BTreeMap::<ToolId, (&ResolvedEntity, &ExtensionPackage)>::new();
    for (entity, extension) in &graph.extensions {
        for tool in &extension.definition.tools {
            ensure!(
                declarations
                    .insert(tool.id.clone(), (entity, extension))
                    .is_none(),
                "multiple Extensions declare the same Tool"
            );
        }
    }
    let portable_authority = requirements.authority;
    portable_authority.validate().map_err(anyhow::Error::msg)?;
    let authority = resolve_authority(&portable_authority, &graph.extensions, &workspace)?;
    for grant in &authority.0 {
        let (entity, extension) = graph
            .extensions
            .iter()
            .find(|(_, extension)| {
                extension
                    .definition
                    .effects
                    .iter()
                    .any(|effect| effect.id == grant.effect)
            })
            .context("authority grant has no declaring Extension")?;
        let binding = exact_binding(bindings, entity, extension)?;
        ensure!(
            (binding.allows_authority)(grant),
            "trusted local Extension policy denied authority grant {}",
            grant.effect
        );
    }
    let mut tools = Vec::new();
    for tool_id in admitted_ids {
        let (entity, extension) = declarations
            .get(&tool_id)
            .with_context(|| format!("no admitted Extension declares Tool {tool_id}"))?;
        let definition = extension
            .definition
            .tools
            .iter()
            .find(|value| value.id == tool_id)
            .expect("declaration map was built from definitions")
            .clone();
        for effect in &definition.required_effects {
            ensure!(
                portable_authority
                    .0
                    .iter()
                    .any(|grant| &grant.effect == effect),
                "Tool {tool_id} requires missing authority grant {effect}"
            );
        }
        let binding = exact_binding(bindings, entity, extension)?;
        let tool_binding = binding
            .tools
            .iter()
            .find(|value| value.tool_id == tool_id)
            .with_context(|| format!("trusted binding is missing Tool {tool_id}"))?;
        let definition_digest = ToolDefinitionDigest::from_bytes(
            Sha256::digest(serde_json::to_vec(&definition)?).into(),
        );
        ensure!(
            tool_binding.definition_digest == definition_digest,
            "trusted Tool binding digest differs for {tool_id}"
        );
        tools.push(AdmittedTool {
            definition,
            extension: ExtensionDefinitionRef {
                id: extension.definition.id.clone(),
                package: ExactPackageRef {
                    id: extension.manifest.id.clone(),
                    version: extension.manifest.version.clone(),
                    digest: package_digest(&entity.content_digest)?,
                },
                definition_digest: ExtensionDefinitionDigest::from_bytes(
                    Sha256::digest(serde_json::to_vec(&extension.definition)?).into(),
                ),
            },
            definition_digest,
        });
    }
    tools.sort_by(|left, right| left.definition.id.cmp(&right.definition.id));
    Ok(AgentRunSnapshot {
        agent,
        model: ModelSelection {
            reasoning_efforts: graph.model.manifest.reasoning.efforts,
            model: ModelDefinitionRef {
                id: graph.model.manifest.id,
                version: graph.model.manifest.version,
                digest: package_digest(&graph.model.entity.content_digest)?,
            },
            runtime,
        },
        invalid_model_output_recovery,
        instructions,
        workspace,
        tools,
        authority,
        limits: source.manifest.limits.agent_run_limits(),
        presentation: agl_core::agent::AgentPresentation {
            tool_output: agl_core::agent::ToolOutputPresentation {
                lines: source.manifest.presentation.tool_output.lines,
                chars: source.manifest.presentation.tool_output.chars,
            },
            tool: agl_core::agent::ToolPresentation {
                frame: source.manifest.presentation.tool.frame,
            },
            colors: source.manifest.presentation.colors.clone(),
            model_generation: agl_core::agent::ModelGenerationPresentation {
                details: source.manifest.presentation.model_generation.details,
            },
        },
        response_format: None,
        planner_read_only: false,
    })
}

fn exact_binding<'a>(
    bindings: &'a [ExtensionBindings],
    entity: &ResolvedEntity,
    extension: &ExtensionPackage,
) -> Result<&'a ExtensionBindings> {
    let candidates = bindings
        .iter()
        .filter(|binding| {
            binding.definition.id == extension.definition.id
                && binding.version == extension.manifest.version
        })
        .collect::<Vec<_>>();
    ensure!(
        candidates.len() == 1,
        "Extension {}@{} requires exactly one trusted local binding",
        extension.manifest.id,
        extension.manifest.version
    );
    let binding = candidates[0];
    ensure!(
        binding.definition == extension.definition,
        "trusted local Extension declaration differs from the locked declaration"
    );
    ensure!(
        binding.content_digest == entity.content_digest,
        "trusted local Extension content digest differs from the Forge declaration"
    );
    Ok(binding)
}

fn resolve_authority(
    authority: &AuthorityGrantSet,
    extensions: &[(ResolvedEntity, ExtensionPackage)],
    workspace: &WorkspaceScope,
) -> Result<AuthorityGrantSet> {
    let schemas = extensions
        .iter()
        .flat_map(|(_, extension)| &extension.definition.effects)
        .map(|effect| (effect.id.clone(), &effect.scope_schema))
        .collect::<BTreeMap<_, _>>();
    let mut realized = Vec::with_capacity(authority.0.len());
    for grant in &authority.0 {
        let schema = schemas
            .get(&grant.effect)
            .with_context(|| format!("authority grant names undeclared Effect {}", grant.effect))?;
        schema.validate(&grant.scope).map_err(anyhow::Error::msg)?;
        let mut value = grant.scope.as_value().clone();
        if let Some(root) = value.get_mut("root") {
            ensure!(
                root.as_str() == Some("workspace"),
                "authority root must be `workspace`"
            );
            *root =
                serde_json::Value::String(workspace.root.as_path().to_string_lossy().into_owned());
        }
        realized.push(AuthorityGrant {
            effect: grant.effect.clone(),
            scope: CanonicalJson::new(value).map_err(anyhow::Error::msg)?,
        });
    }
    realized.sort_by(|left, right| {
        (
            left.effect.as_str(),
            serde_json::to_string(&left.scope).expect("CanonicalJson serializes"),
        )
            .cmp(&(
                right.effect.as_str(),
                serde_json::to_string(&right.scope).expect("CanonicalJson serializes"),
            ))
    });
    let authority = AuthorityGrantSet(realized);
    authority.validate().map_err(anyhow::Error::msg)?;
    Ok(authority)
}

fn canonical_workspace(root: &Path, working_directory: &str) -> Result<WorkspaceScope> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace {}", root.display()))?;
    ensure!(root.is_dir(), "workspace root is not a directory");
    let relative =
        RelativePath::try_from(working_directory.to_owned()).map_err(anyhow::Error::msg)?;
    let working = root.join(relative.as_path()).canonicalize()?;
    ensure!(
        working.is_dir() && working.starts_with(&root),
        "Function working directory escapes or is missing from the workspace"
    );
    Ok(WorkspaceScope {
        root: AbsolutePath::try_from(root.to_string_lossy().into_owned())
            .map_err(anyhow::Error::msg)?,
        working_directory: relative,
    })
}

fn realize_model_runtime(
    inference: &FunctionInference,
    model: &ResolvedModel,
    adapters: &[ResolvedModel],
    acquired_adapters: &[ImportedModel],
    _host_profiles: &[crate::inference::LlamaRuntimeProfile],
    actual_engine_build_digest: PackageDigest,
) -> Result<ModelRuntimeSelection> {
    ensure!(
        adapters.len() == acquired_adapters.len(),
        "adapter acquisition mismatch"
    );
    let generation = &inference.generation;
    let generation = GenerationSettings {
        max_output_tokens: generation.max_output_tokens,
        seed: generation.seed,
        temperature: generation.temperature,
        top_k: generation.top_k,
        top_p: generation.top_p,
        min_p: generation.min_p,
        typical_p: generation.typical_p,
        repeat_last_n: generation.repeat_last_n,
        repeat_penalty: generation.repeat_penalty,
        presence_penalty: generation.presence_penalty,
        frequency_penalty: generation.frequency_penalty,
        stop: generation.stop.clone(),
    };
    let reasoning = if inference.reasoning.enabled {
        agl_core::agent::ReasoningSelection::Enabled {
            max_tokens: inference
                .reasoning
                .max_tokens
                .context("enabled reasoning omitted max_tokens")?,
            effort: inference.reasoning.default,
            preserve: inference
                .reasoning
                .preserve
                .context("enabled reasoning omitted preserve")?,
        }
    } else {
        agl_core::agent::ReasoningSelection::Disabled
    };
    let requested = &inference.load;
    let context_tokens = requested
        .context_tokens
        .or(requested.min_context_tokens)
        .unwrap_or(u64::from(DEFAULT_CONTEXT_TOKENS));
    let context_tokens = u32::try_from(context_tokens).context("context_tokens exceeds u32")?;
    let batch_size = requested.batch_size.unwrap_or(DEFAULT_BATCH_SIZE);
    let ubatch_size = requested
        .ubatch_size
        .unwrap_or(DEFAULT_UBATCH_SIZE.min(batch_size));
    let threads = requested.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|value| value.get().min(u32::MAX as usize) as u32)
            .unwrap_or(1)
    });
    if let Some(authored) = requested.engine_build_digest.as_deref() {
        ensure!(
            PackageDigest::from_bytes(parse_hex_digest(authored)?) == actual_engine_build_digest,
            "authored engine_build_digest is unavailable on this host"
        );
    }
    let load = ModelLoadSelection {
        context_tokens,
        batch_size,
        ubatch_size,
        threads,
        threads_batch: requested.threads_batch.unwrap_or(threads),
        gpu_layers: match requested.gpu_layers {
            Some(GpuLayers::All) => GpuLayerSelection::All,
            Some(GpuLayers::Count(value)) => GpuLayerSelection::Count(value),
            None => match requested.accelerator {
                AcceleratorPreference::Cpu => GpuLayerSelection::Count(0),
                _ => GpuLayerSelection::All,
            },
        },
        devices: if requested.devices.is_empty() {
            Vec::new()
        } else {
            requested.devices.iter().map(device_selector).collect()
        },
        split_mode: match requested.split_mode {
            SplitMode::None => RuntimeSplitMode::None,
            SplitMode::Layer => RuntimeSplitMode::Layer,
            SplitMode::Row => RuntimeSplitMode::Row,
        },
        main_gpu: requested.main_gpu,
        tensor_split: normalize_tensor_split(&requested.tensor_split)?,
        mmap: requested.mmap.unwrap_or(true),
        mlock: requested.mlock.unwrap_or(false),
        flash_attention: requested.flash_attention.unwrap_or(false),
        kv_cache_type_k: requested
            .kv_cache_type_k
            .map(runtime_kv)
            .unwrap_or(RuntimeKvCacheType::F16),
        kv_cache_type_v: requested
            .kv_cache_type_v
            .map(runtime_kv)
            .unwrap_or(RuntimeKvCacheType::F16),
        engine_build_digest: actual_engine_build_digest,
    };
    let adapters = adapters
        .iter()
        .zip(acquired_adapters)
        .zip(&inference.adapters)
        .map(|((model, acquired), authored)| {
            ensure!(
                acquired.digest() == model.manifest.artifact.sha256,
                "adapter digest mismatch"
            );
            Ok(ModelAdapterSelection {
                model: ModelDefinitionRef {
                    id: model.manifest.id.clone(),
                    version: model.manifest.version.clone(),
                    digest: package_digest(&model.entity.content_digest)?,
                },
                artifact: artifact_ref(&model.manifest)?,
                scale: authored.scale,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let slots = inference.service.slots.unwrap_or(DEFAULT_SERVICE_SLOTS);
    let queue_capacity = inference
        .service
        .queue_capacity
        .unwrap_or(DEFAULT_QUEUE_CAPACITY);
    let artifact = artifact_ref(&model.manifest)?;
    let speculative = inference.speculative.map(|selection| match selection.mode {
        crate::function::FunctionSpeculativeMode::Mtp => SpeculativeSelection::Mtp {
            max_draft_tokens: selection.max_draft_tokens,
            kv_cache_type_k: runtime_kv(selection.kv_cache_type_k),
            kv_cache_type_v: runtime_kv(selection.kv_cache_type_v),
        },
    });
    let key = service_key(
        &artifact,
        &load,
        speculative,
        &adapters,
        slots,
        queue_capacity,
        inference.service.continuous_batching,
    )?;
    Ok(ModelRuntimeSelection {
        artifact,
        dialect: match model.manifest.dialect {
            crate::model::ModelDialect::Generic => ModelDialect::Generic,
            crate::model::ModelDialect::Qwen3 => ModelDialect::Qwen3,
            crate::model::ModelDialect::Gemma4 => ModelDialect::Gemma4,
        },
        tool_call_format: match model.manifest.tool_call_format {
            crate::model::ToolCallFormat::StructuredToolCalls => {
                ToolCallFormat::StructuredToolCalls
            }
            crate::model::ToolCallFormat::HermesJson => ToolCallFormat::HermesJson,
            crate::model::ToolCallFormat::GemmaAgentCall => ToolCallFormat::GemmaAgentCall,
        },
        generation,
        reasoning,
        load,
        speculative,
        adapters,
        service: ModelServiceSelection {
            key,
            slots,
            queue_capacity,
            continuous_batching: inference.service.continuous_batching,
            idle_timeout_ms: inference.service.idle_timeout.as_millis(),
        },
    })
}

fn validate_reasoning_selection(
    inference: &FunctionInference,
    model: &ModelManifest,
) -> Result<()> {
    if !inference.reasoning.enabled {
        return Ok(());
    }
    if let Some(effort) = inference.reasoning.default {
        ensure!(
            model.reasoning.efforts.contains(&effort),
            "Model does not declare support for reasoning effort {effort:?}"
        );
    }
    if inference.reasoning.preserve == Some(true) {
        ensure!(
            model.reasoning.preserve,
            "Model does not declare support for preserved reasoning"
        );
    }
    Ok(())
}

fn service_key(
    artifact: &ModelArtifactRef,
    load: &ModelLoadSelection,
    speculative: Option<SpeculativeSelection>,
    adapters: &[ModelAdapterSelection],
    slots: u32,
    queue_capacity: u32,
    continuous_batching: bool,
) -> Result<PackageDigest> {
    #[derive(Serialize)]
    struct ArtifactKey {
        kind: ModelArtifactKind,
        digest: PackageDigest,
        bytes: u64,
    }
    #[derive(Serialize)]
    struct AdapterKey {
        artifact: ArtifactKey,
        scale: f64,
    }
    #[derive(Serialize)]
    struct Key<'a> {
        artifact: ArtifactKey,
        load: &'a ModelLoadSelection,
        speculative: Option<SpeculativeSelection>,
        adapters: Vec<AdapterKey>,
        slots: u32,
        queue_capacity: u32,
        continuous_batching: bool,
    }
    let bytes = serde_json::to_vec(&Key {
        artifact: ArtifactKey {
            kind: artifact.kind,
            digest: artifact.digest,
            bytes: artifact.bytes,
        },
        load,
        speculative,
        adapters: adapters
            .iter()
            .map(|adapter| AdapterKey {
                artifact: ArtifactKey {
                    kind: adapter.artifact.kind,
                    digest: adapter.artifact.digest,
                    bytes: adapter.artifact.bytes,
                },
                scale: adapter.scale,
            })
            .collect(),
        slots,
        queue_capacity,
        continuous_batching,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.model-service-key.v1\0");
    hasher.update(bytes);
    Ok(PackageDigest::from_bytes(hasher.finalize().into()))
}

fn same_service_plan(left: &ModelRuntimeSelection, right: &ModelRuntimeSelection) -> bool {
    left.shares_service_with(right)
}

fn estimated_resident_bytes(runtime: &ModelRuntimeSelection) -> u64 {
    let gpu = !matches!(runtime.load.gpu_layers, GpuLayerSelection::Count(0));
    let artifacts = runtime.artifact.bytes.saturating_add(
        runtime
            .adapters
            .iter()
            .map(|adapter| adapter.artifact.bytes)
            .sum(),
    );
    let contexts = u64::from(runtime.load.context_tokens)
        .saturating_mul(u64::from(runtime.service.slots))
        .saturating_mul(if gpu { 128 * 1024 } else { 256 * 1024 });
    artifacts
        .saturating_mul(if gpu { 1 } else { 2 })
        .saturating_add(contexts)
        .saturating_add(1024 * 1024 * 1024)
}

fn normalize_tensor_split(values: &[f64]) -> Result<Vec<f64>> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let sum = values.iter().sum::<f64>();
    ensure!(sum.is_finite() && sum > 0.0, "tensor_split sum is invalid");
    let mut normalized = values
        .iter()
        .map(|value| ((value / sum) * 1_000_000_000_000_f64).round() / 1_000_000_000_000_f64)
        .collect::<Vec<_>>();
    let prefix = normalized[..normalized.len() - 1].iter().sum::<f64>();
    *normalized.last_mut().expect("nonempty tensor split") = 1.0 - prefix;
    ensure!(
        normalized
            .iter()
            .all(|value| value.is_finite() && *value > 0.0),
        "normalized tensor_split contains a nonpositive value"
    );
    Ok(normalized)
}

fn artifact_ref(manifest: &ModelManifest) -> Result<ModelArtifactRef> {
    Ok(ModelArtifactRef {
        kind: match manifest.artifact.kind {
            crate::model::ModelArtifactKind::Gguf => ModelArtifactKind::Gguf,
            crate::model::ModelArtifactKind::LoraAdapter => ModelArtifactKind::LoraAdapter,
        },
        url: manifest.artifact.url.clone(),
        digest: PackageDigest::from_bytes(*manifest.artifact.sha256.as_bytes()),
        bytes: manifest.artifact.bytes,
    })
}

fn device_selector(value: &DeviceSelector) -> PhysicalDeviceSelector {
    match value {
        DeviceSelector::Pci { address } => PhysicalDeviceSelector::Pci {
            address: address.to_ascii_lowercase(),
        },
        DeviceSelector::Uuid { uuid } => PhysicalDeviceSelector::Uuid {
            uuid: uuid.to_ascii_lowercase(),
        },
    }
}

fn runtime_kv(value: KvCacheType) -> RuntimeKvCacheType {
    match value {
        KvCacheType::F32 => RuntimeKvCacheType::F32,
        KvCacheType::F16 => RuntimeKvCacheType::F16,
        KvCacheType::Bf16 => RuntimeKvCacheType::Bf16,
        KvCacheType::Q8_0 => RuntimeKvCacheType::Q8_0,
        KvCacheType::Q5_0 => RuntimeKvCacheType::Q5_0,
        KvCacheType::Q5_1 => RuntimeKvCacheType::Q5_1,
        KvCacheType::Q4_0 => RuntimeKvCacheType::Q4_0,
        KvCacheType::Q4_1 => RuntimeKvCacheType::Q4_1,
        KvCacheType::Iq4Nl => RuntimeKvCacheType::Iq4Nl,
    }
}

fn validate_artifact_kinds(graph: &ResolvedGraph, inference: &FunctionInference) -> Result<()> {
    ensure!(
        graph.model.manifest.artifact.kind == crate::model::ModelArtifactKind::Gguf,
        "Function base Model must use a GGUF artifact"
    );
    ensure!(
        graph
            .invalid_model_output_recovery
            .as_ref()
            .is_none_or(
                |model| model.manifest.artifact.kind == crate::model::ModelArtifactKind::Gguf
            ),
        "invalid-model-output recovery Model must use a GGUF artifact"
    );
    ensure!(
        graph.adapters.len() == inference.adapters.len()
            && graph
                .adapters
                .iter()
                .all(|model| model.manifest.artifact.kind
                    == crate::model::ModelArtifactKind::LoraAdapter),
        "Function adapters must use LoRA adapter artifacts"
    );
    Ok(())
}

fn validate_bindings(bindings: &[ExtensionBindings]) -> Result<()> {
    let mut identities = BTreeSet::new();
    for binding in bindings {
        binding.definition.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            identities.insert((binding.definition.id.clone(), binding.version.clone())),
            "duplicate trusted Extension binding"
        );
    }
    Ok(())
}

fn package_digest(value: &PackageTreeDigest) -> Result<PackageDigest> {
    Ok(PackageDigest::from_bytes(parse_prefixed_digest(
        value.as_str(),
    )?))
}

fn parse_prefixed_digest(value: &str) -> Result<[u8; 32]> {
    parse_hex_digest(
        value
            .strip_prefix("sha256:")
            .context("digest requires sha256 prefix")?,
    )
}

fn parse_hex_digest(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64,
        "digest must contain 64 hexadecimal characters"
    );
    let mut bytes = [0; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        bytes[index] = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(bytes)
}

fn evict_for_capacity(
    registry: &ModelServiceRegistry,
    requested: u64,
    now: i64,
) -> Result<Vec<PackageDigest>, InferenceServiceError> {
    if requested > registry.max_resident_bytes {
        return Err(InferenceServiceError::Unavailable);
    }
    let mut resident = registry.entries.values().fold(0_u64, |total, entry| {
        total.saturating_add(entry.admitted_bytes)
    });
    if resident.saturating_add(requested) <= registry.max_resident_bytes {
        return Ok(Vec::new());
    }
    let mut candidates = registry
        .entries
        .iter()
        .filter(|(_, entry)| entry.active == 0)
        .map(|(key, entry)| (*key, entry))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(_, entry)| (entry.keep_warm_until_ms > now, entry.last_used_ms));
    let mut evicted = Vec::new();
    for (key, entry) in candidates {
        resident = resident.saturating_sub(entry.admitted_bytes);
        evicted.push(key);
        if resident.saturating_add(requested) <= registry.max_resident_bytes {
            return Ok(evicted);
        }
    }
    Err(InferenceServiceError::Unavailable)
}

fn eviction_loop(shared: Arc<RuntimeShared>) {
    while !shared.stopped.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(250));
        let now = unix_ms();
        if let Ok(_change) = shared.service_changes.lock() {
            let Ok(mut registry) = shared.registry.lock() else {
                continue;
            };
            let keys = registry
                .entries
                .iter()
                .filter(|(_, entry)| entry.active == 0 && entry.keep_warm_until_ms <= now)
                .map(|(key, _)| *key)
                .collect::<Vec<_>>();
            let mut expired = keys
                .into_iter()
                .map(|key| {
                    let entry = registry
                        .entries
                        .remove(&key)
                        .expect("expiration candidate came from the registry");
                    (key, entry)
                })
                .collect::<VecDeque<_>>();
            drop(registry);
            while let Some((key, entry)) = expired.pop_front() {
                if shared
                    .inference
                    .unload_service(key, shared.health.clone())
                    .is_err()
                {
                    if let Ok(mut registry) = shared.registry.lock() {
                        registry.entries.insert(key, entry);
                        registry.entries.extend(expired);
                    }
                    break;
                }
            }
        }
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn model_cache_path(data_root: &Path, artifact: &ModelArtifactRef) -> PathBuf {
    let digest = artifact
        .digest
        .to_string()
        .strip_prefix("sha256:")
        .expect("PackageDigest has a sha256 prefix")
        .to_owned();
    data_root.join("models").join(format!("{digest}.gguf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_selection() -> ModelRuntimeSelection {
        ModelRuntimeSelection {
            artifact: ModelArtifactRef {
                kind: ModelArtifactKind::Gguf,
                url: "https://example.invalid/model.gguf".to_owned(),
                digest: PackageDigest::from_bytes([1; 32]),
                bytes: 1024,
            },
            dialect: ModelDialect::Generic,
            tool_call_format: ToolCallFormat::HermesJson,
            generation: GenerationSettings {
                max_output_tokens: 32,
                seed: 1,
                temperature: 0.0,
                top_k: 1,
                top_p: 1.0,
                min_p: 0.0,
                typical_p: 1.0,
                repeat_last_n: 64,
                repeat_penalty: 1.0,
                presence_penalty: 0.0,
                frequency_penalty: 0.0,
                stop: Vec::new(),
            },
            reasoning: agl_core::agent::ReasoningSelection::Disabled,
            speculative: None,
            load: ModelLoadSelection {
                context_tokens: 4096,
                batch_size: 512,
                ubatch_size: 128,
                threads: 4,
                threads_batch: 4,
                gpu_layers: GpuLayerSelection::Count(0),
                devices: Vec::new(),
                split_mode: RuntimeSplitMode::None,
                main_gpu: None,
                tensor_split: Vec::new(),
                mmap: true,
                mlock: false,
                flash_attention: false,
                kv_cache_type_k: RuntimeKvCacheType::F16,
                kv_cache_type_v: RuntimeKvCacheType::F16,
                engine_build_digest: PackageDigest::from_bytes([2; 32]),
            },
            adapters: Vec::new(),
            service: ModelServiceSelection {
                key: PackageDigest::from_bytes([3; 32]),
                slots: 4,
                queue_capacity: 32,
                continuous_batching: true,
                idle_timeout_ms: 900_000,
            },
        }
    }

    #[test]
    fn request_reasoning_does_not_split_a_shared_model_service() {
        let disabled = runtime_selection();
        let mut enabled = disabled.clone();
        enabled.reasoning = agl_core::agent::ReasoningSelection::Enabled {
            max_tokens: 16,
            effort: None,
            preserve: false,
        };

        assert!(disabled.shares_service_with(&enabled));
        assert!(enabled.shares_service_with(&disabled));
    }

    #[test]
    fn resident_admission_uses_the_selected_host_memory_domain() {
        let cpu = runtime_selection();
        assert_eq!(
            estimated_resident_bytes(&cpu),
            2 * 1024 + 4096 * 4 * 256 * 1024 + 1024 * 1024 * 1024
        );

        let mut gpu = cpu;
        gpu.load.gpu_layers = GpuLayerSelection::All;
        assert_eq!(
            estimated_resident_bytes(&gpu),
            1024 + 4096 * 4 * 128 * 1024 + 1024 * 1024 * 1024
        );
    }

    #[test]
    fn model_capabilities_reject_unsupported_reasoning_before_service_registration() {
        let mut inference = FunctionInference::default();
        inference.reasoning.enabled = true;
        inference.reasoning.max_tokens = Some(16);
        inference.reasoning.default = Some(agl_core::agent::ReasoningEffort::Xhigh);
        inference.reasoning.preserve = Some(true);
        let mut model = ModelManifest::parse(&format!(
            "schema = \"agentlibre.model/v1\"\nid = \"model\"\nversion = \"1.0.0\"\ndialect = \"qwen3\"\ntool_call_format = \"structured_tool_calls\"\n\n[artifact]\nkind = \"gguf\"\nurl = \"https://example.invalid/model.gguf\"\nsha256 = \"sha256:{}\"\nbytes = 1\n",
            "01".repeat(32)
        ))
        .unwrap();
        assert!(validate_reasoning_selection(&inference, &model).is_err());
        model.reasoning.efforts = vec![agl_core::agent::ReasoningEffort::Xhigh];
        assert!(validate_reasoning_selection(&inference, &model).is_err());
        model.reasoning.preserve = true;
        assert!(validate_reasoning_selection(&inference, &model).is_ok());
    }

    #[test]
    fn equivalent_tensor_ratios_share_one_normalized_shape() {
        assert_eq!(
            normalize_tensor_split(&[3.0, 1.0]).unwrap(),
            normalize_tensor_split(&[0.75, 0.25]).unwrap()
        );
    }

    #[test]
    fn request_only_settings_and_idle_timeout_do_not_split_a_service() {
        let left = runtime_selection();
        let mut right = left.clone();
        right.generation.temperature = 0.8;
        right.generation.stop.push("done".to_owned());
        right.dialect = ModelDialect::Qwen3;
        right.tool_call_format = ToolCallFormat::StructuredToolCalls;
        right.service.idle_timeout_ms = 60_000;
        right.artifact.url = "https://mirror.invalid/model.gguf".to_owned();
        assert!(same_service_plan(&left, &right));

        right.artifact.bytes += 1;
        assert!(!same_service_plan(&left, &right));
        right.artifact.bytes -= 1;
        right.load.context_tokens += 1;
        assert!(!same_service_plan(&left, &right));
    }

    #[test]
    fn capacity_eviction_never_selects_an_active_service() {
        let now = 1_000;
        let active_runtime = runtime_selection();
        let active_key = active_runtime.service.key;
        let mut idle_runtime = active_runtime.clone();
        idle_runtime.service.key = PackageDigest::from_bytes([4; 32]);
        let idle_key = idle_runtime.service.key;
        let registry = ModelServiceRegistry {
            max_resident_bytes: 120,
            entries: BTreeMap::from([
                (
                    active_key,
                    ModelServiceEntry {
                        runtime: active_runtime.clone(),
                        active: 1,
                        last_used_ms: 10,
                        keep_warm_until_ms: 10,
                        admitted_bytes: 60,
                    },
                ),
                (
                    idle_key,
                    ModelServiceEntry {
                        runtime: idle_runtime,
                        active: 0,
                        last_used_ms: 20,
                        keep_warm_until_ms: 20,
                        admitted_bytes: 60,
                    },
                ),
            ]),
        };
        assert_eq!(
            evict_for_capacity(&registry, 50, now).unwrap(),
            vec![idle_key]
        );

        let active_only = ModelServiceRegistry {
            max_resident_bytes: 100,
            entries: BTreeMap::from([(
                active_key,
                ModelServiceEntry {
                    runtime: active_runtime,
                    active: 1,
                    last_used_ms: 10,
                    keep_warm_until_ms: 10,
                    admitted_bytes: 60,
                },
            )]),
        };
        assert_eq!(
            evict_for_capacity(&active_only, 50, now).unwrap_err(),
            InferenceServiceError::Unavailable
        );
    }
}
