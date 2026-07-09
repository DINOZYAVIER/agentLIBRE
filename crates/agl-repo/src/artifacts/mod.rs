mod defaults;
mod lock;
mod path;
mod roots;
mod schema;
mod source;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

pub(crate) use defaults::{default_artifact_sources, source_backed_artifact_source};
use lock::{
    artifact_contract_hash, artifact_lock_error_allows_refresh, read_artifact_lock, unix_ms_now,
    validate_locked_artifact,
};
use path::{
    artifact_access_permits, artifact_create_path, artifact_policy_error_blocks_writes,
    validate_artifact_path, validate_artifact_subpath, validate_no_symlink_escape,
};
use roots::undeclared_artifact_roots;
use schema::validate_artifact_schema;
use source::artifact_source_statuses;

use crate::{
    ARTIFACT_LOCK_PATH, ArtifactConflictPolicy, ArtifactContract, ArtifactHandle, ArtifactKind,
    ArtifactLockOptions, ArtifactLockReport, ArtifactPathHandleRequest, ArtifactReportState,
    ArtifactSource, ArtifactSourceStatus, ArtifactState, ArtifactStatus, ArtifactStatusOptions,
    ArtifactStatusReport, ArtifactSyncAction, ArtifactSyncActionKind, ArtifactSyncOptions,
    ArtifactSyncReport, DEFAULT_PROFILE, LockedArtifact, WORKSPACE_MANIFEST_PATH,
    WorkspaceFunctions, WorkspaceManifest, default_manifest, is_not_found, read_manifest,
    resolve_repo_root,
};

#[derive(Clone, Debug)]
struct ResolvedArtifactContract {
    source_id: String,
    source: ArtifactSource,
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
    let lock = read_artifact_lock(&lock_path, &mut errors);
    let sources = artifact_source_statuses(&workspace_root, &manifest);
    let resolved = resolve_artifact_contracts(&workspace_root, &manifest, &mut errors);
    let mut all_artifacts = Vec::new();

    for resolved in resolved {
        let source_status = sources
            .iter()
            .find(|source| source.id == resolved.source_id);
        let locked = lock
            .as_ref()
            .and_then(|lock| lock.artifacts.get(&resolved.contract.id));
        all_artifacts.push(artifact_status(
            &workspace_root,
            resolved,
            locked,
            source_status,
            options.strict,
        ));
    }

    let artifacts = if let Some(requested) = &options.artifact {
        all_artifacts
            .iter()
            .filter(|artifact| &artifact.id == requested)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        all_artifacts.clone()
    };

    if options.artifact.is_some() && artifacts.is_empty() {
        errors.push(format!(
            "artifact_not_found: {}",
            options.artifact.as_deref().unwrap_or_default()
        ));
    }

