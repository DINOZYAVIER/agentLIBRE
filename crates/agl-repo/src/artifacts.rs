use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    ARTIFACT_LOCK_PATH, ArtifactAccess, ArtifactConflictPolicy, ArtifactContract,
    ArtifactCreateRule, ArtifactKind, ArtifactLockOptions, ArtifactLockReport, ArtifactReportState,
    ArtifactSource, ArtifactSourceKind, ArtifactSourceRole, ArtifactState, ArtifactStatus,
    ArtifactStatusOptions, ArtifactStatusReport, ArtifactSyncAction, ArtifactSyncActionKind,
    ArtifactSyncOptions, ArtifactSyncReport, DEFAULT_PROFILE, LockedArtifact,
    WORKSPACE_MANIFEST_PATH, WorkspaceManifest, default_manifest, is_not_found, read_manifest,
    resolve_repo_root, validate_component_path,
};

#[derive(Clone, Debug)]
struct ResolvedArtifactContract {
    source_id: String,
    contract: ArtifactContract,
    contract_hash: String,
}

pub fn status_artifacts(
    start: impl AsRef<Path>,
    options: &ArtifactStatusOptions,
) -> Result<ArtifactStatusReport> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let lock_path = workspace_root.join(ARTIFACT_LOCK_PATH);
    let (manifest, mut warnings, mut errors) = artifact_manifest_or_compat(&workspace_root)?;
    let resolved = resolve_artifact_contracts(&workspace_root, &manifest, &mut errors);
    let mut artifacts = Vec::new();

    for resolved in resolved {
        if let Some(requested) = &options.artifact
            && requested != &resolved.contract.id
        {
            continue;
        }
        artifacts.push(artifact_status(&workspace_root, resolved));
    }

    if options.artifact.is_some() && artifacts.is_empty() {
        errors.push(format!(
            "artifact_not_found: {}",
            options.artifact.as_deref().unwrap_or_default()
        ));
    }

    for artifact in &artifacts {
        warnings.extend(
            artifact
                .warnings
                .iter()
                .map(|warning| format!("artifact.{}.{}", artifact.id, warning)),
        );
        errors.extend(
            artifact
                .errors
                .iter()
                .map(|error| format!("artifact.{}.{}", artifact.id, error)),
        );
    }

    let mut next_steps = Vec::new();
    if artifacts
        .iter()
        .any(|artifact| artifact.state == ArtifactState::Missing)
    {
        next_steps.push("agl repo artifact sync".to_string());
    }
    if !errors.is_empty() {
        next_steps.push("inspect agl repo artifact status --json".to_string());
    } else if !lock_path.exists() {
        warnings.push("artifact_lock_missing".to_string());
        next_steps.push("agl repo artifact lock".to_string());
    }

    let state = artifact_report_state(&warnings, &errors);
    Ok(ArtifactStatusReport {
        state,
        workspace_root,
        manifest_path,
        lock_path,
        artifacts,
        warnings,
        errors,
        next_steps,
    })
}

