use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{ArtifactKind, ArtifactStatus, UndeclaredArtifactRoot};

const NON_ARTIFACT_WORKSPACE_ROOTS: [&str; 2] = ["functions", "inference"];

pub(super) fn undeclared_artifact_roots(
    workspace_root: &Path,
    artifacts: &[ArtifactStatus],
) -> Result<Vec<UndeclaredArtifactRoot>> {
    let agl_root = workspace_root.join(".agl");
    if !agl_root.is_dir() {
        return Ok(Vec::new());
    }
    let declared_paths = artifacts
        .iter()
        .map(|artifact| artifact.path.clone())
        .collect::<Vec<_>>();
    let mut roots = Vec::new();
    for entry in
        fs::read_dir(&agl_root).with_context(|| format!("failed to read {}", agl_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| NON_ARTIFACT_WORKSPACE_ROOTS.contains(&name))
        {
            continue;
        }
        let relative = PathBuf::from(".agl").join(entry.file_name());
        if declared_paths
            .iter()
            .any(|declared| relative.starts_with(declared) || declared.starts_with(&relative))
        {
            continue;
        }
        let (suggested_kind, suggested_target) = suggested_migration_target(&relative);
        roots.push(UndeclaredArtifactRoot {
            path: relative,
            suggested_kind,
            suggested_target,
        });
    }
    roots.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(roots)
}

fn suggested_migration_target(path: &Path) -> (ArtifactKind, PathBuf) {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("cache") => (ArtifactKind::Cache, path.to_path_buf()),
        Some("sources") => (ArtifactKind::Source, path.to_path_buf()),
        Some(name) => (
            ArtifactKind::Generated,
            PathBuf::from(".agl/generated").join(name),
        ),
        None => (ArtifactKind::Generated, path.to_path_buf()),
    }
}
