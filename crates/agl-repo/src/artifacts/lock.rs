use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::ResolvedArtifactContract;
use crate::{ArtifactContract, ArtifactLockFile, ArtifactSourceStatus, LockedArtifact};

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
    resolved: &ResolvedArtifactContract,
    locked: Option<&LockedArtifact>,
    source_status: Option<&ArtifactSourceStatus>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let Some(locked) = locked else {
        warnings.push("lock_entry_missing".to_string());
        return;
    };
    if locked.contract_hash != resolved.contract_hash {
        errors.push("contract_changed".to_string());
    }
    if locked.source_id != resolved.source_id {
        errors.push("source_id_changed".to_string());
    }
    if locked.source_role != resolved.source.role {
        errors.push("source_role_changed".to_string());
    }
    if locked.source_kind != resolved.source.kind {
        errors.push("source_kind_changed".to_string());
    }
    if locked.source_path != resolved.source.path {
        errors.push("source_path_changed".to_string());
    }
    if locked.path != resolved.contract.path {
        errors.push("path_changed".to_string());
    }

    let expected_url = source_status
        .and_then(|source| source.actual_url.clone())
        .or_else(|| resolved.source.url.clone());
    let expected_commit = source_status
        .and_then(|source| source.actual_commit.clone())
        .or_else(|| resolved.source.commit.clone());
    let expected_tree = source_status
        .and_then(|source| source.actual_tree.clone())
        .or_else(|| resolved.source.tree.clone());
    if locked.source_url != expected_url {
        errors.push("source_url_changed".to_string());
    }
    if locked.source_rev != resolved.source.rev {
        errors.push("source_rev_changed".to_string());
    }
    if locked.source_commit != expected_commit {
        errors.push("source_commit_changed".to_string());
    }
    if locked.source_tree != expected_tree {
        errors.push("source_tree_changed".to_string());
    }
}

pub(super) fn artifact_lock_error_allows_refresh(error: &str) -> bool {
    error.ends_with(".contract_changed")
        || error.ends_with(".source_id_changed")
        || error.ends_with(".source_role_changed")
        || error.ends_with(".source_kind_changed")
        || error.ends_with(".source_path_changed")
        || error.ends_with(".source_url_changed")
        || error.ends_with(".source_rev_changed")
        || error.ends_with(".source_commit_changed")
        || error.ends_with(".source_tree_changed")
        || error.ends_with(".path_changed")
}

pub(super) fn artifact_contract_hash(source_id: &str, contract: &ArtifactContract) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        toml::to_string(contract)
            .expect("artifact contract serializes")
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
