use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactAdapterRegistry, ArtifactCandidate, ArtifactSource, ArtifactSourceId,
    ArtifactSourceKind, ArtifactSourceTier, DirectoryArtifactSource, InMemoryPackageView,
    StaticArtifactSource, WorkspaceManifest,
};
use agl_function::FunctionArtifactAdapter;
use agl_model::ModelArtifactAdapter;
use agl_runtime::AgentLibrePaths;
use agl_skill::SkillArtifactAdapter;
use anyhow::{Context, Result};

#[derive(Clone)]
pub struct ArtifactComposition {
    pub registry: Arc<ArtifactAdapterRegistry>,
    pub sources: Vec<Arc<dyn ArtifactSource>>,
}

pub fn compose_artifacts(
    paths: &AgentLibrePaths,
    workspace_root: impl Into<PathBuf>,
) -> Result<ArtifactComposition> {
    let registry = Arc::new(ArtifactAdapterRegistry::from_dyn([
        Arc::new(FunctionArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
        Arc::new(SkillArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
        Arc::new(ModelArtifactAdapter::default()) as Arc<dyn ArtifactAdapter>,
    ])?);
    let workspace_root = workspace_root.into();
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
            paths.config_dir.clone(),
            registry.clone(),
        )),
    ];
    for (index, root) in system_config_roots().into_iter().enumerate() {
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
    Ok(ArtifactComposition { registry, sources })
}

fn add_declared_sources(
    sources: &mut Vec<Arc<dyn ArtifactSource>>,
    workspace_root: &PathBuf,
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
            ArtifactSourceKind::Directory => workspace_root.join(
                declaration
                    .path
                    .context("local artifact source is missing its path")?,
            ),
            ArtifactSourceKind::Git => workspace_root.join(".agl/sources").join(&name),
            ArtifactSourceKind::Embedded => continue,
        };
        sources.push(Arc::new(DirectoryArtifactSource::new(
            ArtifactSourceId::new(name)?,
            ArtifactSourceTier::Workspace,
            kind,
            root,
            registry.clone(),
        )));
    }
    Ok(())
}

fn system_config_roots() -> Vec<PathBuf> {
    std::env::var_os("XDG_CONFIG_DIRS")
        .map(|value| {
            std::env::split_paths(&value)
                .map(|path| path.join("agentLIBRE"))
                .collect()
        })
        .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg/agentLIBRE")])
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
        candidates.push(ArtifactCandidate::new(
            package.type_id.parse()?,
            package.id.parse()?,
            package.version.parse()?,
            source_id.clone(),
            ArtifactSourceTier::Builtin,
            ArtifactSourceKind::Embedded,
            Arc::new(InMemoryPackageView::new(files)?),
        ));
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

    #[test]
    fn composition_registers_all_core_adapters_and_source_tiers() {
        let paths = AgentLibrePaths::from_agl_home(std::env::temp_dir().join("agl-app-test-home"));
        let workspace =
            std::env::temp_dir().join(format!("agl-app-composition-{}", std::process::id()));
        fs::create_dir_all(&workspace).unwrap();
        let composition = compose_artifacts(&paths, &workspace).unwrap();
        assert_eq!(composition.registry.iter().count(), 3);
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
}
