use std::path::PathBuf;
use std::sync::Arc;

use agl_app::{compose_artifacts, resolve_composed_artifacts};
use agl_artifact::{
    ArtifactAdapterRegistry, ArtifactConfigEvidence, ArtifactPackageRef, ArtifactPathRouter,
    ArtifactSource, ArtifactSourceDeclaration, ArtifactSourceId, ArtifactSourceKind,
    ArtifactSourceTier, PackageTreeDigest, ResolvedArtifact, ResolvedArtifactGraph,
    compute_package_digest,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::args::{
    ArtifactCommand, ArtifactLockCommandOptions, ArtifactReferenceOptions, ArtifactSourceCommand,
};

#[derive(Clone)]
struct ArtifactContext {
    registry: Arc<ArtifactAdapterRegistry>,
    sources: Vec<Arc<dyn ArtifactSource>>,
    workspace_root: PathBuf,
    router: ArtifactPathRouter,
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
    package_digest: PackageTreeDigest,
    dependencies: Vec<String>,
    config_layers: Vec<ArtifactConfigEvidence>,
    config_validation_status: &'static str,
    lock_state: &'static str,
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

fn context(runtime: &AgentLibreRuntimeConfig) -> Result<ArtifactContext> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let composition = compose_artifacts(&runtime.paths, &workspace_root)?;
    Ok(ArtifactContext {
        registry: composition.registry.clone(),
        sources: composition.sources,
        router: ArtifactPathRouter::new(
            workspace_root.clone(),
            runtime.paths.data_dir.clone(),
            runtime.paths.config_dir.clone(),
            runtime.paths.state_dir.clone(),
            runtime.paths.cache_dir.clone(),
            composition.registry,
        ),
        workspace_root,
    })
}

fn run_list(json: bool, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let context = context(runtime)?;
    let lock = read_lock(&context).ok();
    let mut projections = Vec::new();
    for source in &context.sources {
        for adapter in context.registry.iter() {
            for candidate in source.candidates(&adapter.descriptor().type_id)? {
                let envelope = adapter.extract_envelope(candidate.view())?;
                let digest = compute_package_digest(candidate.view())?;
                let key = format!(
                    "{}:{}@{}",
                    candidate.type_id, candidate.package_id, candidate.version
                );
                projections.push(ArtifactProjection {
                    type_id: candidate.type_id.to_string(),
                    package_id: candidate.package_id.to_string(),
                    version: candidate.version.to_string(),
                    exact_reference: key.clone(),
                    required_reference: Some(envelope_reference(&envelope)),
                    source_tier: candidate.tier,
                    source_kind: candidate.kind,
                    source_id: candidate.source_id.to_string(),
                    source_revision: None,
                    package_digest: digest,
                    dependencies: envelope.requires.iter().map(ToString::to_string).collect(),
                    config_layers: context
                        .router
                        .config_layers(&candidate.type_id, &candidate.package_id)?,
                    config_validation_status: "package_validated",
                    lock_state: lock
                        .as_ref()
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
    if json {
        crate::print_json(&projections)
    } else {
        for package in projections {
            println!(
                "{} {:?} {} {} {}",
                package.exact_reference,
                package.source_tier,
                package.source_id,
                package.package_digest,
                package.lock_state
            );
        }
        Ok(())
    }
}

fn run_reference(
    operation: &'static str,
    options: ArtifactReferenceOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let context = context(runtime)?;
    let reference = ArtifactPackageRef::parse(&options.reference)
        .with_context(|| format!("artifact.invalid_reference: {}", options.reference))?;
    let lock = read_lock(&context).ok();
    let graph = resolve_composed_artifacts(
        &runtime.paths,
        &context.workspace_root,
        &reference,
        lock.as_ref(),
    )?;
    let projection = graph_projection(&graph, lock.as_ref(), &context.router);
    if options.json {
        if operation == "inspect" {
            let root = projection
                .nodes
                .first()
                .context("resolved graph has no root")?;
            crate::print_json(root)
        } else {
            crate::print_json(&projection)
        }
    } else if operation == "inspect" {
        let root = projection
            .nodes
            .first()
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
    let existing_lock = read_lock(&context).ok();
    let graph = resolve_composed_artifacts(
        &runtime.paths,
        &context.workspace_root,
        &manifest.default_function,
        if options.refresh {
            None
        } else {
            existing_lock.as_ref()
        },
    )?;
    let lock_path = context.workspace_root.join(".agl/artifact-lock.toml");
    if options.refresh {
        let lock = graph.lock()?;
        agl_repo::write_artifact_lock_v2(&lock_path, &lock)?;
    }
    let lock = read_lock(&context).ok();
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
            let materialized_root = if source_kind == ArtifactSourceKind::Git {
                Some(agl_repo::materialize_artifact_source(
                    &workspace_root,
                    &name,
                    &declaration,
                )?)
            } else {
                None
            };
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

fn read_lock(context: &ArtifactContext) -> Result<agl_artifact::ArtifactLock> {
    agl_repo::read_artifact_lock_v2(context.workspace_root.join(".agl/artifact-lock.toml"))
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
        source_revision: None,
        package_digest: node.package_digest.clone(),
        dependencies: node.dependencies.clone(),
        config_layers: router
            .config_layers(&node.candidate.type_id, &node.candidate.package_id)
            .unwrap_or_default(),
        config_validation_status: "package_validated",
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
        value.package_digest,
        value.lock_state
    );
    for dependency in &value.dependencies {
        println!("  -> {dependency}");
    }
}

fn envelope_reference(envelope: &agl_artifact::ArtifactEnvelope) -> String {
    format!("{}:{}@{}", envelope.type_id, envelope.id, envelope.version)
}
