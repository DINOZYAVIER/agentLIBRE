use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::{ArtifactAccess, validate_component_path};

pub(super) fn artifact_create_path(artifact_path: &Path, create_dir: &Path) -> PathBuf {
    let mut path = artifact_path.to_path_buf();
    for component in create_dir.components() {
        match component {
            Component::Normal(segment) => path.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    path
}

pub(super) fn validate_artifact_subpath(path: &Path) -> Result<()> {
    validate_artifact_path(path)?;
    ensure!(
        path.components().count() > 1,
        "artifact path must include an artifact root"
    );
    Ok(())
}

pub(super) fn validate_artifact_path(path: &Path) -> Result<()> {
    validate_component_path(path)?;
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(component)) if component == ".agl" => {}
        _ => bail!("path must be under .agl"),
    }
    Ok(())
}

pub(super) fn artifact_access_permits(actual: ArtifactAccess, requested: ArtifactAccess) -> bool {
    match requested {
        ArtifactAccess::Read => matches!(actual, ArtifactAccess::Read | ArtifactAccess::ReadWrite),
        ArtifactAccess::Write => {
            matches!(actual, ArtifactAccess::Write | ArtifactAccess::ReadWrite)
        }
        ArtifactAccess::ReadWrite => actual == ArtifactAccess::ReadWrite,
    }
}

pub(super) fn artifact_policy_error_blocks_writes(error: &str) -> bool {
    error.starts_with("workspace_manifest_invalid")
        || error.starts_with("artifact_lock_invalid")
        || error.contains("path_invalid")
        || error.contains("path_conflict")
        || error.contains("path_escape")
        || error.contains("path_changed")
        || error.contains("not_directory")
        || error.contains("duplicate_id_conflict")
        || error.contains("contract_changed")
        || error.contains("source_id_changed")
        || error.contains("source_role_changed")
        || error.contains("source_kind_changed")
        || error.contains("source_path_changed")
        || error.contains("source_url_changed")
        || error.contains("source_rev_changed")
        || error.contains("source_commit_changed")
        || error.contains("source_tree_changed")
}

pub(super) fn validate_no_symlink_escape(
    workspace_root: &Path,
    absolute_path: &Path,
) -> Result<()> {
    let workspace_root = workspace_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", workspace_root.display()))?;
    let path = absolute_path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", absolute_path.display()))?;
    if !path.starts_with(&workspace_root) {
        bail!("path escapes workspace root");
    }
    Ok(())
}