    for source in &sources {
        warnings.extend(
            source
                .warnings
                .iter()
                .map(|warning| format!("artifact_source.{}.{}", source.id, warning)),
        );
        errors.extend(
            source
                .errors
                .iter()
                .map(|error| format!("artifact_source.{}.{}", source.id, error)),
        );
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

    if let Some(lock) = &lock {
        for id in lock.artifacts.keys() {
            if options.artifact.is_none()
                && !all_artifacts.iter().any(|artifact| &artifact.id == id)
            {
                warnings.push(format!("artifact_lock_stale: {id}"));
            }
        }
    }

    let undeclared = undeclared_artifact_roots(&workspace_root, &all_artifacts, &sources)?;
    warnings.extend(undeclared.iter().map(|root| {
        format!(
            "undeclared_artifact_root: {} suggested_kind={:?} suggested_target={}",
            root.path.display(),
            root.suggested_kind,
            root.suggested_target.display()
        )
    }));

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
        sources,
        artifacts,
        undeclared,
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
    let mut errors = status.errors;
    let refresh_errors = errors
        .iter()
        .filter(|error| artifact_lock_error_allows_refresh(error))
        .cloned()
        .collect::<Vec<_>>();
    errors.retain(|error| !artifact_lock_error_allows_refresh(error));
    let lock = crate::ArtifactLockFile {
        locked_at_unix_ms: unix_ms_now(),
        version: 1,
        artifacts: status
            .artifacts
            .iter()
            .map(|artifact| {
                let source = status
                    .sources
                    .iter()
                    .find(|source| source.id == artifact.source_id);
                (
                    artifact.id.clone(),
                    LockedArtifact {
                        id: artifact.id.clone(),
                        source_id: artifact.source_id.clone(),
                        source_role: artifact.source_role,
                        source_kind: artifact.source_kind,
                        source_path: source
                            .map(|source| source.path.clone())
                            .unwrap_or_else(PathBuf::new),
                        source_url: source
                            .and_then(|source| source.actual_url.clone())
                            .or_else(|| source.and_then(|source| source.expected_url.clone())),
                        source_rev: source.and_then(|source| source.expected_rev.clone()),
                        source_commit: source
                            .and_then(|source| source.actual_commit.clone())
                            .or_else(|| source.and_then(|source| source.expected_commit.clone())),
                        source_tree: source
                            .and_then(|source| source.actual_tree.clone())
                            .or_else(|| source.and_then(|source| source.expected_tree.clone())),
                        path: artifact.path.clone(),
                        kind: artifact.kind,
                        access: artifact.access,
                        provides: artifact.provides.clone(),
                        schema: artifact.schema.clone(),
                        contract_hash: artifact.contract_hash.clone(),
                        materialized_paths: artifact
                            .create
                            .iter()
                            .map(|create| artifact_create_path(&artifact.path, &create.dir))
                            .collect(),
                    },
                )
            })
            .collect(),
    };

    let mut wrote = false;
    if errors.is_empty() && (!options.strict || warnings.is_empty()) {
        if options.dry_run {
            warnings.push("dry_run_no_lock_written".to_string());
            warnings.extend(
                refresh_errors
                    .iter()
                    .map(|error| format!("lock_refresh_pending: {error}")),
            );
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
            warnings.retain(|warning| {
                warning != "artifact_lock_missing" && !warning.ends_with(".lock_entry_missing")
            });
        }
    } else {
        errors.extend(refresh_errors);
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
                functions: WorkspaceFunctions::default(),
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
                source: source.clone(),
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

fn artifact_status(
    workspace_root: &Path,
    resolved: ResolvedArtifactContract,
    locked: Option<&LockedArtifact>,
    source_status: Option<&ArtifactSourceStatus>,
    strict_schema: bool,
) -> ArtifactStatus {
    let absolute_path = workspace_root.join(&resolved.contract.path);
    let exists = absolute_path.exists();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let locked_contract_hash = locked.map(|locked| locked.contract_hash.clone());

    if !exists && resolved.contract.required {
        errors.push("missing".to_string());
    } else if !exists {
        warnings.push("missing_optional".to_string());
    } else if !absolute_path.is_dir() {
        errors.push("not_directory".to_string());
    }
    if exists && absolute_path.is_dir() {
        validate_artifact_schema(
            workspace_root,
            &resolved.contract.path,
            resolved.contract.schema.as_deref(),
            strict_schema,
            &mut warnings,
            &mut errors,
        );
    }
    validate_locked_artifact(&resolved, locked, source_status, &mut warnings, &mut errors);

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
        source_role: resolved.source.role,
        source_kind: resolved.source.kind,
        path: resolved.contract.path,
        kind: resolved.contract.kind,
        access: resolved.contract.access,
        provides: resolved.contract.provides,
        schema: resolved.contract.schema,
        create: resolved.contract.create,
        state,
        exists,
        contract_hash: resolved.contract_hash,
        locked_contract_hash,
        warnings,
        errors,
    }
}

pub fn resolve_artifact_path_handle(
    start: impl AsRef<Path>,
    request: &ArtifactPathHandleRequest,
) -> Result<ArtifactHandle> {
    validate_artifact_subpath(&request.path)?;
    let status = status_artifacts(
        start,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )?;
    let blocking_errors = status
        .errors
        .iter()
        .filter(|error| artifact_policy_error_blocks_writes(error))
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        blocking_errors.is_empty(),
        "artifact write policy is invalid: {}",
        blocking_errors.join(", ")
    );

    let artifact = status
        .artifacts
        .iter()
        .filter(|artifact| request.path.starts_with(&artifact.path))
        .max_by_key(|artifact| artifact.path.components().count())
        .with_context(|| {
            format!(
                "repository artifact path is not declared: {}",
                request.path.display()
            )
        })?;
    ensure!(
        artifact_access_permits(artifact.access, request.access),
        "repository artifact does not permit {:?}: {} ({})",
        request.access,
        artifact.id,
        artifact.path.display()
    );
    ensure!(
        status.workspace_root.join(&artifact.path).is_dir(),
        "repository artifact root is not a directory: {} ({})",
        artifact.id,
        artifact.path.display()
    );

    let absolute_path = status.workspace_root.join(&request.path);
    if absolute_path.exists() {
        validate_no_symlink_escape(&status.workspace_root, &absolute_path)?;
    } else if let Some(parent) = absolute_path.parent()
        && parent.exists()
    {
        validate_no_symlink_escape(&status.workspace_root, parent)?;
    }
    let path_in_artifact = request
        .path
        .strip_prefix(&artifact.path)
        .unwrap_or_else(|_| Path::new(""))
        .to_path_buf();

    Ok(ArtifactHandle {
        artifact_id: artifact.id.clone(),
        source_id: artifact.source_id.clone(),
        root: artifact.path.clone(),
        relative_path: request.path.clone(),
        path_in_artifact,
        kind: artifact.kind,
        access: artifact.access,
        schema: artifact.schema.clone(),
        contract_hash: artifact.contract_hash.clone(),
    })
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