pub fn sync_artifacts(
    start: impl AsRef<Path>,
    options: &ArtifactSyncOptions,
) -> Result<ArtifactSyncReport> {
    let status = status_artifacts(
        start,
        &ArtifactStatusOptions {
            artifact: None,
            strict: options.strict,
        },
    )?;
    let mut actions = Vec::new();
    let mut warnings = status.warnings;
    let mut errors = status.errors;

    let blocking_errors = errors
        .iter()
        .any(|error| !error.ends_with(".missing") && error != "missing");
    if blocking_errors || (options.strict && !warnings.is_empty()) {
        return Ok(ArtifactSyncReport {
            workspace_root: status.workspace_root,
            manifest_path: status.manifest_path,
            dry_run: options.dry_run,
            actions,
            warnings,
            errors,
        });
    }

    for artifact in &status.artifacts {
        if artifact.create.is_empty() {
            actions.push(ArtifactSyncAction {
                artifact_id: artifact.id.clone(),
                path: artifact.path.clone(),
                action: if artifact.exists {
                    ArtifactSyncActionKind::Exists
                } else {
                    ArtifactSyncActionKind::SkippedNoCreateRule
                },
            });
            continue;
        }
        if artifact.kind == ArtifactKind::Cache {
            continue;
        }
        for create in &artifact.create {
            let relative_path = artifact_create_path(&artifact.path, &create.dir);
            let absolute_path = status.workspace_root.join(&relative_path);
            if absolute_path.exists() {
                actions.push(ArtifactSyncAction {
                    artifact_id: artifact.id.clone(),
                    path: relative_path,
                    action: ArtifactSyncActionKind::Exists,
                });
            } else if options.dry_run {
                actions.push(ArtifactSyncAction {
                    artifact_id: artifact.id.clone(),
                    path: relative_path,
                    action: ArtifactSyncActionKind::WouldCreateDir,
                });
            } else {
                let action_path = relative_path.clone();
                let error_path = relative_path.display().to_string();
                match fs::create_dir_all(&absolute_path) {
                    Ok(()) => actions.push(ArtifactSyncAction {
                        artifact_id: artifact.id.clone(),
                        path: action_path,
                        action: ArtifactSyncActionKind::CreatedDir,
                    }),
                    Err(err) => errors.push(format!(
                        "artifact.{}.create_failed: {}: {}",
                        artifact.id, error_path, err
                    )),
                }
            }
        }
    }

    if options.dry_run {
        errors.retain(|error| !error.ends_with(".missing") && error != "missing");
    } else {
        let refreshed = status_artifacts(
            &status.workspace_root,
            &ArtifactStatusOptions {
                artifact: None,
                strict: false,
            },
        )?;
        warnings = refreshed.warnings;
        errors = refreshed.errors;
    }

    Ok(ArtifactSyncReport {
        workspace_root: status.workspace_root,
        manifest_path: status.manifest_path,
        dry_run: options.dry_run,
        actions,
        warnings,
        errors,
    })
}

pub fn lock_artifacts(
    start: impl AsRef<Path>,
    options: &ArtifactLockOptions,
) -> Result<ArtifactLockReport> {
    let status = status_artifacts(
        start,
        &ArtifactStatusOptions {
            artifact: None,
            strict: options.strict,
        },
    )?;
    let mut warnings = status.warnings;
    let errors = status.errors;
    let lock = crate::ArtifactLockFile {
        version: 1,
        artifacts: status
            .artifacts
            .iter()
            .map(|artifact| {
                (
                    artifact.id.clone(),
                    LockedArtifact {
                        id: artifact.id.clone(),
                        source_id: artifact.source_id.clone(),
                        path: artifact.path.clone(),
                        kind: artifact.kind,
                        access: artifact.access,
                        provides: artifact.provides.clone(),
                        schema: artifact.schema.clone(),
                        contract_hash: artifact.contract_hash.clone(),
                    },
                )
            })
            .collect(),
    };

    let mut wrote = false;
    if errors.is_empty() && (!options.strict || warnings.is_empty()) {
        if options.dry_run {
            warnings.push("dry_run_no_lock_written".to_string());
        } else {
            if let Some(parent) = status.lock_path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create artifact lock dir {}", parent.display())
                })?;
            }
            let content =
                toml::to_string_pretty(&lock).context("failed to render artifact lock")?;
            fs::write(&status.lock_path, content).with_context(|| {
                format!(
                    "failed to write artifact lock {}",
                    status.lock_path.display()
                )
            })?;
            wrote = true;
            warnings.retain(|warning| warning != "artifact_lock_missing");
        }
    }

    Ok(ArtifactLockReport {
        workspace_root: status.workspace_root,
        lock_path: status.lock_path,
        dry_run: options.dry_run,
        wrote,
        lock,
        warnings,
        errors,
    })
}

