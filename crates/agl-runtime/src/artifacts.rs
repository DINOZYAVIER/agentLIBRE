use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactAdapterRegistry, ArtifactCandidate, ArtifactLock, ArtifactPackageRef,
    ArtifactPathRouter, ArtifactResolver, ArtifactSource, ArtifactSourceId, ArtifactSourceKind,
    ArtifactSourceTier, ArtifactTypeId, DirectoryArtifactSource, DirectoryPackageView,
    ExtensionArtifactAdapter, InMemoryPackageView, ResolvedArtifactGraph, StaticArtifactSource,
    WorkspaceManifest,
};
use agl_function::FunctionArtifactAdapter;
use agl_model::ModelArtifactAdapter;
use agl_skill::SkillArtifactAdapter;
use anyhow::{Context, Result};

use crate::AgentLibrePaths;

#[derive(Clone)]
pub struct ArtifactComposition {
    pub registry: Arc<ArtifactAdapterRegistry>,
    pub sources: Vec<Arc<dyn ArtifactSource>>,
    pub router: ArtifactPathRouter,
    pub lock: Option<ArtifactLock>,
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
            return Err(agl_artifact::ArtifactError::AdapterTypeMismatch {
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
            source_id,
            ArtifactSourceTier::Explicit,
            ArtifactSourceKind::Directory,
            vec![candidate],
        )?) as Arc<dyn ArtifactSource>;
        let mut sources = Vec::with_capacity(self.sources.len() + 1);
        sources.push(explicit_source);
        sources.extend(self.sources.iter().cloned());
        ArtifactResolver::new(self.registry.clone(), sources)
            .resolve_and_validate(&root, self.lock.as_ref())
            .map_err(Into::into)
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
        Arc::new(ExtensionArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
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
    Ok(ArtifactComposition {
        registry,
        sources,
        router,
        lock,
    })
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
                Ok::<_, agl_artifact::ArtifactError>((file.path.parse()?, file.bytes.to_vec()))
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
    use agl_artifact::ArtifactTypeId;

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
                .downcast_ref::<agl_artifact::ArtifactError>()
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
                .downcast_ref::<agl_artifact::ArtifactError>()
                .unwrap()
                .code(),
            "lock_stale"
        );

        fs::remove_dir_all(home).unwrap();
    }
}
