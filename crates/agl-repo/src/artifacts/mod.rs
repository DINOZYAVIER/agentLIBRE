mod lock;
mod path;
mod roots;
mod schema;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};

use lock::{
    artifact_definition_hash, artifact_lock_error_allows_refresh, read_artifact_lock,
    validate_locked_artifact,
};
use path::{
    artifact_access_permits, artifact_create_path, artifact_policy_error_blocks_writes,
    validate_artifact_path, validate_artifact_subpath, validate_no_symlink_escape,
};
use roots::undeclared_artifact_roots;
use schema::validate_artifact_schema;

use crate::{
    ARTIFACT_LOCK_PATH, ArtifactDataClass, ArtifactLock, ArtifactLockOptions, ArtifactLockReport,
    ArtifactReportState, ArtifactState, ArtifactStatus, ArtifactStatusOptions,
    ArtifactStatusReport, ArtifactSyncAction, ArtifactSyncActionKind, ArtifactSyncOptions,
    ArtifactSyncReport, ComponentHandle, ComponentPathHandleRequest, DEFAULT_PROFILE,
    LockedWorkspaceComponent, RepoManifest, WORKSPACE_MANIFEST_PATH, WorkspaceComponent,
    WorkspaceComponentKind, WorkspaceFunctions, component_status, is_not_found, read_manifest,
    resolve_repo_root,
};

#[derive(Clone, Debug)]
struct ResolvedArtifact {
    id: String,
    definition: WorkspaceComponent,
    kind: ArtifactDataClass,
    definition_hash: String,
}