pub(crate) fn default_artifact_sources() -> BTreeMap<String, ArtifactSource> {
    BTreeMap::from([(
        "workspace".to_string(),
        ArtifactSource {
            role: ArtifactSourceRole::Compatibility,
            kind: ArtifactSourceKind::Compatibility,
            path: PathBuf::from(".agl"),
            url: None,
            rev: None,
            required: true,
            provides: vec![
                "tasks".to_string(),
                "skills".to_string(),
                "review-packs".to_string(),
                "local-state".to_string(),
            ],
            artifacts: vec![
                ArtifactContract {
                    id: "tasks".to_string(),
                    kind: ArtifactKind::Source,
                    path: PathBuf::from(".agl/tasks"),
                    access: ArtifactAccess::ReadWrite,
                    provides: vec!["tasks".to_string()],
                    schema: Some("agl.task_spec_legacy.v1".to_string()),
                    create: vec![ArtifactCreateRule {
                        dir: PathBuf::from("."),
                    }],
                    required: true,
                    shared: true,
                    conflict_policy: ArtifactConflictPolicy::Identical,
                },
                ArtifactContract {
                    id: "reviews".to_string(),
                    kind: ArtifactKind::Generated,
                    path: PathBuf::from(".agl/reviews"),
                    access: ArtifactAccess::ReadWrite,
                    provides: vec!["review-packs".to_string()],
                    schema: Some("agl.review_pack.v1".to_string()),
                    create: vec![ArtifactCreateRule {
                        dir: PathBuf::from("."),
                    }],
                    required: true,
                    shared: true,
                    conflict_policy: ArtifactConflictPolicy::Identical,
                },
                ArtifactContract {
                    id: "state".to_string(),
                    kind: ArtifactKind::State,
                    path: PathBuf::from(".agl/state"),
                    access: ArtifactAccess::ReadWrite,
                    provides: vec!["local-state".to_string()],
                    schema: None,
                    create: vec![ArtifactCreateRule {
                        dir: PathBuf::from("."),
                    }],
                    required: false,
                    shared: true,
                    conflict_policy: ArtifactConflictPolicy::Identical,
                },
                ArtifactContract {
                    id: "skills".to_string(),
                    kind: ArtifactKind::Source,
                    path: PathBuf::from(".agl/skills"),
                    access: ArtifactAccess::Read,
                    provides: vec!["skills".to_string()],
                    schema: Some("agl.skill_source_legacy.v1".to_string()),
                    create: Vec::new(),
                    required: false,
                    shared: true,
                    conflict_policy: ArtifactConflictPolicy::Identical,
                },
            ],
        },
    )])
}

fn artifact_manifest_or_compat(
    workspace_root: &Path,
) -> Result<(WorkspaceManifest, Vec<String>, Vec<String>)> {
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    match read_manifest(&manifest_path) {
        Ok(manifest) => Ok((manifest, Vec::new(), Vec::new())),
        Err(err) if is_not_found(&err) => {
            let mut manifest = default_manifest();
            manifest.components.clear();
            Ok((
                manifest,
                vec!["workspace_manifest_missing".to_string()],
                Vec::new(),
            ))
        }
        Err(err) => Ok((
            WorkspaceManifest {
                version: 1,
                profile: DEFAULT_PROFILE.to_string(),
                components: BTreeMap::new(),
                artifact_sources: default_artifact_sources(),
            },
            Vec::new(),
            vec![format!("workspace_manifest_invalid: {err:#}")],
        )),
    }
}

