use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::{
    ARTIFACT_LOCK_PATH, ArtifactAccess, ArtifactConflictPolicy, ArtifactContract,
    ArtifactCreateRule, ArtifactHandle, ArtifactKind, ArtifactLockFile, ArtifactLockOptions,
    ArtifactLockReport, ArtifactPathHandleRequest, ArtifactReportState, ArtifactSource,
    ArtifactSourceKind, ArtifactSourceRole, ArtifactSourceState, ArtifactSourceStatus,
    ArtifactState, ArtifactStatus, ArtifactStatusOptions, ArtifactStatusReport, ArtifactSyncAction,
    ArtifactSyncActionKind, ArtifactSyncOptions, ArtifactSyncReport, ComponentKind,
    DEFAULT_PROFILE, LockedArtifact, UndeclaredArtifactRoot, WORKSPACE_MANIFEST_PATH,
    WorkspaceComponent, WorkspaceManifest, component_status, default_manifest, is_not_found,
    read_manifest, resolve_repo_root, validate_component_path, validate_task_spec_markdown,
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
                        source_role: Some(artifact.source_role),
                        source_kind: Some(artifact.source_kind),
                        source_path: Some(
                            source
                                .map(|source| source.path.clone())
                                .unwrap_or_else(PathBuf::new),
                        ),
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
                warning != "artifact_lock_missing"
                    && !warning.ends_with(".lock_entry_missing")
                    && !artifact_lock_warning_resolved_by_write(warning)
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

pub(crate) fn default_artifact_sources() -> BTreeMap<String, ArtifactSource> {
    BTreeMap::from([
        (
            "skills".to_string(),
            default_single_artifact_source("skills", ArtifactSourceKind::Submodule),
        ),
        (
            "tasks".to_string(),
            default_single_artifact_source("tasks", ArtifactSourceKind::Local),
        ),
        (
            "reviews".to_string(),
            default_single_artifact_source("reviews", ArtifactSourceKind::Generated),
        ),
        (
            "decision-docs".to_string(),
            default_single_artifact_source("decision-docs", ArtifactSourceKind::Generated),
        ),
        (
            "smoke".to_string(),
            default_single_artifact_source("smoke", ArtifactSourceKind::Generated),
        ),
        (
            "handoffs".to_string(),
            default_single_artifact_source("handoffs", ArtifactSourceKind::Local),
        ),
        (
            "state".to_string(),
            default_single_artifact_source("state", ArtifactSourceKind::Ignored),
        ),
    ])
}

pub(crate) fn source_backed_artifact_source(
    id: &str,
    path: PathBuf,
    kind: ArtifactSourceKind,
    url: Option<String>,
    rev: Option<String>,
) -> ArtifactSource {
    let mut source = default_single_artifact_source_with_path(id, path, kind);
    source.url = url;
    source.rev = rev;
    source.required = true;
    if let Some(contract) = source.artifacts.first_mut() {
        contract.required = true;
    }
    source
}

fn default_single_artifact_source(id: &str, kind: ArtifactSourceKind) -> ArtifactSource {
    default_single_artifact_source_with_path(id, default_artifact_path(id), kind)
}

fn default_single_artifact_source_with_path(
    id: &str,
    path: PathBuf,
    kind: ArtifactSourceKind,
) -> ArtifactSource {
    ArtifactSource {
        role: default_artifact_source_role(id),
        kind,
        path: path.clone(),
        url: default_artifact_source_url(id),
        rev: default_artifact_source_rev(id),
        commit: None,
        tree: None,
        required: default_artifact_source_required(id),
        provides: default_artifact_provides(id),
        artifacts: vec![default_artifact_contract(id, path)],
    }
}

fn default_artifact_source_role(id: &str) -> ArtifactSourceRole {
    match id {
        "skills" => ArtifactSourceRole::Core,
        "reviews" | "decision-docs" | "smoke" => ArtifactSourceRole::Generated,
        "state" => ArtifactSourceRole::State,
        "tasks" | "handoffs" => ArtifactSourceRole::Planning,
        _ => ArtifactSourceRole::Local,
    }
}

fn default_artifact_source_url(id: &str) -> Option<String> {
    match id {
        "skills" => Some(crate::DEFAULT_SKILLS_URL.to_string()),
        _ => None,
    }
}

fn default_artifact_source_rev(id: &str) -> Option<String> {
    match id {
        "skills" => Some(crate::DEFAULT_SKILLS_REV.to_string()),
        _ => None,
    }
}

fn default_artifact_source_required(id: &str) -> bool {
    matches!(id, "tasks" | "reviews" | "state")
}

fn default_artifact_path(id: &str) -> PathBuf {
    PathBuf::from(format!(".agl/{id}"))
}

fn default_artifact_provides(id: &str) -> Vec<String> {
    match id {
        "skills" => vec!["skills".to_string(), "core-skills".to_string()],
        "tasks" => vec!["tasks".to_string(), "specs".to_string()],
        "reviews" => vec!["review-packs".to_string()],
        "decision-docs" => vec!["decision-docs".to_string(), "decisions".to_string()],
        "smoke" => vec!["smoke-artifacts".to_string(), "smoke-suites".to_string()],
        "handoffs" => vec!["handoffs".to_string()],
        "state" => vec![
            "local-state".to_string(),
            "notes".to_string(),
            "memory".to_string(),
            "matrix".to_string(),
            "cron".to_string(),
            "sessions".to_string(),
            "logs".to_string(),
            "cache".to_string(),
        ],
        other => vec![other.to_string()],
    }
}

fn default_artifact_contract(id: &str, path: PathBuf) -> ArtifactContract {
    ArtifactContract {
        id: id.to_string(),
        kind: default_artifact_kind(id),
        path,
        access: default_artifact_access(id),
        provides: default_artifact_provides(id),
        schema: default_artifact_schema(id),
        create: default_artifact_create_rules(id),
        required: default_artifact_contract_required(id),
        shared: true,
        conflict_policy: ArtifactConflictPolicy::Identical,
    }
}

fn default_artifact_kind(id: &str) -> ArtifactKind {
    match id {
        "reviews" | "decision-docs" | "smoke" => ArtifactKind::Generated,
        "state" => ArtifactKind::State,
        _ => ArtifactKind::Source,
    }
}

fn default_artifact_access(id: &str) -> ArtifactAccess {
    match id {
        "skills" => ArtifactAccess::Read,
        _ => ArtifactAccess::ReadWrite,
    }
}

fn default_artifact_schema(id: &str) -> Option<String> {
    match id {
        "skills" => Some("agl.skill_source.v1".to_string()),
        "tasks" => Some("agl.task_spec.v1".to_string()),
        "reviews" => Some("agl.review_pack.v1".to_string()),
        "decision-docs" => Some("agl.decision_doc.v1".to_string()),
        "smoke" => Some("agl.smoke.v1".to_string()),
        "handoffs" => Some("agl.handoff_markdown.v1".to_string()),
        _ => None,
    }
}

fn default_artifact_create_rules(id: &str) -> Vec<ArtifactCreateRule> {
    match id {
        "skills" => Vec::new(),
        _ => vec![ArtifactCreateRule {
            dir: PathBuf::from("."),
        }],
    }
}

fn default_artifact_contract_required(id: &str) -> bool {
    matches!(id, "tasks" | "reviews")
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

fn artifact_source_statuses(
    workspace_root: &Path,
    manifest: &WorkspaceManifest,
) -> Vec<ArtifactSourceStatus> {
    manifest
        .artifact_sources
        .iter()
        .map(|(source_id, source)| artifact_source_status(workspace_root, source_id, source))
        .collect()
}

fn artifact_source_status(
    workspace_root: &Path,
    source_id: &str,
    source: &ArtifactSource,
) -> ArtifactSourceStatus {
    let component_kind = match source.kind {
        ArtifactSourceKind::Git => ComponentKind::Git,
        ArtifactSourceKind::Submodule => ComponentKind::Submodule,
        ArtifactSourceKind::Local | ArtifactSourceKind::Compatibility => ComponentKind::Local,
        ArtifactSourceKind::Generated => ComponentKind::Generated,
        ArtifactSourceKind::Ignored => ComponentKind::Ignored,
    };
    let component = WorkspaceComponent {
        path: source.path.clone(),
        kind: component_kind,
        url: source.url.clone(),
        rev: source.rev.clone(),
        commit: source.commit.clone(),
        tree: source.tree.clone(),
        lock: None,
    };
    let component_status = component_status(workspace_root, source_id, &component);
    let mut warnings = component_status.warnings.clone();
    let mut errors = component_status.errors.clone();

    if !source.required && errors.iter().any(|error| error == "missing") {
        errors.retain(|error| error != "missing");
        warnings.push("missing_optional".to_string());
    }

    let state = if component_status.exists {
        if !errors.is_empty() {
            ArtifactSourceState::Invalid
        } else if !warnings.is_empty() {
            ArtifactSourceState::Warning
        } else {
            ArtifactSourceState::Ok
        }
    } else if source.required {
        ArtifactSourceState::Missing
    } else {
        ArtifactSourceState::Warning
    };

    ArtifactSourceStatus {
        id: source_id.to_string(),
        role: source.role,
        kind: source.kind,
        path: source.path.clone(),
        required: source.required,
        exists: component_status.exists,
        state,
        expected_url: source.url.clone(),
        actual_url: component_status.actual_url,
        expected_rev: source.rev.clone(),
        expected_commit: source.commit.clone(),
        actual_commit: component_status.actual_commit,
        expected_tree: source.tree.clone(),
        actual_tree: component_status.actual_tree,
        tracked_dirty: component_status.tracked_dirty,
        untracked_suspicious: component_status.untracked_suspicious,
        warnings,
        errors,
    }
}

fn read_artifact_lock(lock_path: &Path, errors: &mut Vec<String>) -> Option<ArtifactLockFile> {
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

fn validate_locked_artifact(
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
    match locked.source_role {
        Some(role) if role != resolved.source.role => {
            errors.push("source_role_changed".to_string())
        }
        Some(_) => {}
        None => warnings.push("source_role_missing".to_string()),
    }
    match locked.source_kind {
        Some(kind) if kind != resolved.source.kind => {
            errors.push("source_kind_changed".to_string())
        }
        Some(_) => {}
        None => warnings.push("source_kind_missing".to_string()),
    }
    match &locked.source_path {
        Some(path) if path != &resolved.source.path => {
            errors.push("source_path_changed".to_string())
        }
        Some(_) => {}
        None => warnings.push("source_path_missing".to_string()),
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

fn undeclared_artifact_roots(
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

fn validate_artifact_schema(
    workspace_root: &Path,
    root: &Path,
    schema: Option<&str>,
    strict: bool,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let Some(schema) = schema else {
        return;
    };
    let absolute_root = workspace_root.join(root);
    let mut schema_errors = Vec::new();
    match schema {
        "agl.task_spec.v1" | "agl.task_spec_legacy.v1" => {
            validate_task_spec_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.review_pack.v1" => {
            validate_review_pack_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.decision_doc.v1" | "agl.decision_doc_legacy.v1" => {
            validate_decision_doc_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.handoff_markdown.v1" => {
            validate_handoff_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.smoke.v1"
        | "agl.smoke_legacy.v1"
        | "agl.skill_source.v1"
        | "agl.skill_source_legacy.v1" => {}
        _ => warnings.push(format!("schema_validator_unknown: {schema}")),
    }
    if strict {
        errors.extend(schema_errors);
    } else {
        warnings.extend(schema_errors);
    }
}

fn validate_task_spec_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if !file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        match fs::read_to_string(&file) {
            Ok(content) => {
                let validation = validate_task_spec_markdown(&content);
                if !validation.is_valid() {
                    errors.push(format!(
                        "schema_invalid: {} missing_sections={}",
                        display_relative(workspace_root, &file).display(),
                        validation.missing_sections.join("|")
                    ));
                }
            }
            Err(err) => errors.push(format!(
                "schema_read_failed: {}: {err}",
                display_relative(workspace_root, &file).display()
            )),
        }
    }
}

fn validate_review_pack_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if file.file_name().and_then(|name| name.to_str()) == Some("review-manifest.json") {
            validate_json_file(workspace_root, &file, errors);
        }
    }
}

fn validate_decision_doc_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            validate_json_file(workspace_root, &file, errors);
        }
    }
}

