use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agl_extension::package::{ExtensionPackage, ExtensionPackageAdapter};
use agl_function::FunctionArtifactAdapter;
use agl_model::{ModelArtifactAdapter, ModelPackage, ModelPackageProvenance};
use agl_package::{
    ArtifactAdapter, ArtifactAdapterRegistry, ArtifactCandidate, ArtifactLock, ArtifactPackageRef,
    ArtifactPathRouter, ArtifactResolver, ArtifactSource, ArtifactSourceId, ArtifactSourceKind,
    ArtifactSourceTier, ArtifactTypeId, DirectoryArtifactSource, DirectoryPackageView,
    InMemoryPackageView, PackageTreeDigest, ResolvedArtifact, ResolvedArtifactGraph,
    StaticArtifactSource, WorkspaceManifest,
};
use agl_skill::{SkillArtifactAdapter, SkillHarness, SkillSource};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::AgentLibrePaths;

#[derive(Clone)]
pub struct ArtifactComposition {
    pub registry: Arc<ArtifactAdapterRegistry>,
    pub sources: Vec<Arc<dyn ArtifactSource>>,
    pub router: ArtifactPathRouter,
    pub lock: Option<ArtifactLock>,
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
    pub graph: ResolvedArtifactGraph,
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
    pub package_digest: PackageTreeDigest,
    pub source_tier: ArtifactSourceTier,
    pub source_kind: ArtifactSourceKind,
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

impl ArtifactComposition {
    pub fn resolve(&self, root: &ArtifactPackageRef) -> Result<ResolvedArtifactGraph> {
        self.resolve_with_lock(root, self.lock.as_ref())
    }

    pub fn resolve_for_lock_refresh(
        &self,
        root: &ArtifactPackageRef,
    ) -> Result<ResolvedArtifactGraph> {
        self.resolve_with_lock(root, None)
    }

