use std::path::PathBuf;

use agl_artifact::{
    ArtifactAdapter, ArtifactCandidate, ArtifactConfigEvidence, ArtifactEnvelope, ArtifactError,
    ArtifactPackageRef, ArtifactPathRouter, ArtifactSourceDeclaration, ArtifactSourceId,
    ArtifactSourceKind, ArtifactSourceTier, PackageTreeDigest, ResolvedArtifact,
    ResolvedArtifactGraph, compute_package_digest,
};
use agl_runtime::{AgentLibreRuntimeConfig, ArtifactComposition, compose_artifacts};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::args::{
    ArtifactCommand, ArtifactLockCommandOptions, ArtifactReferenceOptions, ArtifactSourceCommand,
};

#[derive(Clone)]
struct ArtifactContext {
    composition: ArtifactComposition,
    workspace_root: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
struct ArtifactProjection {
    type_id: String,
    package_id: String,
    version: String,
    exact_reference: String,
    required_reference: Option<String>,
    source_tier: ArtifactSourceTier,
    source_kind: ArtifactSourceKind,
    source_id: String,
    source_revision: Option<String>,
    source_tree: Option<String>,
    package_digest: Option<PackageTreeDigest>,
    dependencies: Vec<String>,
    config_layers: Vec<ArtifactConfigEvidence>,
    validation_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ArtifactDiagnostic>,
    lock_state: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ArtifactDiagnostic {
    code: &'static str,
    message: String,
    context: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ArtifactErrorEnvelope {
    error: ArtifactDiagnostic,
}

#[derive(Clone, Debug, Serialize)]
struct ArtifactGraphProjection {
    root: String,
    nodes: Vec<ArtifactProjection>,
}

#[derive(Clone, Debug, Serialize)]
struct ArtifactLockProjection {
    path: PathBuf,
    root: String,
    refreshed: bool,
    lock_present: bool,
    package_count: usize,
}

#[derive(Clone, Debug, Serialize)]
struct ArtifactSourceProjection {
    id: String,
    tier: ArtifactSourceTier,
    kind: ArtifactSourceKind,
    root: Option<PathBuf>,
}

pub(crate) fn run_artifact(
    command: ArtifactCommand,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    match command {
        ArtifactCommand::List(options) => run_list(options.json, runtime),
        ArtifactCommand::Inspect(options) => run_reference("inspect", options, runtime),
        ArtifactCommand::Resolve(options) => run_reference("resolve", options, runtime),
        ArtifactCommand::Graph(options) => run_reference("graph", options, runtime),
        ArtifactCommand::Lock(options) => run_lock(options, runtime),
        ArtifactCommand::Source(command) => run_source(command, runtime),
    }
}

pub(crate) fn json_requested(command: &ArtifactCommand) -> bool {
    match command {
        ArtifactCommand::List(options) => options.json,
        ArtifactCommand::Inspect(options)
        | ArtifactCommand::Resolve(options)
        | ArtifactCommand::Graph(options) => options.json,
        ArtifactCommand::Lock(options) => options.json,
        ArtifactCommand::Source(ArtifactSourceCommand::List { json })
        | ArtifactCommand::Source(ArtifactSourceCommand::Add { json, .. })
        | ArtifactCommand::Source(ArtifactSourceCommand::Remove { json, .. }) => *json,
    }
}

pub(crate) fn print_error_json(error: &anyhow::Error) {
    let diagnostic = error
        .downcast_ref::<ArtifactError>()
        .map(artifact_diagnostic)
        .unwrap_or_else(|| ArtifactDiagnostic {
            code: "artifact_error",
            message: format!("{error:#}"),
            context: serde_json::json!({}),
        });
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&ArtifactErrorEnvelope { error: diagnostic })
            .expect("artifact error envelope is serializable")
    );
}

fn context(runtime: &AgentLibreRuntimeConfig) -> Result<ArtifactContext> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let composition = compose_artifacts(&runtime.paths, &workspace_root)?;
    Ok(ArtifactContext {
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
            for candidate in source.candidates(&adapter.descriptor().type_id)? {
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
                projections.push(ArtifactProjection {
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
                    package_digest: digest,
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
                    error: validation_error.as_ref().map(artifact_diagnostic),
                    lock_state: lock
                        .and_then(|value| value.packages.get(&key))
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
                    .package_digest
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
        "artifact list contains {invalid_count} invalid candidate(s)"
    );
    Ok(())
}

fn validate_list_candidate(
    adapter: &dyn ArtifactAdapter,
    candidate: &ArtifactCandidate,
) -> std::result::Result<(ArtifactEnvelope, PackageTreeDigest), ArtifactError> {
    let envelope = adapter.extract_envelope(candidate.view())?;
    envelope.validate()?;
    if envelope.type_id != candidate.type_id {
        return Err(ArtifactError::AdapterTypeMismatch {
            type_id: candidate.type_id.to_string(),
            actual_type: envelope.type_id.to_string(),
        });
    }
    if envelope.id != candidate.package_id {
        return Err(ArtifactError::AdapterPackageMismatch {
            type_id: candidate.type_id.to_string(),
            actual_id: envelope.id.to_string(),
        });
    }
    if envelope.version != candidate.version {
        return Err(ArtifactError::CandidateVersionMismatch {
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
    options: ArtifactReferenceOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let context = context(runtime)?;
    let reference = ArtifactPackageRef::parse(&options.reference)
        .with_context(|| format!("artifact.invalid_reference: {}", options.reference))?;
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

fn run_lock(options: ArtifactLockCommandOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let context = context(runtime)?;
    let manifest_path = context.workspace_root.join(".agl/workspace.toml");
    let manifest = agl_repo::read_workspace_manifest_v2(&manifest_path)?;
    let graph = if options.refresh {
        context
            .composition
            .resolve_for_lock_refresh(&manifest.default_function)?
    } else {
        context.composition.resolve(&manifest.default_function)?
    };
    let lock_path = context.workspace_root.join(".agl/artifact-lock.toml");
    if options.refresh {
        agl_repo::replace_artifact_lock_packages_v2(&lock_path, graph.package_lock_entries()?)?;
    }
    let lock = agl_repo::read_optional_artifact_lock_v2(&lock_path)?;
    let projection = ArtifactLockProjection {
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

fn run_source(command: ArtifactSourceCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let path = workspace_root.join(".agl/workspace.toml");
    let mut manifest = agl_repo::read_workspace_manifest_v2(&path)?;
    match command {
        ArtifactSourceCommand::List { json } => {
            let sources = manifest
                .sources
                .iter()
                .map(|(id, declaration)| ArtifactSourceProjection {
                    id: id.clone(),
                    tier: ArtifactSourceTier::Workspace,
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
        ArtifactSourceCommand::Add {
            name,
            git,
            local,
            rev,
            json,
        } => {
            let source_id = ArtifactSourceId::new(name.clone())?;
            ensure!(
                !manifest.sources.contains_key(source_id.as_str()),
                "artifact.source_exists: {name}"
            );
            let declaration = if let Some(url) = git {
                ArtifactSourceDeclaration {
                    kind: ArtifactSourceKind::Git,
                    path: None,
                    url: Some(url),
                    rev,
                }
            } else if let Some(path) = local {
                ArtifactSourceDeclaration {
                    kind: ArtifactSourceKind::Directory,
                    path: Some(path),
                    url: None,
                    rev: None,
                }
            } else {
                bail!("artifact.source_kind_required: choose --git or --local")
            };
            declaration.validate()?;
            let source_kind = declaration.kind;
            let materialized_root = Some(agl_repo::materialize_artifact_source(
                &workspace_root,
                &name,
                &declaration,
            )?);
            manifest.sources.insert(name.clone(), declaration);
            agl_repo::write_workspace_manifest_v2(&path, &manifest)?;
            let result = ArtifactSourceProjection {
                id: name,
                tier: ArtifactSourceTier::Workspace,
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
        ArtifactSourceCommand::Remove { name, json } => {
            ensure!(
                manifest.sources.remove(&name).is_some(),
                "artifact.source_missing: {name}"
            );
            agl_repo::write_workspace_manifest_v2(&path, &manifest)?;
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
    graph: &ResolvedArtifactGraph,
    lock: Option<&agl_artifact::ArtifactLock>,
    router: &ArtifactPathRouter,
) -> ArtifactGraphProjection {
    let nodes = graph
        .nodes
        .values()
        .map(|node| projection(node, lock, router))
        .collect();
    ArtifactGraphProjection {
        root: graph.root.clone(),
        nodes,
    }
}

fn projection(
    node: &ResolvedArtifact,
    lock: Option<&agl_artifact::ArtifactLock>,
    router: &ArtifactPathRouter,
) -> ArtifactProjection {
    let key = node.key();
    ArtifactProjection {
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
        package_digest: Some(node.package_digest.clone()),
        dependencies: node.dependencies.clone(),
        config_layers: router
            .config_layers(&node.candidate.type_id, &node.candidate.package_id)
            .unwrap_or_default(),
        validation_status: "package_validated",
        error: None,
        lock_state: lock
            .and_then(|lock| lock.packages.get(&key))
            .map(|_| "locked")
            .unwrap_or("unlocked"),
    }
}

fn print_projection(value: &ArtifactProjection) {
    println!(
        "{} source={} tier={:?} kind={:?} digest={} lock={}",
        value.exact_reference,
        value.source_id,
        value.source_tier,
        value.source_kind,
        value
            .package_digest
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned()),
        value.lock_state
    );
    for dependency in &value.dependencies {
        println!("  -> {dependency}");
    }
}

fn envelope_reference(envelope: &agl_artifact::ArtifactEnvelope) -> String {
    format!("{}:{}@{}", envelope.type_id, envelope.id, envelope.version)
}

fn artifact_diagnostic(error: &ArtifactError) -> ArtifactDiagnostic {
    let context = match error {
        ArtifactError::InvalidReference { value, reason } => {
            serde_json::json!({"reference": value, "reason": reason})
        }
        ArtifactError::UnsupportedType { type_id } => serde_json::json!({"type": type_id}),
        ArtifactError::PackageNotFound {
            type_id,
            package_id,
        } => serde_json::json!({"type": type_id, "id": package_id}),
        ArtifactError::IncompatibleVersion {
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
        ArtifactError::AmbiguousCandidate {
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
        ArtifactError::LockMissingPackage { key } => serde_json::json!({"package": key}),
        ArtifactError::LockDrift {
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
        ArtifactError::DependencyCycle { path } => serde_json::json!({"path": path}),
        ArtifactError::AdapterPayload { type_id, reason } => {
            serde_json::json!({"type": type_id, "reason": reason})
        }
        ArtifactError::PathEscape { path } | ArtifactError::PackageSymlinkRejected { path } => {
            serde_json::json!({"path": path})
        }
        _ => serde_json::json!({}),
    };
    ArtifactDiagnostic {
        code: error.code(),
        message: error.to_string(),
        context,
    }
}