pub fn status_artifacts(
    start: impl AsRef<Path>,
    options: &ArtifactStatusOptions,
) -> Result<ArtifactStatusReport> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let lock_path = workspace_root.join(ARTIFACT_LOCK_PATH);
    let (manifest, mut warnings, mut errors) = artifact_manifest_for_status(&workspace_root)?;
    let lock = read_artifact_lock(&lock_path, &mut errors);
    let resolved = resolve_artifacts(&workspace_root, &manifest, &mut errors);
    let mut all_artifacts = Vec::new();

    for resolved in resolved {
        let locked = lock
            .as_ref()
            .and_then(|lock| lock.components.get(&resolved.id));
        all_artifacts.push(artifact_status(
            &workspace_root,
            resolved,
            locked,
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
        for id in lock.components.keys() {
            if options.artifact.is_none()
                && !all_artifacts.iter().any(|artifact| &artifact.id == id)
            {
                warnings.push(format!("artifact_lock_stale: {id}"));
            }
        }
    }

    let undeclared = undeclared_artifact_roots(&workspace_root, &all_artifacts)?;
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
        next_steps.push("agl repo component sync".to_string());
    }
    if !errors.is_empty() {
        next_steps.push("inspect agl repo component status --json".to_string());
    } else if !all_artifacts.is_empty() && !lock_path.exists() {
        warnings.push("artifact_lock_missing".to_string());
        next_steps.push("agl repo component lock".to_string());
    }

    let state = artifact_report_state(&warnings, &errors);
    Ok(ArtifactStatusReport {
        state,
        workspace_root,
        manifest_path,
        lock_path,
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
        if artifact.kind == ArtifactDataClass::Cache {
            continue;
        }
        for create in &artifact.create {
            let relative_path = artifact_create_path(&artifact.path, create);
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
    let components = status
        .artifacts
        .iter()
        .map(|artifact| {
            (
                artifact.id.clone(),
                LockedWorkspaceComponent {
                    kind: Some(artifact.storage),
                    path: Some(artifact.path.clone()),
                    definition_digest: Some(artifact.definition_hash.clone()),
                    source_id: None,
                    source_kind: None,
                    url: artifact
                        .actual_url
                        .clone()
                        .or_else(|| artifact.expected_url.clone()),
                    rev: artifact.expected_rev.clone(),
                    commit: artifact
                        .actual_commit
                        .clone()
                        .or_else(|| artifact.expected_commit.clone()),
                    tree: artifact
                        .actual_tree
                        .clone()
                        .or_else(|| artifact.expected_tree.clone()),
                },
            )
        })
        .collect();
    let lock = ArtifactLock::new(components, BTreeMap::new())
        .map_err(|error| anyhow::anyhow!("failed to build artifact lock: {error}"))?;

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
            lock.write_atomic(&status.lock_path)
                .map_err(|error| anyhow::anyhow!("failed to write artifact lock: {error}"))?;
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

fn artifact_manifest_for_status(
    workspace_root: &Path,
) -> Result<(RepoManifest, Vec<String>, Vec<String>)> {
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    match read_manifest(&manifest_path) {
        Ok(manifest) => Ok((manifest, Vec::new(), Vec::new())),
        Err(err) if is_not_found(&err) => Ok((
            empty_workspace_manifest(),
            Vec::new(),
            vec!["workspace_manifest_missing".to_string()],
        )),
        Err(err) => Ok((
            RepoManifest {
                version: 2,
                profile: DEFAULT_PROFILE.to_string(),
                functions: WorkspaceFunctions::default(),
                sources: BTreeMap::new(),
                artifacts: BTreeMap::new(),
            },
            Vec::new(),
            vec![format!("workspace_manifest_invalid: {err:#}")],
        )),
    }
}

fn resolve_artifacts(
    workspace_root: &Path,
    manifest: &RepoManifest,
    errors: &mut Vec<String>,
) -> Vec<ResolvedArtifact> {
    let mut resolved = Vec::new();
    let mut seen_paths = BTreeMap::<PathBuf, String>::new();

    for (id, artifact) in &manifest.artifacts {
        validate_artifact_definition(workspace_root, id, artifact, errors);
        for (other_path, other_id) in &seen_paths {
            if artifact.path.starts_with(other_path) || other_path.starts_with(&artifact.path) {
                errors.push(format!(
                    "artifact.{id}.path_overlap: {} overlaps artifact {other_id} at {}",
                    artifact.path.display(),
                    other_path.display()
                ));
            }
        }
        seen_paths.insert(artifact.path.clone(), id.clone());
        let kind = artifact_kind(artifact.kind);
        resolved.push(ResolvedArtifact {
            id: id.clone(),
            definition: artifact.clone(),
            kind,
            definition_hash: artifact_definition_hash(id, artifact),
        });
    }

    resolved
}

fn empty_workspace_manifest() -> RepoManifest {
    RepoManifest {
        version: 2,
        profile: DEFAULT_PROFILE.to_string(),
        functions: WorkspaceFunctions::default(),
        sources: BTreeMap::new(),
        artifacts: BTreeMap::new(),
    }
}

fn artifact_kind(storage: WorkspaceComponentKind) -> ArtifactDataClass {
    match storage {
        WorkspaceComponentKind::Generated => ArtifactDataClass::Config,
        WorkspaceComponentKind::Ignored => ArtifactDataClass::State,
        WorkspaceComponentKind::Git
        | WorkspaceComponentKind::Submodule
        | WorkspaceComponentKind::Local => ArtifactDataClass::Package,
    }
}

fn validate_artifact_definition(
    workspace_root: &Path,
    id: &str,
    artifact: &WorkspaceComponent,
    errors: &mut Vec<String>,
) {
    if id.trim().is_empty() {
        errors.push("artifact.id_blank".to_string());
    }
    if let Err(err) = validate_artifact_path(&artifact.path) {
        errors.push(format!("artifact.{id}.path_invalid: {err:#}"));
    }
    for create in &artifact.create {
        if create.is_absolute() {
            errors.push(format!("artifact.{id}.create_dir_absolute"));
        }
        if create
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            errors.push(format!("artifact.{id}.create_dir_parent"));
        }
    }
    let absolute_path = workspace_root.join(&artifact.path);
    if absolute_path.exists()
        && let Err(err) = validate_no_symlink_escape(workspace_root, &absolute_path)
    {
        errors.push(format!("artifact.{id}.path_escape: {err:#}"));
    }
}

fn artifact_status(
    workspace_root: &Path,
    resolved: ResolvedArtifact,
    locked: Option<&LockedWorkspaceComponent>,
    strict_schema: bool,
) -> ArtifactStatus {
    let absolute_path = workspace_root.join(&resolved.definition.path);
    let exists = absolute_path.exists();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let locked_definition_hash = locked.and_then(|locked| locked.definition_digest.clone());

    let component_status = component_status(workspace_root, &resolved.id, &resolved.definition);
    warnings.extend(component_status.warnings.clone());
    errors.extend(component_status.errors.clone());
    if !resolved.definition.required {
        errors.retain(|error| error != "missing");
        if !exists && !warnings.iter().any(|warning| warning == "missing_optional") {
            warnings.push("missing_optional".to_string());
        }
    }

    if exists && !absolute_path.is_dir() {
        errors.push("not_directory".to_string());
    }
    if exists && absolute_path.is_dir() {
        validate_artifact_schema(
            workspace_root,
            &resolved.definition.path,
            resolved.definition.validation.as_deref(),
            strict_schema,
            &mut warnings,
            &mut errors,
        );
    }
    validate_locked_artifact(
        &resolved,
        locked,
        component_status.actual_url.as_deref(),
        component_status.actual_commit.as_deref(),
        component_status.actual_tree.as_deref(),
        &mut warnings,
        &mut errors,
    );

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
        id: resolved.id,
        storage: resolved.definition.kind,
        path: resolved.definition.path,
        kind: resolved.kind,
        access: resolved.definition.access,
        required: resolved.definition.required,
        validation: resolved.definition.validation,
        create: resolved.definition.create,
        state,
        exists,
        expected_url: resolved.definition.url,
        actual_url: component_status.actual_url,
        expected_rev: resolved.definition.rev,
        expected_commit: resolved.definition.commit,
        actual_commit: component_status.actual_commit,
        expected_tree: resolved.definition.tree,
        actual_tree: component_status.actual_tree,
        tracked_dirty: component_status.tracked_dirty,
        untracked_suspicious: component_status.untracked_suspicious,
        definition_hash: resolved.definition_hash,
        locked_definition_hash,
        warnings,
        errors,
    }
}

pub fn resolve_component_path_handle(
    start: impl AsRef<Path>,
    request: &ComponentPathHandleRequest,
) -> Result<ComponentHandle> {
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

    Ok(ComponentHandle {
        component_id: artifact.id.clone(),
        root: artifact.path.clone(),
        relative_path: request.path.clone(),
        path_in_artifact,
        kind: artifact.kind,
        access: artifact.access,
        validation: artifact.validation.clone(),
        definition_hash: artifact.definition_hash.clone(),
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