fn resolve_artifact_contracts(
    workspace_root: &Path,
    manifest: &WorkspaceManifest,
    errors: &mut Vec<String>,
) -> Vec<ResolvedArtifactContract> {
    let mut resolved = Vec::new();
    let mut seen_ids = BTreeMap::<String, ArtifactContract>::new();
    let mut seen_paths = BTreeMap::<PathBuf, ArtifactContract>::new();

    for (source_id, source) in &manifest.artifact_sources {
        if let Err(err) = validate_artifact_path(&source.path) {
            errors.push(format!("artifact_source.{source_id}.path_invalid: {err:#}"));
        }
        for contract in &source.artifacts {
            validate_artifact_contract(workspace_root, source_id, contract, errors);
            if let Some(existing) = seen_ids.insert(contract.id.clone(), contract.clone())
                && existing != *contract
            {
                errors.push(format!("artifact.{}.duplicate_id_conflict", contract.id));
            }
            if let Some(existing) = seen_paths.insert(contract.path.clone(), contract.clone())
                && existing != *contract
                && (!existing.shared
                    || !contract.shared
                    || existing.conflict_policy != ArtifactConflictPolicy::Identical
                    || contract.conflict_policy != ArtifactConflictPolicy::Identical)
            {
                errors.push(format!(
                    "artifact.{}.path_conflict: {}",
                    contract.id,
                    contract.path.display()
                ));
            }
            resolved.push(ResolvedArtifactContract {
                source_id: source_id.clone(),
                contract: contract.clone(),
                contract_hash: artifact_contract_hash(source_id, contract),
            });
        }
    }

    resolved
}

fn validate_artifact_contract(
    workspace_root: &Path,
    source_id: &str,
    contract: &ArtifactContract,
    errors: &mut Vec<String>,
) {
    if contract.id.trim().is_empty() {
        errors.push(format!("artifact_source.{source_id}.artifact_id_blank"));
    }
    if let Err(err) = validate_artifact_path(&contract.path) {
        errors.push(format!("artifact.{}.path_invalid: {err:#}", contract.id));
    }
    for create in &contract.create {
        if create.dir.is_absolute() {
            errors.push(format!("artifact.{}.create_dir_absolute", contract.id));
        }
        if create
            .dir
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            errors.push(format!("artifact.{}.create_dir_parent", contract.id));
        }
    }
    let absolute_path = workspace_root.join(&contract.path);
    if absolute_path.exists()
        && let Err(err) = validate_no_symlink_escape(workspace_root, &absolute_path)
    {
        errors.push(format!("artifact.{}.path_escape: {err:#}", contract.id));
    }
}

fn artifact_status(workspace_root: &Path, resolved: ResolvedArtifactContract) -> ArtifactStatus {
    let absolute_path = workspace_root.join(&resolved.contract.path);
    let exists = absolute_path.exists();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    if !exists && resolved.contract.required {
        errors.push("missing".to_string());
    } else if !exists {
        warnings.push("missing_optional".to_string());
    } else if !absolute_path.is_dir() {
        errors.push("not_directory".to_string());
    }

    let state = if !errors.is_empty() {
        if errors.iter().any(|error| error == "missing") {
            ArtifactState::Missing
        } else {
            ArtifactState::Invalid
        }
    } else if !warnings.is_empty() {
        ArtifactState::Warning
    } else {
        ArtifactState::Ok
    };

    ArtifactStatus {
        id: resolved.contract.id,
        source_id: resolved.source_id,
        path: resolved.contract.path,
        kind: resolved.contract.kind,
        access: resolved.contract.access,
        provides: resolved.contract.provides,
        schema: resolved.contract.schema,
        create: resolved.contract.create,
        state,
        exists,
        contract_hash: resolved.contract_hash,
        warnings,
        errors,
    }
}

fn artifact_create_path(artifact_path: &Path, create_dir: &Path) -> PathBuf {
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

fn validate_artifact_path(path: &Path) -> Result<()> {
    validate_component_path(path)?;
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(component)) if component == ".agl" => {}
        _ => bail!("path must be under .agl"),
    }
    Ok(())
}

fn validate_no_symlink_escape(workspace_root: &Path, absolute_path: &Path) -> Result<()> {
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

fn artifact_contract_hash(source_id: &str, contract: &ArtifactContract) -> String {
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

fn artifact_report_state(warnings: &[String], errors: &[String]) -> ArtifactReportState {
    if !errors.is_empty() {
        ArtifactReportState::Invalid
    } else if !warnings.is_empty() {
        ArtifactReportState::Warning
    } else {
        ArtifactReportState::Ok
    }
}
