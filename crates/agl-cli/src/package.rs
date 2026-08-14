use std::path::PathBuf;

use agl_package::{
    PackageAdapter, PackageCandidate, PackageConfigEvidence, PackageEnvelope, PackageError,
    PackagePathRouter, PackageRef, PackageSourceDeclaration, PackageSourceId, PackageSourceKind,
    PackageSourceTier, PackageTreeDigest, ResolvedPackage, ResolvedPackageGraph,
    compute_package_digest,
};
use agl_runtime::{AgentLibreRuntimeConfig, PackageComposition};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::args::{
    PackageCommand, PackageLockCommandOptions, PackageReferenceOptions, PackageSourceCommand,
};

#[derive(Clone)]
struct PackageContext {
    composition: PackageComposition,
    workspace_root: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
struct PackageProjection {
    type_id: String,
    package_id: String,
    version: String,
    exact_reference: String,
    required_reference: Option<String>,
    source_tier: PackageSourceTier,
    source_kind: PackageSourceKind,
    source_id: String,
    source_revision: Option<String>,
    source_tree: Option<String>,
    package_tree_digest: Option<PackageTreeDigest>,
    dependencies: Vec<String>,
    config_layers: Vec<PackageConfigEvidence>,
    validation_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PackageDiagnostic>,
    lock_state: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PackageDiagnostic {
    code: &'static str,
    message: String,
    context: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct PackageErrorEnvelope {
    error: PackageDiagnostic,
}

#[derive(Clone, Debug, Serialize)]
struct PackageGraphProjection {
    root: String,
    nodes: Vec<PackageProjection>,
}

#[derive(Clone, Debug, Serialize)]
struct PackageLockProjection {
    path: PathBuf,
    root: String,
    refreshed: bool,
    lock_present: bool,
    package_count: usize,
}

#[derive(Clone, Debug, Serialize)]
struct PackageSourceProjection {
    id: String,
    tier: PackageSourceTier,
    kind: PackageSourceKind,
    root: Option<PathBuf>,
}

pub(crate) fn run_package(
    command: PackageCommand,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    match command {
        PackageCommand::List(options) => run_list(options.json, runtime),
        PackageCommand::Inspect(options) => run_reference("inspect", options, runtime),
        PackageCommand::Resolve(options) => run_reference("resolve", options, runtime),
        PackageCommand::Graph(options) => run_reference("graph", options, runtime),
        PackageCommand::Lock(options) => run_lock(options, runtime),
        PackageCommand::Source(command) => run_source(command, runtime),
    }
}

pub(crate) fn json_requested(command: &PackageCommand) -> bool {
    match command {
        PackageCommand::List(options) => options.json,
        PackageCommand::Inspect(options)
        | PackageCommand::Resolve(options)
        | PackageCommand::Graph(options) => options.json,
        PackageCommand::Lock(options) => options.json,
        PackageCommand::Source(PackageSourceCommand::List { json })
        | PackageCommand::Source(PackageSourceCommand::Add { json, .. })
        | PackageCommand::Source(PackageSourceCommand::Remove { json, .. }) => *json,
    }
}

pub(crate) fn print_error_json(error: &anyhow::Error) {
    let diagnostic = error
        .downcast_ref::<PackageError>()
        .map(package_diagnostic)
        .unwrap_or_else(|| PackageDiagnostic {
            code: "package_error",
            message: format!("{error:#}"),
            context: serde_json::json!({}),
        });
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&PackageErrorEnvelope { error: diagnostic })
            .expect("package error envelope is serializable")
    );
}

fn context(runtime: &AgentLibreRuntimeConfig) -> Result<PackageContext> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let composition = agl_runtime::compose_packages(
        &runtime.paths,
        agl_repo::package_composition_input(&workspace_root)?,
    )?;
    Ok(PackageContext {
        composition,
        workspace_root,
    })
}

