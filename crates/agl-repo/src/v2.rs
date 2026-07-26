use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use agl_artifact::{
    ArtifactLock, LockedArtifactPackage, LockedWorkspaceComponent, WorkspaceManifest,
};
use anyhow::{Context, Result};

/// Read the breaking workspace v2 composition root.
pub fn read_workspace_manifest_v2(path: impl AsRef<Path>) -> Result<WorkspaceManifest> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read workspace manifest {}", path.display()))?;
    WorkspaceManifest::from_toml(&content)
        .map_err(|error| anyhow::anyhow!("failed to validate {}: {error}", path.display()))
}

/// Write a validated workspace v2 composition root.
pub fn write_workspace_manifest_v2(
    path: impl AsRef<Path>,
    manifest: &WorkspaceManifest,
) -> Result<()> {
    let path = path.as_ref();
    let content = manifest
        .to_toml()
        .map_err(|error| anyhow::anyhow!("failed to render workspace manifest: {error}"))?;
    let temporary = path.with_extension("toml.tmp");
    let result = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(anyhow::anyhow!(
            "failed to write workspace manifest {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

/// Read the combined v2 lock with distinct component and semantic package maps.
pub fn read_artifact_lock_v2(path: impl AsRef<Path>) -> Result<ArtifactLock> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read artifact lock {}", path.display()))?;
    ArtifactLock::from_toml(&content)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("failed to validate {}", path.display()))
}

/// Read the combined v2 lock when it exists.
///
/// Only a genuinely missing file is represented as `None`; malformed and
/// unreadable locks remain hard errors.
pub fn read_optional_artifact_lock_v2(path: impl AsRef<Path>) -> Result<Option<ArtifactLock>> {
    let path = path.as_ref();
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read artifact lock {}", path.display()));
        }
    };
    ArtifactLock::from_toml(&content)
        .map(Some)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("failed to validate {}", path.display()))
}

pub fn write_artifact_lock_v2(path: impl AsRef<Path>, lock: &ArtifactLock) -> Result<()> {
    lock.write_atomic(path)
        .map_err(|error| anyhow::anyhow!("failed to write artifact lock: {error}"))
}

pub fn replace_artifact_lock_packages_v2(
    path: impl AsRef<Path>,
    packages: BTreeMap<String, LockedArtifactPackage>,
) -> Result<ArtifactLock> {
    let path = path.as_ref();
    let components = read_optional_artifact_lock_v2(path)?
        .map(|lock| lock.components)
        .unwrap_or_default();
    let lock = ArtifactLock::new(components, packages)
        .map_err(anyhow::Error::new)
        .context("failed to assemble combined artifact lock")?;
    write_artifact_lock_v2(path, &lock)?;
    Ok(lock)
}

pub fn replace_artifact_lock_components_v2(
    path: impl AsRef<Path>,
    components: BTreeMap<String, LockedWorkspaceComponent>,
) -> Result<ArtifactLock> {
    let path = path.as_ref();
    let packages = read_optional_artifact_lock_v2(path)?
        .map(|lock| lock.packages)
        .unwrap_or_default();
    let lock = ArtifactLock::new(components, packages)
        .map_err(anyhow::Error::new)
        .context("failed to assemble combined artifact lock")?;
    write_artifact_lock_v2(path, &lock)?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_artifact::{ArtifactSourceKind, ArtifactSourceTier, WorkspaceComponentKind};

    fn lock_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agl-repo-v2-{label}-{}-artifact-lock.toml",
            std::process::id()
        ))
    }

    fn package() -> LockedArtifactPackage {
        LockedArtifactPackage {
            type_id: "function".parse().unwrap(),
            id: "fixture".parse().unwrap(),
            version: "1.0.0".parse().unwrap(),
            source_tier: ArtifactSourceTier::Workspace,
            source_kind: ArtifactSourceKind::Directory,
            source_id: "workspace".parse().unwrap(),
            package_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
            envelope_schema: "agentlibre.artifact/v1".parse().unwrap(),
            payload_schema: "agentlibre.function/v2".parse().unwrap(),
            source_revision: None,
            source_tree: None,
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn class_scoped_replacements_preserve_the_other_record_map() {
        let path = lock_path("class-merge");
        let _ = fs::remove_file(&path);
        let package = package();
        replace_artifact_lock_packages_v2(
            &path,
            BTreeMap::from([(package.key(), package.clone())]),
        )
        .unwrap();
        let component = LockedWorkspaceComponent {
            kind: Some(WorkspaceComponentKind::Local),
            path: Some(".agl/tasks".into()),
            definition_digest: Some("0".repeat(64)),
            ..Default::default()
        };
        let combined = replace_artifact_lock_components_v2(
            &path,
            BTreeMap::from([("tasks".to_owned(), component.clone())]),
        )
        .unwrap();
        assert_eq!(combined.packages.get(&package.key()), Some(&package));
        assert_eq!(combined.components.get("tasks"), Some(&component));

        let bytes = fs::read(&path).unwrap();
        replace_artifact_lock_packages_v2(&path, combined.packages.clone()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn optional_lock_read_only_maps_not_found_to_none() {
        let path = lock_path("optional-read");
        let _ = fs::remove_file(&path);
        assert!(read_optional_artifact_lock_v2(&path).unwrap().is_none());
        fs::write(&path, "not valid TOML = [").unwrap();
        assert!(read_optional_artifact_lock_v2(&path).is_err());
        fs::remove_file(path).unwrap();
    }
}