fn validate_handoff_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if !file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        match fs::read_to_string(&file) {
            Ok(content) if content.trim().is_empty() => errors.push(format!(
                "schema_invalid: {} empty_handoff",
                display_relative(workspace_root, &file).display()
            )),
            Ok(_) => {}
            Err(err) => errors.push(format!(
                "schema_read_failed: {}: {err}",
                display_relative(workspace_root, &file).display()
            )),
        }
    }
}

fn validate_json_file(workspace_root: &Path, file: &Path, errors: &mut Vec<String>) {
    match fs::read_to_string(file) {
        Ok(content) => {
            if let Err(err) = serde_json::from_str::<serde_json::Value>(&content) {
                errors.push(format!(
                    "schema_invalid: {} json_parse_failed: {err}",
                    display_relative(workspace_root, file).display()
                ));
            }
        }
        Err(err) => errors.push(format!(
            "schema_read_failed: {}: {err}",
            display_relative(workspace_root, file).display()
        )),
    }
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            collect_files(&path, files)?;
        } else if entry.file_type()?.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn display_relative(workspace_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(workspace_root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
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

fn validate_artifact_subpath(path: &Path) -> Result<()> {
    validate_artifact_path(path)?;
    ensure!(
        path.components().count() > 1,
        "artifact path must include an artifact root"
    );
    Ok(())
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

fn artifact_access_permits(actual: ArtifactAccess, requested: ArtifactAccess) -> bool {
    match requested {
        ArtifactAccess::Read => matches!(actual, ArtifactAccess::Read | ArtifactAccess::ReadWrite),
        ArtifactAccess::Write => {
            matches!(actual, ArtifactAccess::Write | ArtifactAccess::ReadWrite)
        }
        ArtifactAccess::ReadWrite => actual == ArtifactAccess::ReadWrite,
    }
}

fn artifact_policy_error_blocks_writes(error: &str) -> bool {
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

fn artifact_lock_error_allows_refresh(error: &str) -> bool {
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

fn artifact_lock_warning_resolved_by_write(warning: &str) -> bool {
    warning.ends_with(".source_role_missing")
        || warning.ends_with(".source_kind_missing")
        || warning.ends_with(".source_path_missing")
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

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
