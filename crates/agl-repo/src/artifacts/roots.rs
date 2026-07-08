use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{ArtifactKind, ArtifactSourceStatus, ArtifactStatus, UndeclaredArtifactRoot};

pub(super) fn undeclared_artifact_roots(
    workspace_root: &Path,
    artifacts: &[ArtifactStatus],
    sources: &[ArtifactSourceStatus],
) -> Result<Vec<UndeclaredArtifactRoot>> {
    let agl_root = workspace_root.join(".agl");
    if !agl_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut declared_paths = artifacts
        .iter()
        .map(|artifact| artifact.path.clone())
        .collect::<Vec<_>>();
    declared_paths.extend(
        sources
            .iter()
            .filter(|source| source.path != Path::new(".agl"))
            .map(|source| source.path.clone()),
    );
    let mut roots = Vec::new();
    for entry in
        fs::read_dir(&agl_root).with_context(|| format!("failed to read {}", agl_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
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
