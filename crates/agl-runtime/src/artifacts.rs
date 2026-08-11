use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agl_artifact::{ArtifactBinding, ArtifactHandle};
use agl_extension::package::{ExtensionPackage, ExtensionPackageAdapter};
use agl_function::FunctionPackageAdapter;
use agl_model::{ModelPackage, ModelPackageAdapter, ModelPackageProvenance};
use agl_package::{
    DirectoryPackageSource, DirectoryPackageView, InMemoryPackageView, PackageAdapter,
    PackageAdapterRegistry, PackageCandidate, PackageLock, PackagePathRouter, PackageRef,
    PackageResolver, PackageSource, PackageSourceId, PackageSourceKind, PackageSourceTier,
    PackageTreeDigest, PackageTypeId, ResolvedPackage, ResolvedPackageGraph, StaticPackageSource,
    WorkspaceManifest,
};
use agl_skill::{
    RegisteredSkill, SkillHarness, SkillPackageAdapter, SkillRegistry, SkillSource, SkillTrustStore,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::AgentLibrePaths;

#[derive(Clone, Debug)]
pub struct ArtifactBindingInput {
    declarations: Vec<agl_kernel::ArtifactDeclaration>,
    bindings: Vec<ArtifactBinding>,
}

impl ArtifactBindingInput {
    pub fn new(
        declarations: impl IntoIterator<Item = agl_kernel::ArtifactDeclaration>,
        bindings: impl IntoIterator<Item = ArtifactBinding>,
    ) -> Self {
        Self {
            declarations: declarations.into_iter().collect(),
            bindings: bindings.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArtifactCompositionError {
    #[error("Artifact `{artifact_id}` has no binding")]
    MissingBinding { artifact_id: agl_kernel::ArtifactId },
    #[error("Artifact binding `{artifact_id}` is not verified")]
    UnverifiedBinding { artifact_id: agl_kernel::ArtifactId },
    #[error("duplicate Artifact binding path `{path}`")]
    DuplicateBindingPath { path: PathBuf },
    #[error("Artifact binding `{artifact_id}` has no matching declaration")]
    UndeclaredBinding { artifact_id: agl_kernel::ArtifactId },
    #[error("Artifact handle binding failed: {0}")]
    Handle(String),
}

pub fn bind_artifact_handles(
    input: ArtifactBindingInput,
) -> Result<Vec<ArtifactHandle>, ArtifactCompositionError> {
    let mut by_id = input
        .bindings
        .into_iter()
        .map(|binding| (binding.artifact_id().clone(), binding))
        .collect::<BTreeMap<_, _>>();
    let mut paths = BTreeSet::new();
    for binding in by_id.values() {
        if !paths.insert(binding.submodule_path().to_path_buf()) {
            return Err(ArtifactCompositionError::DuplicateBindingPath {
                path: binding.submodule_path().to_path_buf(),
            });
        }
    }
    let mut handles = Vec::new();
    for declaration in input.declarations {
        let binding = by_id.remove(&declaration.id).ok_or_else(|| {
            ArtifactCompositionError::MissingBinding {
                artifact_id: declaration.id.clone(),
            }
        })?;
        if !binding.is_verified() {
            return Err(ArtifactCompositionError::UnverifiedBinding {
                artifact_id: declaration.id,
            });
        }
        handles.push(
            ArtifactHandle::bind(declaration, binding)
                .map_err(|error| ArtifactCompositionError::Handle(error.to_string()))?,
        );
    }
    if let Some((artifact_id, _)) = by_id.into_iter().next() {
        return Err(ArtifactCompositionError::UndeclaredBinding { artifact_id });
    }
    Ok(handles)
}

#[derive(Clone)]
pub struct PackageComposition {
    pub registry: Arc<PackageAdapterRegistry>,
    pub sources: Vec<Arc<dyn PackageSource>>,
    pub router: PackagePathRouter,
    pub lock: Option<PackageLock>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceSkillRegistry {
    pub registry: SkillRegistry,
    pub package_lock_present: bool,
    pub external_package_count: usize,
}

pub fn resolve_workspace_skills(
    paths: &AgentLibrePaths,
    workspace_root: impl Into<PathBuf>,
    trust_store_path: impl AsRef<Path>,
) -> Result<WorkspaceSkillRegistry> {
    let workspace_root = workspace_root.into();
    let composition = compose_packages(paths, workspace_root)?;
    let trust = SkillTrustStore::load(trust_store_path)?;
    let skill_type = PackageTypeId::new("skill")?;
    let mut ids = BTreeSet::new();
    for source in &composition.sources {
        for candidate in source.inventory_candidates(&skill_type)? {
            ids.insert(candidate.package_id.to_string());
        }
    }
    let mut registry = SkillRegistry::new();
    let mut external_package_count = 0;
    for id in ids {
        let reference = PackageRef::parse(&format!("skill:{id}@*"))?;
        let graph = composition.resolve(&reference)?;
        let node = graph
            .nodes
            .get(&graph.root)
            .context("resolved Skill graph has no root node")?;
        let payload = composition
            .registry
            .lookup(&node.candidate.type_id)?
            .validate_payload(node.candidate.view(), &node.envelope)?;
        let mut harness = *payload
            .downcast::<SkillHarness>()
            .map_err(|_| anyhow::anyhow!("Skill package adapter returned an unexpected payload"))?;
        harness.source = match node.candidate.tier {
            PackageSourceTier::Builtin => SkillSource::Core,
            PackageSourceTier::User | PackageSourceTier::System => SkillSource::Community,
            PackageSourceTier::Explicit | PackageSourceTier::Workspace => SkillSource::Local,
        };
        if node.candidate.tier != PackageSourceTier::Builtin {
            external_package_count += 1;
        }
        let trust_state = trust.state(&harness);
        registry.register(RegisteredSkill {
            harness,
            trust: trust_state,
        })?;
    }
    Ok(WorkspaceSkillRegistry {
        registry,
        package_lock_present: composition.lock.is_some(),
        external_package_count,
    })
}

#[derive(Clone, Debug)]
pub struct ResolvedRuntimeModel {
    pub node_key: String,
    pub package: ModelPackage,
}

#[derive(Clone, Debug)]
pub struct ResolvedRuntimeExtension {
    pub node_key: String,
    pub package: ExtensionPackage,
}

#[derive(Clone, Debug)]
pub struct ResolvedRuntimeSkill {
    pub node_key: String,
    pub harness: SkillHarness,
}

/// One admitted, immutable projection of every semantic artifact consumed by
/// a Function session. Package views inside `graph` are byte snapshots rather
/// than paths that can be rediscovered later.
#[derive(Clone, Debug)]
pub struct ResolvedRuntimeBundle {
    pub graph: ResolvedPackageGraph,
    pub function: agl_function::RuntimeFunction,
    pub model: Option<ResolvedRuntimeModel>,
    pub extensions: BTreeMap<String, ResolvedRuntimeExtension>,
    pub skills: BTreeMap<String, ResolvedRuntimeSkill>,
    pub lock: RuntimeBundleLockIdentity,
    pub runtime: crate::CurrentRuntimeIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeBundleLockState {
    Unlocked,
    Verified,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleLockIdentity {
    pub state: RuntimeBundleLockState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleEmbeddedProvenance {
    pub generation_id: String,
    pub builtin_catalog_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleNodeIdentity {
    pub reference: String,
    pub type_id: String,
    pub package_id: String,
    pub version: String,
    pub package_tree_digest: PackageTreeDigest,
    pub source_tier: PackageSourceTier,
    pub source_kind: PackageSourceKind,
    pub source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_root: Option<PathBuf>,
    pub envelope_schema: String,
    pub payload_schema: String,
    pub dependencies: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded_runtime: Option<RuntimeBundleEmbeddedProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleIdentity {
    pub schema: String,
    pub root: String,
    pub nodes: BTreeMap<String, RuntimeBundleNodeIdentity>,
    pub lock: RuntimeBundleLockIdentity,
    pub function: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub extensions: BTreeMap<String, String>,
    pub skills: BTreeMap<String, String>,
    pub runtime: crate::CurrentRuntimeIdentity,
}

impl PackageComposition {
    pub fn resolve(&self, root: &PackageRef) -> Result<ResolvedPackageGraph> {
        self.resolve_with_lock(root, self.lock.as_ref())
    }

    pub fn resolve_for_lock_refresh(&self, root: &PackageRef) -> Result<ResolvedPackageGraph> {
        self.resolve_with_lock(root, None)
    }

    pub fn workspace_lock(&self, root: &PackageRef) -> Result<PackageLock> {
        let mut entries = self
            .resolve_for_lock_refresh(root)?
            .package_lock_entries()?;
        let skill_type = PackageTypeId::skill();
        let mut skill_ids = BTreeSet::new();
        for source in &self.sources {
            for candidate in source.inventory_candidates(&skill_type)? {
                skill_ids.insert(candidate.package_id);
            }
        }
        for skill_id in skill_ids {
            let reference = PackageRef::parse(&format!("skill:{skill_id}@*"))?;
            for (key, package) in self
                .resolve_for_lock_refresh(&reference)?
                .package_lock_entries()?
            {
                if let Some(existing) = entries.insert(key.clone(), package.clone()) {
                    ensure!(
                        existing == package,
                        "package lock contains conflicting entries for `{key}`"
                    );
                }
            }
        }
        Ok(PackageLock::new(entries.into_values())?)
    }

    pub fn resolve_function_reference(
        &self,
        workspace_root: &Path,
        reference: &str,
    ) -> Result<ResolvedPackageGraph> {
        if !looks_like_function_path(reference) {
            let reference = if reference.contains(':') {
                reference.to_owned()
            } else {
                format!("function:{reference}@*")
            };
            return self.resolve(&PackageRef::parse(&reference)?);
        }

        let requested_path = PathBuf::from(reference);
        let requested_path = if requested_path.is_absolute() {
            requested_path
        } else {
            workspace_root.join(requested_path)
        };
        let package_root = if requested_path.is_dir() {
            requested_path
        } else {
            requested_path
                .parent()
                .context("explicit Function path has no package directory")?
                .to_path_buf()
        };
        let view = Arc::new(DirectoryPackageView::new(&package_root)?);
        let type_id = PackageTypeId::function();
        let adapter = self.registry.lookup(&type_id)?;
        let envelope = adapter.extract_envelope(view.as_ref())?;
        envelope.validate()?;
        if envelope.type_id != type_id {
            return Err(agl_package::PackageError::AdapterTypeMismatch {
                type_id: type_id.to_string(),
                actual_type: envelope.type_id.to_string(),
            }
            .into());
        }
        let source_id: PackageSourceId = "explicit-function".parse()?;
        let root = PackageRef::new(
            type_id.clone(),
            envelope.id.clone(),
            envelope.version.to_string().parse()?,
        );
        let candidate = PackageCandidate::new(
            type_id,
            envelope.id.clone(),
            envelope.version.clone(),
            source_id.clone(),
            PackageSourceTier::Explicit,
            PackageSourceKind::Directory,
            view,
        )
        .with_package_root(package_root);
        let explicit_source = Arc::new(StaticPackageSource::new(
            source_id.clone(),
            PackageSourceTier::Explicit,
            PackageSourceKind::Directory,
            vec![candidate],
        )?) as Arc<dyn PackageSource>;
        let mut sources = Vec::with_capacity(self.sources.len() + 1);
        sources.push(explicit_source);
        sources.extend(self.sources.iter().cloned());
        PackageResolver::new(self.registry.clone(), sources)
            .resolve_and_validate_with_explicit_root(&root, &source_id, self.lock.as_ref())
            .map_err(Into::into)
    }

    pub fn resolve_runtime_bundle(
        &self,
        workspace_root: &Path,
        config_dir: &Path,
        reference: &str,
        require_profile: bool,
        additional_skills: &[String],
    ) -> Result<ResolvedRuntimeBundle> {
        let graph = self.resolve_function_reference(workspace_root, reference)?;
        let function = agl_function::runtime_function_from_resolved_graph(
            &graph,
            &self.registry,
            workspace_root,
            config_dir,
            require_profile,
        )?;
        ResolvedRuntimeBundle::from_function_graph(
            self,
            graph,
            function,
            workspace_root,
            additional_skills,
        )
    }

    fn resolve_with_lock(
        &self,
        root: &PackageRef,
        lock: Option<&PackageLock>,
    ) -> Result<ResolvedPackageGraph> {
        PackageResolver::new(self.registry.clone(), self.sources.clone())
            .resolve_and_validate(root, lock)
            .map_err(Into::into)
    }
}

impl ResolvedRuntimeBundle {
    pub fn model_execution_inputs(
        &self,
        visible_tools_digest: impl Into<String>,
    ) -> Result<
        Option<(
            agl_model::ResolvedFunctionPlanInput,
            agl_model::ResolvedModelPlanInput,
        )>,
    > {
        let Some(model) = &self.model else {
            return Ok(None);
        };
        let function_node = self
            .graph
            .nodes
            .get(&self.graph.root)
            .context("resolved Function graph has no root node")?;
        let model_node = self
            .graph
            .nodes
            .get(&model.node_key)
            .context("resolved Model graph node is missing")?;
        let profile_id = self
            .function
            .model_profile
            .clone()
            .context("Function with a Model dependency has no model.profile")?;
        let generation_policy = self
            .function
            .generation_policy
            .clone()
            .context("Function with a Model dependency has no generation policy")?;
        let prompt_template_digest = sha256_text(&self.function.context);
        Ok(Some((
            agl_model::ResolvedFunctionPlanInput {
                package: plan_identity(function_node)?,
                selected_profile_id: profile_id,
                generation_policy,
                prompt_template_digest,
                visible_tools_digest: visible_tools_digest.into(),
            },
            agl_model::ResolvedModelPlanInput {
                package: plan_identity(model_node)?,
                payload_schema: model_node.envelope.payload_schema.to_string(),
                model: model.package.clone(),
            },
        )))
    }

    fn from_function_graph(
        composition: &PackageComposition,
        graph: ResolvedPackageGraph,
        function: agl_function::RuntimeFunction,
        workspace_root: &Path,
        additional_skills: &[String],
    ) -> Result<Self> {
        let runtime = crate::current_runtime_identity()
            .context("failed to establish runtime identity for artifact admission")?;
        let lock = lock_identity(composition.lock.as_ref())?;
        let model = resolved_runtime_model(&graph, &composition.registry, &function)?;
        let extensions = resolved_runtime_extensions(&graph, &composition.registry, &function)?;
        let mut bundle = Self {
            graph,
            function,
            model,
            extensions,
            skills: BTreeMap::new(),
            lock,
            runtime,
        };
        let mut selected = bundle.function.skills.clone();
        for spec in bundle.function.subagent_specs.values() {
            selected.extend(spec.skills.iter().cloned());
        }
        selected.extend(additional_skills.iter().cloned());
        bundle.add_selected_skills(composition, workspace_root, &selected)?;
        Ok(bundle)
    }

    pub fn with_selected_skills(
        mut self,
        composition: &PackageComposition,
        workspace_root: &Path,
        selected_skills: &[String],
    ) -> Result<Self> {
        self.add_selected_skills(composition, workspace_root, selected_skills)?;
        Ok(self)
    }

    pub fn identity(&self) -> RuntimeBundleIdentity {
        let embedded = RuntimeBundleEmbeddedProvenance {
            generation_id: self.runtime.generation_id.clone(),
            builtin_catalog_digest: self.runtime.builtin_catalog_digest.clone(),
        };
        let nodes = self
            .graph
            .nodes
            .iter()
            .map(|(key, node)| {
                let embedded_runtime = (node.candidate.tier == PackageSourceTier::Builtin
                    && node.candidate.kind == PackageSourceKind::Embedded)
                    .then(|| embedded.clone());
                (
                    key.clone(),
                    RuntimeBundleNodeIdentity {
                        reference: node.key(),
                        type_id: node.candidate.type_id.to_string(),
                        package_id: node.candidate.package_id.to_string(),
                        version: node.candidate.version.to_string(),
                        package_tree_digest: node.package_tree_digest.clone(),
                        source_tier: node.candidate.tier,
                        source_kind: node.candidate.kind,
                        source_id: node.candidate.source_id.to_string(),
                        source_revision: node.candidate.source_revision.clone(),
                        source_tree: node.candidate.source_tree.clone(),
                        package_root: node.candidate.package_root.clone(),
                        envelope_schema: node.envelope.schema.to_string(),
                        payload_schema: node.envelope.payload_schema.to_string(),
                        dependencies: node.dependencies.clone(),
                        embedded_runtime,
                    },
                )
            })
            .collect();
        RuntimeBundleIdentity {
            schema: "agentlibre.runtime-bundle/v1".to_owned(),
            root: self.graph.root.clone(),
            nodes,
            lock: self.lock.clone(),
            function: self.graph.root.clone(),
            model: self.model.as_ref().map(|model| model.node_key.clone()),
            extensions: self
                .extensions
                .iter()
                .map(|(id, extension)| (id.clone(), extension.node_key.clone()))
                .collect(),
            skills: self
                .skills
                .iter()
                .map(|(id, skill)| (id.clone(), skill.node_key.clone()))
                .collect(),
            runtime: self.runtime.clone(),
        }
    }

    fn add_selected_skills(
        &mut self,
        composition: &PackageComposition,
        _workspace_root: &Path,
        selected_skills: &[String],
    ) -> Result<()> {
        let selected = selected_skills
            .iter()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .collect::<BTreeSet<_>>();
        for skill_id in selected {
            if self.skills.contains_key(&skill_id) {
                continue;
            }
            let reference = PackageRef::parse(&format!("skill:{skill_id}@*"))?;
            let skill_graph = composition.resolve(&reference)?;
            let root = skill_graph
                .nodes
                .get(&skill_graph.root)
                .context("resolved Skill graph has no root candidate")?;
            let payload = composition
                .registry
                .lookup(&root.candidate.type_id)?
                .validate_payload(root.candidate.view(), &root.envelope)?;
            let mut harness = *payload
                .downcast::<SkillHarness>()
                .map_err(|_| anyhow::anyhow!("Skill adapter returned an unexpected payload"))?;
            harness.source = skill_source(root.candidate.tier);
            ensure!(
                harness.id.as_str() == skill_id,
                "resolved Skill `{}` produced payload `{}`",
                skill_id,
                harness.id
            );
            let node_key = skill_graph.root.clone();
            merge_runtime_graph(&mut self.graph, skill_graph)?;
            self.skills
                .insert(skill_id, ResolvedRuntimeSkill { node_key, harness });
        }
        Ok(())
    }
}

fn plan_identity(node: &agl_package::ResolvedPackage) -> Result<agl_model::PackagePlanIdentity> {
    Ok(agl_model::PackagePlanIdentity {
        reference: PackageRef::parse(&format!(
            "{}:{}@={}",
            node.candidate.type_id, node.candidate.package_id, node.candidate.version
        ))?,
        source_id: node.candidate.source_id.clone(),
        package_tree_digest: node.package_tree_digest.clone(),
    })
}

fn sha256_text(value: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(value.as_bytes());
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn resolved_runtime_model(
    graph: &ResolvedPackageGraph,
    registry: &PackageAdapterRegistry,
    function: &agl_function::RuntimeFunction,
) -> Result<Option<ResolvedRuntimeModel>> {
    let models = graph
        .nodes
        .iter()
        .filter(|(_, node)| node.candidate.type_id == PackageTypeId::model())
        .collect::<Vec<_>>();
    if function.model_profile.is_none() {
        ensure!(
            models.is_empty(),
            "Function without model.profile selected a Model artifact"
        );
        return Ok(None);
    }
    ensure!(
        models.len() == 1,
        "Function runtime bundle requires exactly one resolved Model; found {}",
        models.len()
    );
    let (node_key, node) = models[0];
    let payload = registry
        .lookup(&node.candidate.type_id)?
        .validate_payload(node.candidate.view(), &node.envelope)?;
    let mut package = *payload
        .downcast::<ModelPackage>()
        .map_err(|_| anyhow::anyhow!("Model adapter returned an unexpected payload"))?;
    package.provenance = Some(ModelPackageProvenance {
        reference: PackageRef::parse(&format!(
            "{}:{}@={}",
            node.candidate.type_id, node.candidate.package_id, node.candidate.version
        ))?,
        source_id: node.candidate.source_id.clone(),
        source_tier: node.candidate.tier,
        source_kind: node.candidate.kind,
        package_tree_digest: node.package_tree_digest.clone(),
    });
    Ok(Some(ResolvedRuntimeModel {
        node_key: node_key.clone(),
        package,
    }))
}

fn resolved_runtime_extensions(
    graph: &ResolvedPackageGraph,
    registry: &PackageAdapterRegistry,
    function: &agl_function::RuntimeFunction,
) -> Result<BTreeMap<String, ResolvedRuntimeExtension>> {
    let mut extensions = BTreeMap::new();
    for extension_id in &function.extensions {
        let matches = graph
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.candidate.type_id == PackageTypeId::extension()
                    && node.candidate.package_id.as_str() == extension_id
            })
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "Function requires Extension `{extension_id}` but the exact graph contains {} candidates",
            matches.len()
        );
        let (node_key, node) = matches[0];
        let payload = registry
            .lookup(&node.candidate.type_id)?
            .validate_payload(node.candidate.view(), &node.envelope)?;
        let package = *payload
            .downcast::<ExtensionPackage>()
            .map_err(|_| anyhow::anyhow!("Extension adapter returned an unexpected payload"))?;
        extensions.insert(
            extension_id.clone(),
            ResolvedRuntimeExtension {
                node_key: node_key.clone(),
                package,
            },
        );
    }
    Ok(extensions)
}

fn merge_runtime_graph(
    target: &mut ResolvedPackageGraph,
    incoming: ResolvedPackageGraph,
) -> Result<()> {
    for node in incoming.nodes.values() {
        if let Some(existing) = target.nodes.values().find(|existing| {
            existing.candidate.type_id == node.candidate.type_id
                && existing.candidate.package_id == node.candidate.package_id
        }) {
            ensure!(
                existing.key() == node.key()
                    && existing.package_tree_digest == node.package_tree_digest
                    && existing.candidate.source_id == node.candidate.source_id,
                "runtime artifact snapshot conflict for {}:{}: admitted `{}` from `{}` but later selected `{}` from `{}`",
                node.candidate.type_id,
                node.candidate.package_id,
                existing.key(),
                existing.candidate.source_id,
                node.key(),
                node.candidate.source_id
            );
        }
    }
    for (key, node) in incoming.nodes {
        if let Some(existing) = target.nodes.get(&key) {
            ensure!(
                same_resolved_node(existing, &node),
                "runtime artifact snapshot changed for `{key}`"
            );
        } else {
            target.nodes.insert(key, node);
        }
    }
    Ok(())
}

fn same_resolved_node(left: &ResolvedPackage, right: &ResolvedPackage) -> bool {
    left.envelope == right.envelope
        && left.package_tree_digest == right.package_tree_digest
        && left.dependencies == right.dependencies
        && left.candidate.source_id == right.candidate.source_id
        && left.candidate.tier == right.candidate.tier
        && left.candidate.kind == right.candidate.kind
        && left.candidate.source_revision == right.candidate.source_revision
        && left.candidate.source_tree == right.candidate.source_tree
}

fn skill_source(tier: PackageSourceTier) -> SkillSource {
    match tier {
        PackageSourceTier::Builtin => SkillSource::Core,
        PackageSourceTier::User | PackageSourceTier::System => SkillSource::Community,
        PackageSourceTier::Explicit | PackageSourceTier::Workspace => SkillSource::Local,
    }
}

fn lock_identity(lock: Option<&PackageLock>) -> Result<RuntimeBundleLockIdentity> {
    let Some(lock) = lock else {
        return Ok(RuntimeBundleLockIdentity {
            state: RuntimeBundleLockState::Unlocked,
            sha256: None,
        });
    };
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.runtime-bundle.lock.v1\0");
    digest.update(lock.to_toml()?.as_bytes());
    Ok(RuntimeBundleLockIdentity {
        state: RuntimeBundleLockState::Verified,
        sha256: Some(format!("sha256:{}", lowercase_hex(&digest.finalize()))),
    })
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

pub fn resolve_composed_packages(
    paths: &AgentLibrePaths,
    workspace_root: impl Into<PathBuf>,
    root: &PackageRef,
) -> Result<ResolvedPackageGraph> {
    compose_packages(paths, workspace_root)?.resolve(root)
}

pub fn resolve_composed_runtime_function(
    paths: &AgentLibrePaths,
    workspace_root: impl AsRef<Path>,
    reference: &str,
    require_profile: bool,
) -> Result<agl_function::RuntimeFunction> {
    let workspace_root = workspace_root.as_ref();
    let composition = compose_packages(paths, workspace_root)?;
    let graph = composition.resolve_function_reference(workspace_root, reference)?;
    agl_function::runtime_function_from_resolved_graph(
        &graph,
        &composition.registry,
        workspace_root,
        &paths.config_dir,
        require_profile,
    )
}

fn looks_like_function_path(reference: &str) -> bool {
    let path = Path::new(reference);
    path.is_absolute()
        || reference.contains('/')
        || reference.contains('\\')
        || reference.starts_with('.')
        || path.extension().is_some()
}

pub fn compose_packages(
    paths: &AgentLibrePaths,
    workspace_root: impl Into<PathBuf>,
) -> Result<PackageComposition> {
    let registry = Arc::new(PackageAdapterRegistry::from_dyn([
        Arc::new(FunctionPackageAdapter::default()) as Arc<dyn PackageAdapter>,
        Arc::new(SkillPackageAdapter::default()) as Arc<dyn PackageAdapter>,
        Arc::new(ModelPackageAdapter::default()) as Arc<dyn PackageAdapter>,
        Arc::new(ExtensionPackageAdapter::default()) as Arc<dyn PackageAdapter>,
    ])?);
    let workspace_root = workspace_root.into();
    let lock_path = workspace_root.join(".agl/package-lock.toml");
    let lock = match fs::read_to_string(&lock_path) {
        Ok(value) => Some(PackageLock::from_toml(&value)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let router = PackagePathRouter::new(
        workspace_root.clone(),
        paths.data_dir.clone(),
        paths.config_dir.clone(),
        paths.state_dir.clone(),
        paths.cache_dir.clone(),
        registry.clone(),
    );
    let mut sources: Vec<Arc<dyn PackageSource>> = vec![Arc::new(DirectoryPackageSource::new(
        "user".parse()?,
        PackageSourceTier::User,
        PackageSourceKind::Directory,
        paths.data_dir.clone(),
        registry.clone(),
    ))];
    for (index, root) in system_data_roots().into_iter().enumerate() {
        sources.push(Arc::new(DirectoryPackageSource::new(
            PackageSourceId::new(format!("system-{index}"))?,
            PackageSourceTier::System,
            PackageSourceKind::Directory,
            root,
            registry.clone(),
        )));
    }
    add_declared_sources(&mut sources, &workspace_root, &registry)?;
    sources.push(builtin_source()?);
    let sources = freeze_package_sources(&registry, sources)?;
    Ok(PackageComposition {
        registry,
        sources,
        router,
        lock,
    })
}

fn freeze_package_sources(
    registry: &PackageAdapterRegistry,
    sources: Vec<Arc<dyn PackageSource>>,
) -> Result<Vec<Arc<dyn PackageSource>>> {
    let type_ids = registry
        .iter()
        .map(|adapter| adapter.descriptor().type_id.clone())
        .collect::<Vec<_>>();
    sources
        .into_iter()
        .map(|source| {
            let mut candidates = Vec::new();
            for type_id in &type_ids {
                for candidate in source.inventory_candidates(type_id)? {
                    candidates.push(candidate.snapshot()?);
                }
            }
            Ok(Arc::new(StaticPackageSource::new(
                source.id().clone(),
                source.tier(),
                source.kind(),
                candidates,
            )?) as Arc<dyn PackageSource>)
        })
        .collect()
}

fn add_declared_sources(
    sources: &mut Vec<Arc<dyn PackageSource>>,
    workspace_root: &Path,
    registry: &Arc<PackageAdapterRegistry>,
) -> Result<()> {
    let manifest_path = workspace_root.join(".agl/workspace.toml");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let manifest = WorkspaceManifest::from_toml(&fs::read_to_string(&manifest_path)?)?;
    for declaration in manifest.sources {
        let kind = declaration.kind;
        let source_id = declaration.id.clone();
        let root = match kind {
            PackageSourceKind::Directory => {
                let relative = declaration
                    .path
                    .as_ref()
                    .context("declared package source path is missing")?;
                let root = workspace_root.join(relative).canonicalize()?;
                let workspace = workspace_root.canonicalize()?;
                ensure!(
                    root.starts_with(&workspace),
                    "package source escapes workspace"
                );
                root
            }
            PackageSourceKind::Git => {
                let relative = declaration
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".agl/sources").join(declaration.id.as_str()));
                let root = workspace_root.join(relative).canonicalize()?;
                let workspace = workspace_root.canonicalize()?;
                ensure!(
                    root.starts_with(&workspace),
                    "package source escapes workspace"
                );
                root
            }
            PackageSourceKind::Embedded => continue,
        };
        let mut source =
            DirectoryPackageSource::new(source_id, declaration.tier, kind, &root, registry.clone());
        if kind == PackageSourceKind::Git {
            let revision = declaration
                .rev
                .as_deref()
                .context("declared Git package source revision is missing")?;
            let provenance = agl_repo::verified_git_source_provenance(&root, revision)?;
            source = source.with_source_provenance(provenance.revision, provenance.tree);
        }
        sources.push(Arc::new(source));
    }
    Ok(())
}

fn system_data_roots() -> Vec<PathBuf> {
    std::env::var_os("XDG_DATA_DIRS")
        .map(|value| {
            std::env::split_paths(&value)
                .map(|path| path.join("agentLIBRE"))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                PathBuf::from("/usr/local/share/agentLIBRE"),
                PathBuf::from("/usr/share/agentLIBRE"),
            ]
        })
}

fn builtin_source() -> Result<Arc<dyn PackageSource>> {
    let source_id: PackageSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_PACKAGES {
        let files = package
            .files
            .iter()
            .map(|file| {
                Ok::<_, agl_package::PackageError>((file.path.parse()?, file.bytes.to_vec()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        candidates.push(
            PackageCandidate::new(
                package.type_id.parse()?,
                package.id.parse()?,
                package.version.parse()?,
                source_id.clone(),
                PackageSourceTier::Builtin,
                PackageSourceKind::Embedded,
                Arc::new(InMemoryPackageView::new(files)?),
            )
            .with_package_root(format!("builtin:{}/{}", package.type_id, package.id)),
        );
    }
    Ok(Arc::new(StaticPackageSource::new(
        source_id,
        PackageSourceTier::Builtin,
        PackageSourceKind::Embedded,
        candidates,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_package::PackageTypeId;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write_test_function(root: &Path, id: &str) {
        let package = root.join("functions").join(id);
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("FUNCTION.md"),
            format!(
                r#"---
package:
  schema: agentlibre.package/v1
  type: function
  id: {id}
  version: 1.0.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Test Function
---
"#
            ),
        )
        .unwrap();
        fs::write(package.join("SYSTEM.md"), "Test.\n").unwrap();
    }

    fn declare_workspace_source(workspace: &Path) {
        fs::create_dir_all(workspace.join(".agl")).unwrap();
        fs::write(
            workspace.join(".agl/workspace.toml"),
            r#"version = 3
default_function = "function:gemma4-31b-32k@^1"

[[sources]]
id = "workspace"
tier = "workspace"
kind = "directory"
path = ".agl"

[policy]
[config]
"#,
        )
        .unwrap();
    }

    #[test]
    fn composition_registers_all_core_adapters_and_source_tiers() {
        let paths = AgentLibrePaths::from_agl_home(std::env::temp_dir().join("agl-app-test-home"));
        let workspace =
            std::env::temp_dir().join(format!("agl-app-composition-{}", std::process::id()));
        fs::create_dir_all(&workspace).unwrap();
        declare_workspace_source(&workspace);
        let composition = compose_packages(&paths, &workspace).unwrap();
        assert_eq!(composition.registry.iter().count(), 4);
        assert!(
            composition
                .registry
                .lookup(&PackageTypeId::extension())
                .is_ok()
        );
        assert!(
            composition
                .sources
                .iter()
                .any(|source| source.tier() == PackageSourceTier::Builtin)
        );
        assert!(
            composition
                .sources
                .iter()
                .any(|source| source.tier() == PackageSourceTier::Workspace)
        );
        assert!(
            composition
                .sources
                .iter()
                .any(|source| source.tier() == PackageSourceTier::User)
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn composed_resolution_rejects_lock_drift_before_activation() {
        let paths = AgentLibrePaths::from_agl_home(
            std::env::temp_dir().join("agl-app-locked-resolution-home"),
        );
        let workspace =
            std::env::temp_dir().join(format!("agl-app-locked-resolution-{}", std::process::id()));
        fs::create_dir_all(&workspace).unwrap();
        let root: PackageRef = "function:gemma4-e4b@^1.0".parse().unwrap();
        let composition = compose_packages(&paths, &workspace).unwrap();
        let graph = composition.resolve_for_lock_refresh(&root).unwrap();
        let mut lock = graph.lock().unwrap();
        let package = lock.packages.first_mut().unwrap();
        package.package_tree_digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();

        fs::create_dir_all(workspace.join(".agl")).unwrap();
        lock.write_atomic(workspace.join(".agl/package-lock.toml"))
            .unwrap();
        let error = resolve_composed_packages(&paths, &workspace, &root).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<agl_package::PackageError>()
                .unwrap()
                .code(),
            "digest_drift"
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn composition_reads_user_packages_from_data_not_config() {
        let home = std::env::temp_dir().join(format!("agl-runtime-xdg-{}", std::process::id()));
        let workspace = home.join("workspace");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&workspace).unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);
        write_test_function(&paths.data_dir, "from-data");
        write_test_function(&paths.config_dir, "from-config");

        let composition = compose_packages(&paths, &workspace).unwrap();
        let user = composition
            .sources
            .iter()
            .find(|source| source.tier() == PackageSourceTier::User)
            .unwrap();
        let ids = user
            .candidates(&PackageTypeId::function())
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.package_id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["from-data"]);

        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn git_workspace_source_is_locked_by_clean_declared_revision_and_tree() {
        let home = std::env::temp_dir().join(format!(
            "agl-runtime-git-workspace-source-{}",
            std::process::id()
        ));
        let workspace = home.join("workspace");
        let source = workspace.join(".agl/private-skills");
        let skill = source.join("skills/private-notes");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            r#"---
package:
  schema: agentlibre.package/v1
  type: skill
  id: private-notes
  version: 1.0.0
  payload_schema: agentlibre.skill/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
description: Fixture private skill.
pack: agl
required_hooks: []
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees:
  - fixture guarantee
---

Fixture.
"#,
        )
        .unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "fixture"]);
        fs::write(
            workspace.join(".agl/workspace.toml"),
            r#"version = 3
default_function = "function:gemma4-e4b@^1"

[[sources]]
id = "private-skills"
tier = "workspace"
kind = "git"
path = ".agl/private-skills"
url = "fixture"
rev = "main"

[policy]
[config]
"#,
        )
        .unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);
        let composition = compose_packages(&paths, &workspace).unwrap();
        let graph = composition
            .resolve_for_lock_refresh(&"skill:private-notes@*".parse().unwrap())
            .unwrap();
        let node = graph.nodes.get(&graph.root).unwrap();
        assert_eq!(
            node.candidate.source_revision.as_deref(),
            Some(git(&source, &["rev-parse", "HEAD"]).as_str())
        );
        assert_eq!(
            node.candidate.source_tree.as_deref(),
            Some(git(&source, &["rev-parse", "HEAD^{tree}"]).as_str())
        );
        let lock = composition
            .workspace_lock(&"function:gemma4-e4b@^1".parse().unwrap())
            .unwrap();
        assert!(
            lock.packages
                .iter()
                .any(|package| package.key() == "skill:private-notes@1.0.0")
        );

        fs::write(skill.join("untracked.txt"), "dirty").unwrap();
        let error = match compose_packages(&paths, &workspace) {
            Ok(_) => panic!("dirty Git source unexpectedly produced a composition"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("uncommitted or ignored files"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn frozen_composition_preserves_invalid_inventory_but_resolution_fails_closed() {
        let home = std::env::temp_dir().join(format!(
            "agl-runtime-invalid-frozen-inventory-{}",
            std::process::id()
        ));
        let workspace = home.join("workspace");
        let package = workspace.join(".agl/functions/broken");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&package).unwrap();
        declare_workspace_source(&workspace);
        fs::write(
            package.join("FUNCTION.md"),
            r#"---
package:
  schema: agentlibre.artifact/v999
  type: function
  id: broken
  version: 1.0.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Broken
---
"#,
        )
        .unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);

        let composition = compose_packages(&paths, &workspace).unwrap();
        let workspace_source = composition
            .sources
            .iter()
            .find(|source| source.id().as_str() == "workspace")
            .unwrap();
        let candidates = workspace_source
            .inventory_candidates(&PackageTypeId::function())
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].package_id.as_str(), "broken");
        assert_eq!(candidates[0].version.to_string(), "0.0.0-invalid");
        assert_eq!(
            candidates[0].discovery_error().unwrap().code(),
            "invalid_envelope"
        );

        let reference: PackageRef = "function:broken@*".parse().unwrap();
        let error = composition.resolve(&reference).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<agl_package::PackageError>()
                .unwrap()
                .code(),
            "invalid_envelope"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_function_path_resolves_through_the_common_graph() {
        let home = std::env::temp_dir().join(format!(
            "agl-runtime-explicit-function-{}",
            std::process::id()
        ));
        let workspace = home.join("workspace");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&workspace).unwrap();
        write_test_function(&workspace.join(".agl"), "explicit");
        let function_path = workspace.join(".agl/functions/explicit/FUNCTION.md");
        let paths = AgentLibrePaths::from_agl_home(&home);

        let function = resolve_composed_runtime_function(
            &paths,
            &workspace,
            function_path.to_str().unwrap(),
            false,
        )
        .unwrap();

        assert_eq!(
            function.source,
            agl_function::FunctionPackageSource::Explicit
        );
        assert_eq!(function.path, function_path);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_current_gemma_function_keeps_only_the_root_explicit() {
        let home = std::env::temp_dir().join(format!(
            "agl-runtime-explicit-gemma-function-{}",
            std::process::id()
        ));
        let workspace = home.join("workspace");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&workspace).unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);
        let function_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/functions/gemma4-31b-32k/FUNCTION.md")
            .canonicalize()
            .unwrap();
        let composition = compose_packages(&paths, &workspace).unwrap();

        let graph = composition
            .resolve_function_reference(&workspace, function_path.to_str().unwrap())
            .unwrap();

        let root = &graph.nodes[&graph.root];
        assert_eq!(root.candidate.tier, PackageSourceTier::Explicit);
        assert_eq!(root.candidate.package_id.as_str(), "gemma4-31b-32k");
        assert_eq!(graph.nodes.len(), 4);
        for dependency in graph.nodes.values().filter(|node| node.key() != graph.root) {
            assert_eq!(dependency.candidate.tier, PackageSourceTier::Builtin);
            assert_eq!(dependency.candidate.source_id.as_str(), "builtin");
        }
        let function = agl_function::runtime_function_from_resolved_graph(
            &graph,
            &composition.registry,
            &workspace,
            &paths.config_dir,
            true,
        )
        .unwrap();
        assert_eq!(function.id, "gemma4-31b-32k");
        assert_eq!(
            function.model_profile.as_deref(),
            Some("gpu-rx7900xtx-32768")
        );

        let bundle = composition
            .resolve_runtime_bundle(
                &workspace,
                &paths.config_dir,
                function_path.to_str().unwrap(),
                true,
                &[],
            )
            .unwrap();
        let model = bundle.model.as_ref().unwrap();
        let provenance = model.package.provenance.as_ref().unwrap();
        assert_eq!(provenance.reference.to_string(), "model:gemma4-31b@=1.3.0");
        assert_eq!(provenance.source_id.as_str(), "builtin");
        assert_eq!(bundle.extensions.len(), 2);
        let identity = bundle.identity();
        assert_eq!(identity.nodes.len(), 4);
        assert_eq!(identity.model.as_deref(), Some(model.node_key.as_str()));
        for node in identity
            .nodes
            .values()
            .filter(|node| node.source_tier == PackageSourceTier::Builtin)
        {
            assert!(node.source_revision.is_none());
            assert!(node.source_tree.is_none());
            let embedded = node.embedded_runtime.as_ref().unwrap();
            assert_eq!(embedded.generation_id, identity.runtime.generation_id);
            assert_eq!(
                embedded.builtin_catalog_digest,
                identity.runtime.builtin_catalog_digest
            );
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn locked_workspace_model_is_the_exact_planning_input() {
        let home = std::env::temp_dir().join(format!(
            "agl-runtime-workspace-model-bundle-{}",
            std::process::id()
        ));
        let workspace = home.join("workspace");
        let function_root = workspace.join(".agl/functions/workspace-model");
        let model_root = workspace.join(".agl/models/gemma4-31b");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&function_root).unwrap();
        fs::create_dir_all(model_root.join("evidence")).unwrap();
        declare_workspace_source(&workspace);
        fs::write(
            function_root.join("FUNCTION.md"),
            r#"---
package:
  schema: agentlibre.package/v1
  type: function
  id: workspace-model
  version: 1.0.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:gemma4-31b@^1.0
title: Workspace model
model:
  profile: workspace-vulkan-32768
runtime:
  tool_mode: read-only
  max_output_tokens: 32
  stop_rules: []
  structured_generation: lazy_tool
  repair_malformed_tool_calls: true
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply with workspace model."
---
"#,
        )
        .unwrap();
        fs::write(
            function_root.join("SYSTEM.md"),
            "Use the workspace model.\n",
        )
        .unwrap();
        let model_document = r#"package = { schema = "agentlibre.package/v1", type = "model", id = "gemma4-31b", version = "1.3.0", payload_schema = "agentlibre.model/v3", agl = { compatible = ">=1.0.0-alpha.12", tested = ["1.0.0-alpha.12"] }, requires = [] }

display_name = "Workspace Gemma fixture"
capabilities = ["text", "tools"]
license = "test-only"
license_url = "https://example.invalid/license"
repository = "workspace/gemma-fixture"
upstream_revision = "1111111111111111111111111111111111111111"

[[weights]]
role = "main"
model_id = "gemma4-31b"
files = [{ filename = "workspace-gemma.gguf", byte_size = 123456789, sha256 = "2222222222222222222222222222222222222222222222222222222222222222" }]
required = true

[[profiles]]
id = "workspace-vulkan-32768"
device = "gpu"
pci_device_id = "1002:744c"
pci_subsystem_id = "1da2:471e"
benchmark_evidence = "evidence/workspace.md"
required_total_ram_bytes = 1024
host_private_bytes = 512
device_private_bytes = 1024
shared_bytes = 0
decoder_scratch_bytes = 0
gpu_layers = 999
context_tokens = 32768
batch_size = 64
ubatch_size = 32
threads = 2
flash_attention = true
cache_type_k = "q8_0"
cache_type_v = "q8_0"
mmap = true
unified_kv = false
slot_count = 1
smoke_timeout_seconds = 30
expected_speed = "fixture"
"#;
        fs::write(model_root.join("MODEL.toml"), model_document).unwrap();
        fs::write(
            model_root.join("evidence/workspace.md"),
            "Workspace evidence.\n",
        )
        .unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);
        let root: PackageRef = "function:workspace-model@=1.0.0".parse().unwrap();
        let unlocked = compose_packages(&paths, &workspace).unwrap();
        let graph = unlocked.resolve_for_lock_refresh(&root).unwrap();
        fs::create_dir_all(workspace.join(".agl")).unwrap();
        graph
            .lock()
            .unwrap()
            .write_atomic(workspace.join(".agl/package-lock.toml"))
            .unwrap();

        let composition = compose_packages(&paths, &workspace).unwrap();
        fs::write(model_root.join("MODEL.toml"), "mutated after admission").unwrap();
        let bundle = composition
            .resolve_runtime_bundle(&workspace, &paths.config_dir, "workspace-model", true, &[])
            .unwrap();
        assert_eq!(bundle.lock.state, RuntimeBundleLockState::Verified);
        let model = &bundle.model.as_ref().unwrap().package;
        let provenance = model.provenance.as_ref().unwrap();
        assert_eq!(provenance.reference.to_string(), "model:gemma4-31b@=1.3.0");
        assert_eq!(provenance.source_tier, PackageSourceTier::Workspace);
        assert_eq!(model.profiles[0].id, "workspace-vulkan-32768");

        let (function_input, model_input) = bundle
            .model_execution_inputs("sha256:visible-tools")
            .unwrap()
            .unwrap();
        let host = agl_model::HostCapabilities {
            physical_host_bytes: 16_000_000_000,
            physical_cpu_cores: 4,
            logical_cpu_cores: 8,
            devices: vec![agl_model::HostCapabilityDevice {
                identity: "Vulkan0".to_owned(),
                kind: agl_model::HostCapabilityDeviceKind::DiscreteGpu,
                pci_device_id: Some("1002:744c".to_owned()),
                pci_subsystem_id: Some("1da2:471e".to_owned()),
                physical_pool_bytes: 24_000_000_000,
                usable: true,
                supports_gpu_offload: true,
            }],
        };
        let plan = agl_model::resolve_execution_plan(&function_input, &model_input, &host).unwrap();
        assert_eq!(plan.profile_id(), "workspace-vulkan-32768");
        assert_eq!(plan.model_package().reference, provenance.reference);
        assert_eq!(
            plan.artifact_roles()[0].files()[0].basename(),
            "workspace-gemma.gguf"
        );
        assert_eq!(
            plan.selected_device().unwrap().pci_device_id.as_deref(),
            Some("1002:744c")
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn composition_rejects_an_existing_invalid_lock() {
        let home =
            std::env::temp_dir().join(format!("agl-runtime-invalid-lock-{}", std::process::id()));
        let workspace = home.join("workspace");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(workspace.join(".agl")).unwrap();
        fs::write(workspace.join(".agl/package-lock.toml"), "invalid lock = [").unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);

        let error = match compose_packages(&paths, &workspace) {
            Ok(_) => panic!("invalid lock unexpectedly produced a composition"),
            Err(error) => error,
        };
        assert_eq!(
            error
                .downcast_ref::<agl_package::PackageError>()
                .unwrap()
                .code(),
            "lock_stale"
        );

        fs::remove_dir_all(home).unwrap();
    }
}