fn run_list(json: bool, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let context = context(runtime)?;
    let lock = context.composition.lock.as_ref();
    let mut projections = Vec::new();
    let mut invalid_count = 0_usize;
    for source in &context.composition.sources {
        for adapter in context.composition.registry.iter() {
            for candidate in source.inventory_candidates(&adapter.descriptor().type_id)? {
                let validation = validate_list_candidate(adapter, &candidate);
                let (envelope, digest, validation_error) = match validation {
                    Ok((envelope, digest)) => (Some(envelope), Some(digest), None),
                    Err(error) => (None, None, Some(error)),
                };
                if validation_error.is_some() {
                    invalid_count += 1;
                }
                let key = format!(
                    "{}:{}@{}",
                    candidate.type_id, candidate.package_id, candidate.version
                );
                projections.push(PackageProjection {
                    type_id: candidate.type_id.to_string(),
                    package_id: candidate.package_id.to_string(),
                    version: candidate.version.to_string(),
                    exact_reference: key.clone(),
                    required_reference: envelope.as_ref().map(envelope_reference),
                    source_tier: candidate.tier,
                    source_kind: candidate.kind,
                    source_id: candidate.source_id.to_string(),
                    source_revision: candidate.source_revision.clone(),
                    source_tree: candidate.source_tree.clone(),
                    package_tree_digest: digest,
                    dependencies: envelope
                        .as_ref()
                        .map(|value| value.requires.iter().map(ToString::to_string).collect())
                        .unwrap_or_default(),
                    config_layers: context
                        .composition
                        .router
                        .config_layers(&candidate.type_id, &candidate.package_id)?,
                    validation_status: if validation_error.is_some() {
                        "invalid"
                    } else {
                        "package_validated"
                    },
                    error: validation_error.as_ref().map(package_diagnostic),
                    lock_state: lock
                        .and_then(|value| value.packages.iter().find(|item| item.key() == key))
                        .map(|_| "locked")
                        .unwrap_or("unlocked"),
                });
            }
        }
    }
    projections.sort_by(|left, right| {
        (
            &left.type_id,
            &left.package_id,
            &left.version,
            &left.source_tier,
        )
            .cmp(&(
                &right.type_id,
                &right.package_id,
                &right.version,
                &right.source_tier,
            ))
    });
    let output = if json {
        crate::print_json(&projections)
    } else {
        for package in projections {
            println!(
                "{} {:?} {} {} {}",
                package.exact_reference,
                package.source_tier,
                package.source_id,
                package
                    .package_tree_digest
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "-".to_owned()),
                package.lock_state
            );
        }
        Ok(())
    };
    output?;
    ensure!(
        invalid_count == 0,
        "package list contains {invalid_count} invalid candidate(s)"
    );
    Ok(())
}

fn validate_list_candidate(
    adapter: &dyn PackageAdapter,
    candidate: &PackageCandidate,
) -> std::result::Result<(PackageEnvelope, PackageTreeDigest), PackageError> {
    if let Some(error) = candidate.discovery_error() {
        return Err(error.clone());
    }
    let envelope = adapter.extract_envelope(candidate.view())?;
    envelope.validate()?;
    if envelope.type_id != candidate.type_id {
        return Err(PackageError::AdapterTypeMismatch {
            type_id: candidate.type_id.to_string(),
            actual_type: envelope.type_id.to_string(),
        });
    }
    if envelope.id != candidate.package_id {
        return Err(PackageError::AdapterPackageMismatch {
            type_id: candidate.type_id.to_string(),
            actual_id: envelope.id.to_string(),
        });
    }
    if envelope.version != candidate.version {
        return Err(PackageError::CandidateVersionMismatch {
            candidate: candidate.version.to_string(),
            envelope: envelope.version.to_string(),
        });
    }
    let digest = compute_package_digest(candidate.view())?;
    adapter.validate_payload(candidate.view(), &envelope)?;
    Ok((envelope, digest))
}

fn run_reference(
    operation: &'static str,
    options: PackageReferenceOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let context = context(runtime)?;
    let reference = PackageRef::parse(&options.reference)
        .with_context(|| format!("package.invalid_reference: {}", options.reference))?;
    let graph = context.composition.resolve(&reference)?;
    let projection = graph_projection(
        &graph,
        context.composition.lock.as_ref(),
        &context.composition.router,
    );
    if options.json {
        if operation == "inspect" {
            let root = projection
                .nodes
                .iter()
                .find(|node| node.exact_reference == projection.root)
                .context("resolved graph has no root")?;
            crate::print_json(root)
        } else {
            crate::print_json(&projection)
        }
    } else if operation == "inspect" {
        let root = projection
            .nodes
            .iter()
            .find(|node| node.exact_reference == projection.root)
            .context("resolved graph has no root")?;
        print_projection(root);
        Ok(())
    } else {
        println!("root={}", projection.root);
        for node in projection.nodes {
            print_projection(&node);
        }
        Ok(())
    }
}