    pub fn resolve_function_reference(
        &self,
        workspace_root: &Path,
        reference: &str,
    ) -> Result<ResolvedArtifactGraph> {
        if !looks_like_function_path(reference) {
            let reference = if reference.contains(':') {
                reference.to_owned()
            } else {
                format!("function:{reference}@*")
            };
            return self.resolve(&ArtifactPackageRef::parse(&reference)?);
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
        let type_id = ArtifactTypeId::function();
        let adapter = self.registry.lookup(&type_id)?;
        let envelope = adapter.extract_envelope(view.as_ref())?;
        envelope.validate()?;
        if envelope.type_id != type_id {
            return Err(agl_package::ArtifactError::AdapterTypeMismatch {
                type_id: type_id.to_string(),
                actual_type: envelope.type_id.to_string(),
            }
            .into());
        }
        let source_id: ArtifactSourceId = "explicit-function".parse()?;
        let root = ArtifactPackageRef::new(
            type_id.clone(),
            envelope.id.clone(),
            envelope.version.to_string().parse()?,
        );
        let candidate = ArtifactCandidate::new(
            type_id,
            envelope.id.clone(),
            envelope.version.clone(),
            source_id.clone(),
            ArtifactSourceTier::Explicit,
            ArtifactSourceKind::Directory,
            view,
        )
        .with_package_root(package_root);
        let explicit_source = Arc::new(StaticArtifactSource::new(
            source_id.clone(),
            ArtifactSourceTier::Explicit,
            ArtifactSourceKind::Directory,
            vec![candidate],
        )?) as Arc<dyn ArtifactSource>;
        let mut sources = Vec::with_capacity(self.sources.len() + 1);
        sources.push(explicit_source);
        sources.extend(self.sources.iter().cloned());
        ArtifactResolver::new(self.registry.clone(), sources)
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
        root: &ArtifactPackageRef,
        lock: Option<&ArtifactLock>,
    ) -> Result<ResolvedArtifactGraph> {
        ArtifactResolver::new(self.registry.clone(), self.sources.clone())
            .resolve_and_validate(root, lock)
            .map_err(Into::into)
    }
}

impl ResolvedRuntimeBundle {
    fn from_function_graph(
        composition: &ArtifactComposition,
        graph: ResolvedArtifactGraph,
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
        composition: &ArtifactComposition,
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
                let embedded_runtime = (node.candidate.tier == ArtifactSourceTier::Builtin
                    && node.candidate.kind == ArtifactSourceKind::Embedded)
                    .then(|| embedded.clone());
                (
                    key.clone(),
                    RuntimeBundleNodeIdentity {
                        reference: node.key(),
                        type_id: node.candidate.type_id.to_string(),
                        package_id: node.candidate.package_id.to_string(),
                        version: node.candidate.version.to_string(),
                        package_digest: node.package_digest.clone(),
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
        composition: &ArtifactComposition,
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
            let reference = ArtifactPackageRef::parse(&format!("skill:{skill_id}@*"))?;
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

fn resolved_runtime_model(
    graph: &ResolvedArtifactGraph,
    registry: &ArtifactAdapterRegistry,
    function: &agl_function::RuntimeFunction,
) -> Result<Option<ResolvedRuntimeModel>> {
    let models = graph
        .nodes
        .iter()
        .filter(|(_, node)| node.candidate.type_id == ArtifactTypeId::model())
        .collect::<Vec<_>>();
    if function.inference_config_toml.is_none() {
        ensure!(
            models.is_empty(),
            "Function without an inference config selected a Model artifact"
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
        reference: ArtifactPackageRef::parse(&format!(
            "{}:{}@={}",
            node.candidate.type_id, node.candidate.package_id, node.candidate.version
        ))?,
        source_id: node.candidate.source_id.clone(),
        source_tier: node.candidate.tier,
        source_kind: node.candidate.kind,
        package_digest: node.package_digest.clone(),
    });
    Ok(Some(ResolvedRuntimeModel {
        node_key: node_key.clone(),
        package,
    }))
}

fn resolved_runtime_extensions(
    graph: &ResolvedArtifactGraph,
    registry: &ArtifactAdapterRegistry,
    function: &agl_function::RuntimeFunction,
) -> Result<BTreeMap<String, ResolvedRuntimeExtension>> {
    let mut extensions = BTreeMap::new();
    for extension_id in &function.extensions {
        let matches = graph
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.candidate.type_id == ArtifactTypeId::extension()
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
    target: &mut ResolvedArtifactGraph,
    incoming: ResolvedArtifactGraph,
) -> Result<()> {
    for node in incoming.nodes.values() {
        if let Some(existing) = target.nodes.values().find(|existing| {
            existing.candidate.type_id == node.candidate.type_id
                && existing.candidate.package_id == node.candidate.package_id
        }) {
            ensure!(
                existing.key() == node.key()
                    && existing.package_digest == node.package_digest
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

fn same_resolved_node(left: &ResolvedArtifact, right: &ResolvedArtifact) -> bool {
    left.envelope == right.envelope
        && left.package_digest == right.package_digest
        && left.dependencies == right.dependencies
        && left.candidate.source_id == right.candidate.source_id
        && left.candidate.tier == right.candidate.tier
        && left.candidate.kind == right.candidate.kind
        && left.candidate.source_revision == right.candidate.source_revision
        && left.candidate.source_tree == right.candidate.source_tree
}

fn skill_source(tier: ArtifactSourceTier) -> SkillSource {
    match tier {
        ArtifactSourceTier::Builtin => SkillSource::Core,
        ArtifactSourceTier::User | ArtifactSourceTier::System => SkillSource::Community,
        ArtifactSourceTier::Explicit | ArtifactSourceTier::Workspace => SkillSource::Local,
    }
}

fn lock_identity(lock: Option<&ArtifactLock>) -> Result<RuntimeBundleLockIdentity> {
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

pub fn resolve_composed_artifacts(
    paths: &AgentLibrePaths,
    workspace_root: impl Into<PathBuf>,
    root: &ArtifactPackageRef,
) -> Result<ResolvedArtifactGraph> {
    compose_artifacts(paths, workspace_root)?.resolve(root)
}

pub fn resolve_composed_runtime_function(
    paths: &AgentLibrePaths,
    workspace_root: impl AsRef<Path>,
    reference: &str,
    require_profile: bool,
) -> Result<agl_function::RuntimeFunction> {
    let workspace_root = workspace_root.as_ref();
    let composition = compose_artifacts(paths, workspace_root)?;
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

pub fn compose_artifacts(
    paths: &AgentLibrePaths,
    workspace_root: impl Into<PathBuf>,
) -> Result<ArtifactComposition> {
    let registry = Arc::new(ArtifactAdapterRegistry::from_dyn([
        Arc::new(FunctionArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
        Arc::new(SkillArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
        Arc::new(ModelArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
        Arc::new(ExtensionPackageAdapter::default()) as Arc<dyn ArtifactAdapter>,
    ])?);
    let workspace_root = workspace_root.into();
    let lock =
        agl_repo::read_optional_artifact_lock_v2(workspace_root.join(".agl/artifact-lock.toml"))?;
    let router = ArtifactPathRouter::new(
        workspace_root.clone(),
        paths.data_dir.clone(),
        paths.config_dir.clone(),
        paths.state_dir.clone(),
        paths.cache_dir.clone(),
        registry.clone(),
    );
    let mut sources: Vec<Arc<dyn ArtifactSource>> = vec![
        Arc::new(DirectoryArtifactSource::new(
            "workspace".parse()?,
            ArtifactSourceTier::Workspace,
            ArtifactSourceKind::Directory,
            workspace_root.join(".agl"),
            registry.clone(),
        )),
        Arc::new(DirectoryArtifactSource::new(
            "user".parse()?,
            ArtifactSourceTier::User,
            ArtifactSourceKind::Directory,
            paths.data_dir.clone(),
            registry.clone(),
        )),
    ];
    for (index, root) in system_data_roots().into_iter().enumerate() {
        sources.push(Arc::new(DirectoryArtifactSource::new(
            ArtifactSourceId::new(format!("system-{index}"))?,
            ArtifactSourceTier::System,
            ArtifactSourceKind::Directory,
            root,
            registry.clone(),
        )));
    }
    add_declared_sources(&mut sources, &workspace_root, &registry)?;
    sources.push(builtin_source()?);
    let sources = freeze_artifact_sources(&registry, sources)?;
    Ok(ArtifactComposition {
        registry,
        sources,
        router,
        lock,
    })
}

fn freeze_artifact_sources(
    registry: &ArtifactAdapterRegistry,
    sources: Vec<Arc<dyn ArtifactSource>>,
) -> Result<Vec<Arc<dyn ArtifactSource>>> {
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
            Ok(Arc::new(StaticArtifactSource::new(
                source.id().clone(),
                source.tier(),
                source.kind(),
                candidates,
            )?) as Arc<dyn ArtifactSource>)
        })
        .collect()
}

fn add_declared_sources(
    sources: &mut Vec<Arc<dyn ArtifactSource>>,
    workspace_root: &Path,
    registry: &Arc<ArtifactAdapterRegistry>,
) -> Result<()> {
    let manifest_path = workspace_root.join(".agl/workspace.toml");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let manifest = WorkspaceManifest::from_toml(&fs::read_to_string(&manifest_path)?)?;
    for (name, declaration) in manifest.sources {
        let kind = declaration.kind;
        let root = match kind {
            ArtifactSourceKind::Directory | ArtifactSourceKind::Git => {
                agl_repo::resolve_artifact_source_root(workspace_root, &name, &declaration)?
            }
            ArtifactSourceKind::Embedded => continue,
        };
        let provenance = if kind == ArtifactSourceKind::Git {
            Some(agl_repo::git_source_provenance(&root)?)
        } else {
            None
        };
        let mut source = DirectoryArtifactSource::new(
            ArtifactSourceId::new(name)?,
            ArtifactSourceTier::Workspace,
            kind,
            root,
            registry.clone(),
        );
        if let Some(provenance) = provenance {
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

fn builtin_source() -> Result<Arc<dyn ArtifactSource>> {
    let source_id: ArtifactSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_ARTIFACT_PACKAGES {
        let files = package
            .files
            .iter()
            .map(|file| {
                Ok::<_, agl_package::ArtifactError>((file.path.parse()?, file.bytes.to_vec()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        candidates.push(
            ArtifactCandidate::new(
                package.type_id.parse()?,
                package.id.parse()?,
                package.version.parse()?,
                source_id.clone(),
                ArtifactSourceTier::Builtin,
                ArtifactSourceKind::Embedded,
                Arc::new(InMemoryPackageView::new(files)?),
            )
            .with_package_root(format!("builtin:{}/{}", package.type_id, package.id)),
        );
    }
    Ok(Arc::new(StaticArtifactSource::new(
        source_id,
        ArtifactSourceTier::Builtin,
        ArtifactSourceKind::Embedded,
        candidates,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_package::ArtifactTypeId;

    fn write_test_function(root: &Path, id: &str) {
        let package = root.join("functions").join(id);
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("FUNCTION.md"),
            format!(
                r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: {id}
  version: 1.0.0
  payload_schema: agentlibre.function/v2
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

    #[test]
    fn composition_registers_all_core_adapters_and_source_tiers() {
        let paths = AgentLibrePaths::from_agl_home(std::env::temp_dir().join("agl-app-test-home"));
        let workspace =
            std::env::temp_dir().join(format!("agl-app-composition-{}", std::process::id()));
        fs::create_dir_all(&workspace).unwrap();
        let composition = compose_artifacts(&paths, &workspace).unwrap();
        assert_eq!(composition.registry.iter().count(), 4);
        assert!(
            composition
                .registry
                .lookup(&ArtifactTypeId::extension())
                .is_ok()
        );
        assert!(
            composition
                .sources
                .iter()
                .any(|source| source.tier() == ArtifactSourceTier::Builtin)
        );
        assert!(
            composition
                .sources
                .iter()
                .any(|source| source.tier() == ArtifactSourceTier::Workspace)
        );
        assert!(
            composition
                .sources
                .iter()
                .any(|source| source.tier() == ArtifactSourceTier::User)
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
        let root: ArtifactPackageRef = "function:gemma4-e4b@^1.0".parse().unwrap();
        let composition = compose_artifacts(&paths, &workspace).unwrap();
        let graph = composition.resolve_for_lock_refresh(&root).unwrap();
        let mut lock = graph.lock().unwrap();
        let package = lock.packages.values_mut().next().unwrap();
        package.package_digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();

        fs::create_dir_all(workspace.join(".agl")).unwrap();
        agl_repo::write_artifact_lock_v2(workspace.join(".agl/artifact-lock.toml"), &lock).unwrap();
        let error = resolve_composed_artifacts(&paths, &workspace, &root).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<agl_package::ArtifactError>()
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

        let composition = compose_artifacts(&paths, &workspace).unwrap();
        let user = composition
            .sources
            .iter()
            .find(|source| source.tier() == ArtifactSourceTier::User)
            .unwrap();
        let ids = user
            .candidates(&ArtifactTypeId::function())
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.package_id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["from-data"]);

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
        fs::write(
            package.join("FUNCTION.md"),
            r#"---
artifact:
  schema: agentlibre.artifact/v999
  type: function
  id: broken
  version: 1.0.0
  payload_schema: agentlibre.function/v2
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

        let composition = compose_artifacts(&paths, &workspace).unwrap();
        let workspace_source = composition
            .sources
            .iter()
            .find(|source| source.id().as_str() == "workspace")
            .unwrap();
        let candidates = workspace_source
            .inventory_candidates(&ArtifactTypeId::function())
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].package_id.as_str(), "broken");
        assert_eq!(candidates[0].version.to_string(), "0.0.0-invalid");
        assert_eq!(
            candidates[0].discovery_error().unwrap().code(),
            "invalid_envelope"
        );

        let reference: ArtifactPackageRef = "function:broken@*".parse().unwrap();
        let error = composition.resolve(&reference).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<agl_package::ArtifactError>()
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
        let composition = compose_artifacts(&paths, &workspace).unwrap();

        let graph = composition
            .resolve_function_reference(&workspace, function_path.to_str().unwrap())
            .unwrap();

        let root = &graph.nodes[&graph.root];
        assert_eq!(root.candidate.tier, ArtifactSourceTier::Explicit);
        assert_eq!(root.candidate.package_id.as_str(), "gemma4-31b-32k");
        assert_eq!(graph.nodes.len(), 4);
        for dependency in graph.nodes.values().filter(|node| node.key() != graph.root) {
            assert_eq!(dependency.candidate.tier, ArtifactSourceTier::Builtin);
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
        assert!(
            function
                .inference_config_toml
                .as_deref()
                .unwrap()
                .contains("max_context_tokens = 32768")
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
        assert_eq!(provenance.reference.to_string(), "model:gemma4-31b@=1.1.0");
        assert_eq!(provenance.source_id.as_str(), "builtin");
        assert_eq!(bundle.extensions.len(), 2);
        let identity = bundle.identity();
        assert_eq!(identity.nodes.len(), 4);
        assert_eq!(identity.model.as_deref(), Some(model.node_key.as_str()));
        for node in identity
            .nodes
            .values()
            .filter(|node| node.source_tier == ArtifactSourceTier::Builtin)
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
        fs::write(
            function_root.join("FUNCTION.md"),
            r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: workspace-model
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:gemma4-31b@^1.0
title: Workspace model
model:
  config: inference.toml
runtime:
  tool_mode: read-only
  max_output_tokens: 32
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
        fs::write(
            function_root.join("inference.toml"),
            r#"[backend]
kind = "llama_cpp"
model_id = "gemma4-31b"

[runtime]
mode = "auto"
max_context_tokens = 32768
max_batch_size = 64
max_ubatch_size = 32
device = "vulkan0"
flash_attention = "on"
cache_type_k = "q8_0"
cache_type_v = "q8_0"

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
        )
        .unwrap();
        let model_document = r#"artifact = { schema = "agentlibre.artifact/v1", type = "model", id = "gemma4-31b", version = "1.2.0", payload_schema = "agentlibre.model/v2", agl = { compatible = ">=1.0.0-alpha.12", tested = ["1.0.0-alpha.12"] }, requires = [] }

display_name = "Workspace Gemma fixture"
capabilities = ["text", "tools"]
license = "test-only"
license_url = "https://example.invalid/license"
repository = "workspace/gemma-fixture"
upstream_revision = "1111111111111111111111111111111111111111"

[[weights]]
role = "main"
model_id = "gemma4-31b"
filename = "workspace-gemma.gguf"
byte_size = 123456789
sha256 = "2222222222222222222222222222222222222222222222222222222222222222"
required = true

[[profiles]]
id = "workspace-vulkan-32768"
device = "gpu"
pci_device_id = "1002:744c"
pci_subsystem_id = "1da2:471e"
benchmark_evidence = "evidence/workspace.md"
required_total_ram_bytes = 1024
required_available_ram_bytes = 512
required_vram_bytes = 1024
gpu_layers = 999
context_tokens = 32768
batch_size = 64
ubatch_size = 32
threads = 2
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
        let root: ArtifactPackageRef = "function:workspace-model@=1.0.0".parse().unwrap();
        let unlocked = compose_artifacts(&paths, &workspace).unwrap();
        let graph = unlocked.resolve_for_lock_refresh(&root).unwrap();
        fs::create_dir_all(workspace.join(".agl")).unwrap();
        agl_repo::write_artifact_lock_v2(
            workspace.join(".agl/artifact-lock.toml"),
            &graph.lock().unwrap(),
        )
        .unwrap();

        let composition = compose_artifacts(&paths, &workspace).unwrap();
        fs::write(model_root.join("MODEL.toml"), "mutated after admission").unwrap();
        let bundle = composition
            .resolve_runtime_bundle(&workspace, &paths.config_dir, "workspace-model", true, &[])
            .unwrap();
        assert_eq!(bundle.lock.state, RuntimeBundleLockState::Verified);
        let model = &bundle.model.as_ref().unwrap().package;
        let provenance = model.provenance.as_ref().unwrap();
        assert_eq!(provenance.reference.to_string(), "model:gemma4-31b@=1.2.0");
        assert_eq!(provenance.source_tier, ArtifactSourceTier::Workspace);
        assert_eq!(model.profiles[0].id, "workspace-vulkan-32768");

        let preset = agl_config::load_inference_preset_from_str(
            "workspace fixture",
            bundle.function.inference_config_toml.as_deref().unwrap(),
        )
        .unwrap();
        let host = agl_model::HostResources {
            detected_total_memory_bytes: 16_000_000_000,
            nominal_memory_class_bytes: 16_000_000_000,
            available_memory_bytes: 15_000_000_000,
            cpu: agl_model::CpuResources {
                physical_cores: 4,
                logical_cores: 8,
            },
            disk: agl_model::DiskResources {
                path: home.clone(),
                mount_point: home.clone(),
                total_bytes: 10_000_000_000,
                available_bytes: 9_000_000_000,
            },
            devices: vec![agl_model::LlamaDeviceInfo {
                name: "Vulkan0".to_owned(),
                description: "fixture GPU".to_owned(),
                kind: agl_model::LlamaDeviceKind::DiscreteGpu,
                pci_device_id: Some("1002:744c".to_owned()),
                pci_subsystem_id: Some("1da2:471e".to_owned()),
                free_memory_bytes: 20_000_000_000,
                total_memory_bytes: 24_000_000_000,
                usable: true,
                supports_gpu_offload: true,
            }],
        };
        let plan = agl_model::RuntimePlanner
            .plan(model, &host, preset.runtime.auto_policy().unwrap(), false)
            .unwrap();
        assert_eq!(plan.profile_id, "workspace-vulkan-32768");
        assert_eq!(plan.model.provenance, provenance.clone());
        assert_eq!(plan.model.weights[0].filename, "workspace-gemma.gguf");
        assert_eq!(
            plan.selected_device_identity
                .as_ref()
                .unwrap()
                .pci_device_id
                .as_deref(),
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
        fs::write(
            workspace.join(".agl/artifact-lock.toml"),
            "invalid lock = [",
        )
        .unwrap();
        let paths = AgentLibrePaths::from_agl_home(&home);

        let error = match compose_artifacts(&paths, &workspace) {
            Ok(_) => panic!("invalid lock unexpectedly produced a composition"),
            Err(error) => error,
        };
        assert_eq!(
            error
                .downcast_ref::<agl_package::ArtifactError>()
                .unwrap()
                .code(),
            "lock_stale"
        );

        fs::remove_dir_all(home).unwrap();
    }
}
