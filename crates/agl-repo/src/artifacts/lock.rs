use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::ResolvedArtifact;
use crate::{ArtifactLockFile, LockedArtifact, WorkspaceArtifact};

pub(super) fn read_artifact_lock(
    lock_path: &Path,
    errors: &mut Vec<String>,
) -> Option<ArtifactLockFile> {
    match fs::read_to_string(lock_path) {
        Ok(content) => match toml::from_str::<ArtifactLockFile>(&content) {
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
    locked: Option<&LockedArtifact>,
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
    if locked.definition_hash != resolved.definition_hash {
        errors.push("definition_changed".to_string());
    }
    if locked.storage != resolved.definition.kind {
        errors.push("storage_changed".to_string());
    }
    if locked.path != resolved.definition.path {
        errors.push("path_changed".to_string());
    }
    if locked.required != resolved.definition.required {
        errors.push("required_changed".to_string());
    }
    if locked.kind != resolved.kind {
        errors.push("kind_changed".to_string());
    }
    if locked.access != resolved.definition.access {
        errors.push("access_changed".to_string());
    }
    if locked.validation != resolved.definition.validation {
        errors.push("validation_changed".to_string());
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
        || error.ends_with(".storage_changed")
        || error.ends_with(".path_changed")
        || error.ends_with(".required_changed")
        || error.ends_with(".kind_changed")
        || error.ends_with(".access_changed")
        || error.ends_with(".validation_changed")
        || error.ends_with(".url_changed")
        || error.ends_with(".rev_changed")
        || error.ends_with(".commit_changed")
        || error.ends_with(".tree_changed")
}

pub(super) fn artifact_definition_hash(id: &str, artifact: &WorkspaceArtifact) -> String {
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

pub(super) fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
