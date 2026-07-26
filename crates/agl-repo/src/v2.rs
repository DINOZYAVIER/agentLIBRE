use std::fs;
use std::io::Write;
use std::path::Path;

use agl_artifact::{ArtifactLock, WorkspaceManifest};
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
        .map_err(|error| anyhow::anyhow!("failed to validate {}: {error}", path.display()))
}

pub fn write_artifact_lock_v2(path: impl AsRef<Path>, lock: &ArtifactLock) -> Result<()> {
    lock.write_atomic(path)
        .map_err(|error| anyhow::anyhow!("failed to write artifact lock: {error}"))
}
