use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::ResolvedArtifact;
use crate::{ArtifactLock, LockedWorkspaceComponent, WorkspaceComponent};

pub(super) fn read_artifact_lock(
    lock_path: &Path,
    errors: &mut Vec<String>,
) -> Option<ArtifactLock> {
    match fs::read_to_string(lock_path) {
        Ok(content) => match ArtifactLock::from_toml(&content) {
            Ok(lock) => Some(lock),
            Err(err) => {
                errors.push(format!("artifact_lock_invalid: {err:#}"));
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            errors.push(format!("artifact_lock_read_failed: {err}"));
            None
        }
    }
}

pub(super) fn validate_locked_artifact(
    resolved: &ResolvedArtifact,
    locked: Option<&LockedWorkspaceComponent>,
    actual_url: Option<&str>,
    actual_commit: Option<&str>,
    actual_tree: Option<&str>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let Some(locked) = locked else {
        warnings.push("lock_entry_missing".to_string());
        return;
    };
    if locked.definition_digest.as_deref() != Some(resolved.definition_hash.as_str()) {
        errors.push("definition_changed".to_string());
    }
    if locked.kind != Some(resolved.definition.kind) {
        errors.push("component_kind_changed".to_string());
    }
    if locked.path.as_ref() != Some(&resolved.definition.path) {
        errors.push("path_changed".to_string());
    }

    let expected_url = actual_url
        .map(ToOwned::to_owned)
        .or_else(|| resolved.definition.url.clone());
    let expected_commit = actual_commit
        .map(ToOwned::to_owned)
        .or_else(|| resolved.definition.commit.clone());
    let expected_tree = actual_tree
        .map(ToOwned::to_owned)
        .or_else(|| resolved.definition.tree.clone());
    if locked.url != expected_url {
        errors.push("url_changed".to_string());
    }
    if locked.rev != resolved.definition.rev {
        errors.push("rev_changed".to_string());
    }
    if locked.commit != expected_commit {
        errors.push("commit_changed".to_string());
    }
    if locked.tree != expected_tree {
        errors.push("tree_changed".to_string());
    }
}

pub(super) fn artifact_lock_error_allows_refresh(error: &str) -> bool {
    error.ends_with(".definition_changed")
        || error.ends_with(".component_kind_changed")
        || error.ends_with(".path_changed")
        || error.ends_with(".url_changed")
        || error.ends_with(".rev_changed")
        || error.ends_with(".commit_changed")
        || error.ends_with(".tree_changed")
}

pub(super) fn artifact_definition_hash(id: &str, artifact: &WorkspaceComponent) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        toml::to_string(artifact)
            .expect("artifact definition serializes")
            .as_bytes(),
    );
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