fn run_lock(options: PackageLockCommandOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let context = context(runtime)?;
    let manifest_path = context.workspace_root.join(".agl/workspace.toml");
    let manifest = agl_repo::read_workspace_manifest(&manifest_path)?;
    let graph = if options.refresh {
        context
            .composition
            .resolve_for_lock_refresh(&manifest.default_function)?
    } else {
        context.composition.resolve(&manifest.default_function)?
    };
    let lock_path = context.workspace_root.join(".agl/package-lock.toml");
    if options.refresh {
        agl_repo::write_package_lock(
            &lock_path,
            &context
                .composition
                .workspace_lock(&manifest.default_function)?,
        )?;
    }
    let lock = agl_repo::read_optional_package_lock(&lock_path)?;
    let projection = PackageLockProjection {
        path: lock_path,
        root: graph.root.clone(),
        refreshed: options.refresh,
        lock_present: lock.is_some(),
        package_count: lock.as_ref().map(|lock| lock.packages.len()).unwrap_or(0),
    };
    if options.json {
        crate::print_json(&projection)
    } else {
        println!(
            "root={} lock={} packages={} refreshed={}",
            projection.root,
            projection.lock_present,
            projection.package_count,
            projection.refreshed
        );
        Ok(())
    }
}

fn run_source(command: PackageSourceCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let path = workspace_root.join(".agl/workspace.toml");
    let mut manifest = agl_repo::read_workspace_manifest(&path)?;
    match command {
        PackageSourceCommand::List { json } => {
            let sources = manifest
                .sources
                .iter()
                .map(|declaration| PackageSourceProjection {
                    id: declaration.id.to_string(),
                    tier: declaration.tier,
                    kind: declaration.kind,
                    root: declaration.path.clone(),
                })
                .collect::<Vec<_>>();
            if json {
                crate::print_json(&sources)
            } else {
                for source in sources {
                    println!(
                        "{} {:?} {:?} {:?}",
                        source.id, source.tier, source.kind, source.root
                    );
                }
                Ok(())
            }
        }
        PackageSourceCommand::Add {
            name,
            git,
            local,
            rev,
            json,
        } => {
            let source_id = PackageSourceId::new(name.clone())?;
            ensure!(
                !manifest.sources.iter().any(|source| source.id == source_id),
                "package.source_exists: {name}"
            );
            let declaration = if let Some(url) = git {
                PackageSourceDeclaration {
                    id: source_id.clone(),
                    tier: PackageSourceTier::Workspace,
                    kind: PackageSourceKind::Git,
                    path: None,
                    url: Some(url),
                    rev,
                }
            } else if let Some(path) = local {
                PackageSourceDeclaration {
                    id: source_id.clone(),
                    tier: PackageSourceTier::Workspace,
                    kind: PackageSourceKind::Directory,
                    path: Some(path),
                    url: None,
                    rev: None,
                }
            } else {
                bail!("package.source_kind_required: choose --git or --local")
            };
            declaration.validate()?;
            let source_kind = declaration.kind;
            let materialized_root = Some(agl_repo::materialize_package_source(
                &workspace_root,
                &declaration,
            )?);
            manifest.sources.push(declaration);
            manifest
                .sources
                .sort_by(|left, right| left.id.cmp(&right.id));
            agl_repo::write_workspace_manifest(&path, &manifest)?;
            let result = PackageSourceProjection {
                id: name,
                tier: PackageSourceTier::Workspace,
                kind: source_kind,
                root: materialized_root,
            };
            if json {
                crate::print_json(&result)
            } else {
                println!("source added: {}", result.id);
                Ok(())
            }
        }
        PackageSourceCommand::Remove { name, json } => {
            let before = manifest.sources.len();
            manifest.sources.retain(|source| source.id.as_str() != name);
            ensure!(
                before != manifest.sources.len(),
                "package.source_missing: {name}"
            );
            agl_repo::write_workspace_manifest(&path, &manifest)?;
            if json {
                crate::print_json(&serde_json::json!({"removed": name}))
            } else {
                println!("source removed: {name}");
                Ok(())
            }
        }
    }
}

fn graph_projection(
    graph: &ResolvedPackageGraph,
    lock: Option<&agl_package::PackageLock>,
    router: &PackagePathRouter,
) -> PackageGraphProjection {
    let nodes = graph
        .nodes
        .values()
        .map(|node| projection(node, lock, router))
        .collect();
    PackageGraphProjection {
        root: graph.root.clone(),
        nodes,
    }
}

fn projection(
    node: &ResolvedPackage,
    lock: Option<&agl_package::PackageLock>,
    router: &PackagePathRouter,
) -> PackageProjection {
    let key = node.key();
    PackageProjection {
        type_id: node.candidate.type_id.to_string(),
        package_id: node.candidate.package_id.to_string(),
        version: node.candidate.version.to_string(),
        exact_reference: key.clone(),
        required_reference: Some(envelope_reference(&node.envelope)),
        source_tier: node.candidate.tier,
        source_kind: node.candidate.kind,
        source_id: node.candidate.source_id.to_string(),
        source_revision: node.candidate.source_revision.clone(),
        source_tree: node.candidate.source_tree.clone(),
        package_tree_digest: Some(node.package_tree_digest.clone()),
        dependencies: node.dependencies.clone(),
        config_layers: router
            .config_layers(&node.candidate.type_id, &node.candidate.package_id)
            .unwrap_or_default(),
        validation_status: "package_validated",
        error: None,
        lock_state: lock
            .and_then(|lock| lock.packages.iter().find(|item| item.key() == key))
            .map(|_| "locked")
            .unwrap_or("unlocked"),
    }
}

fn print_projection(value: &PackageProjection) {
    println!(
        "{} source={} tier={:?} kind={:?} digest={} lock={}",
        value.exact_reference,
        value.source_id,
        value.source_tier,
        value.source_kind,
        value
            .package_tree_digest
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned()),
        value.lock_state
    );
    for dependency in &value.dependencies {
        println!("  -> {dependency}");
    }
}

fn envelope_reference(envelope: &agl_package::PackageEnvelope) -> String {
    format!("{}:{}@{}", envelope.type_id, envelope.id, envelope.version)
}

fn package_diagnostic(error: &PackageError) -> PackageDiagnostic {
    let context = match error {
        PackageError::InvalidReference { value, reason } => {
            serde_json::json!({"reference": value, "reason": reason})
        }
        PackageError::UnsupportedType { type_id } => serde_json::json!({"type": type_id}),
        PackageError::PackageNotFound {
            type_id,
            package_id,
        } => serde_json::json!({"type": type_id, "id": package_id}),
        PackageError::IncompatibleVersion {
            type_id,
            package_id,
            requirements,
            available,
        } => serde_json::json!({
            "type": type_id,
            "id": package_id,
            "constraints": requirements,
            "available": available,
        }),
        PackageError::AmbiguousCandidate {
            type_id,
            package_id,
            version,
            sources,
        } => serde_json::json!({
            "type": type_id,
            "id": package_id,
            "version": version,
            "sources": sources,
        }),
        PackageError::LockMissingPackage { key } => serde_json::json!({"package": key}),
        PackageError::LockDrift {
            key,
            field,
            expected,
            actual,
        } => serde_json::json!({
            "package": key,
            "field": field,
            "expected": expected,
            "actual": actual,
        }),
        PackageError::DependencyCycle { path } => serde_json::json!({"path": path}),
        PackageError::AdapterEnvelope { type_id, reason }
        | PackageError::AdapterPayload { type_id, reason } => {
            serde_json::json!({"type": type_id, "reason": reason})
        }
        PackageError::PathEscape { path } | PackageError::PackageSymlinkRejected { path } => {
            serde_json::json!({"path": path})
        }
        _ => serde_json::json!({}),
    };
    PackageDiagnostic {
        code: error.code(),
        message: error.to_string(),
        context,
    }
}
