use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use agl_client::ClientError;
use agl_config::{InferencePreset, bind_inference_preset, load_inference_preset_from_str};
use agl_function::{
    FUNCTION_FILE_NAME, FUNCTION_SYSTEM_PROMPT_FILE_NAME, FunctionListEntry, FunctionPackageSource,
    FunctionStatusReport, FunctionToolPolicy, LoadedFunction, function_status_from_loaded,
    load_function_candidate, workspace_functions_root,
};
use agl_model::{CatalogRuntimeProfile, ProfileDevice};
use agl_package::{
    PackageRef, PackageSourceDeclaration, PackageSourceId, PackageSourceKind, PackageSourceTier,
    PackageTypeId, WorkspaceConfigReferences, WorkspaceManifest, WorkspacePolicy,
};
use agl_protocol::RuntimeGenerationIdentity;
use agl_runtime::{AgentLibreRuntimeConfig, RuntimeBundleIdentity};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::args::{
    FunctionCommand, FunctionDoctorOptions, FunctionInitOptions, FunctionListOptions,
    FunctionShowOptions, FunctionStatusOptions,
};
use crate::doctor::{FunctionSmokeRequest, run_function_smoke};

const DEVELOPMENT_ASSETS_NOTE: &str = "repository development assets/functions is not a runtime source; materialize packages through the artifact source contract";

#[derive(Debug, Serialize)]
struct FunctionResolutionDiagnostics {
    target_workspace: PathBuf,
    runtime_source_contract: &'static str,
    client_runtime: agl_runtime::CurrentRuntimeIdentity,
    daemon_runtime: DaemonRuntimeDiagnostics,
    artifacts: RuntimeBundleIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference: Option<FunctionInferenceDiagnostics>,
}

#[derive(Debug, Serialize)]
struct DaemonRuntimeDiagnostics {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<RuntimeGenerationIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct FunctionInferenceDiagnostics {
    preset: InferencePreset,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_profile: Option<CatalogRuntimeProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_package_digest: Option<String>,
    model_bindings_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    binding: Option<FunctionModelBindingDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binding_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct FunctionModelBindingDiagnostics {
    model_id: String,
    model_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    multimodal_projector_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    multimodal_projector_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct FunctionShowReport {
    resolution: FunctionResolutionDiagnostics,
    function: LoadedFunction,
}

#[derive(Debug, Serialize)]
struct FunctionStatusOutput {
    resolution: FunctionResolutionDiagnostics,
    status: FunctionStatusReport,
}

#[derive(Debug, Serialize)]
struct FunctionDoctorOutput<T> {
    resolution: FunctionResolutionDiagnostics,
    doctor: T,
}

pub(crate) fn run_function(
    command: FunctionCommand,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    match command {
        FunctionCommand::List(options) => run_function_list(options, runtime),
        FunctionCommand::Show(options) => run_function_show(options, runtime),
        FunctionCommand::Status(options) => run_function_status(options, runtime),
        FunctionCommand::Init(options) => run_function_init(options, runtime),
        FunctionCommand::Doctor(options) => run_function_doctor(options, runtime),
    }
}

fn run_function_list(
    options: FunctionListOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let composition = agl_runtime::compose_packages(&runtime.paths, &workspace_root)?;
    let mut functions = Vec::new();
    for source in &composition.sources {
        for candidate in source.candidates(&PackageTypeId::function())? {
            let loaded = load_function_candidate(&candidate);
            functions.push(match loaded {
                Ok(function) => FunctionListEntry {
                    source: function.locator.source,
                    id: function.front_matter.id().to_owned(),
                    path: function.locator.path,
                    valid: true,
                    title: Some(function.front_matter.title),
                    error: None,
                },
                Err(error) => FunctionListEntry {
                    source: function_source(candidate.tier),
                    id: candidate.package_id.to_string(),
                    path: candidate
                        .package_root
                        .clone()
                        .unwrap_or_else(|| {
                            PathBuf::from(format!(
                                "artifact/function/{}@{}",
                                candidate.package_id, candidate.version
                            ))
                        })
                        .join(FUNCTION_FILE_NAME),
                    valid: false,
                    title: None,
                    error: Some(format!("{error:#}")),
                },
            });
        }
    }
    functions.sort_by(|left, right| {
        (&left.id, left.source.as_str(), &left.path).cmp(&(
            &right.id,
            right.source.as_str(),
            &right.path,
        ))
    });
    let report = FunctionListReport {
        workspace_root: workspace_root.clone(),
        workspace_functions_root: workspace_functions_root(&workspace_root),
        user_functions_root: runtime.paths.data_dir.join("functions"),
        functions,
    };

    crate::print_json_or(options.json, &report, || {
        println!("state=ok");
        println!("workspace_root={}", report.workspace_root.display());
        println!(
            "workspace_functions_root={}",
            report.workspace_functions_root.display()
        );
        println!(
            "user_functions_root={}",
            report.user_functions_root.display()
        );
        for function in &report.functions {
            print_function_list_entry(function);
        }
        if report.functions.is_empty() {
            println!("next_step=agl function init coding --workspace");
        }
    })
}

fn run_function_show(
    options: FunctionShowOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let (function, resolution) =
        resolve_function_diagnostics(runtime, &workspace_root, &options.reference)?;
    let report = FunctionShowReport {
        resolution,
        function,
    };

    crate::print_json_or(options.json, &report, || {
        print_function_resolution(&report.resolution);
        print_loaded_function(&report.function);
    })
}

fn run_function_status(
    options: FunctionStatusOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let (loaded, resolution) =
        resolve_function_diagnostics(runtime, &workspace_root, &options.reference)?;
    let status = function_status_from_loaded(
        &options.reference,
        loaded,
        &workspace_root,
        &runtime.paths.config_dir,
        None,
    );
    let report = FunctionStatusOutput { resolution, status };
    crate::print_json_or(options.json, &report, || {
        print_function_resolution(&report.resolution);
        print_function_status_report(&report.status);
    })?;
    if !report.status.errors.is_empty() {
        bail!("function status failed");
    }
    if options.strict && !report.status.warnings.is_empty() {
        bail!("function status has warnings");
    }
    Ok(())
}

fn resolve_function_diagnostics(
    runtime: &AgentLibreRuntimeConfig,
    workspace_root: &std::path::Path,
    reference: &str,
) -> Result<(LoadedFunction, FunctionResolutionDiagnostics)> {
    let composition = agl_runtime::compose_packages(&runtime.paths, workspace_root)?;
    let bundle = composition.resolve_runtime_bundle(
        workspace_root,
        &runtime.paths.config_dir,
        reference,
        false,
        &[],
    )?;
    let root = bundle
        .graph
        .nodes
        .get(&bundle.graph.root)
        .context("resolved Function graph has no root candidate")?;
    let function = load_function_candidate(&root.candidate)?;
    let inference = function_inference_diagnostics(runtime, &bundle)?;
    let resolution = FunctionResolutionDiagnostics {
        target_workspace: workspace_root.to_path_buf(),
        runtime_source_contract: DEVELOPMENT_ASSETS_NOTE,
        client_runtime: bundle.runtime.clone(),
        daemon_runtime: daemon_runtime_diagnostics(runtime),
        artifacts: bundle.identity(),
        inference,
    };
    Ok((function, resolution))
}

fn function_inference_diagnostics(
    runtime: &AgentLibreRuntimeConfig,
    bundle: &agl_runtime::ResolvedRuntimeBundle,
) -> Result<Option<FunctionInferenceDiagnostics>> {
    let Some(config) = bundle.function.inference_config_toml.as_deref() else {
        return Ok(None);
    };
    let preset = load_inference_preset_from_str("resolved Function inference.toml", config)?;
    let selected_profile = match (preset.runtime.auto_policy(), bundle.model.as_ref()) {
        (Some(policy), Some(model)) if policy.device.is_some() => {
            let matches = model
                .package
                .profiles
                .iter()
                .filter(|profile| profile.context_tokens == policy.max_context_tokens)
                .filter(|profile| profile.device == ProfileDevice::Gpu)
                .cloned()
                .collect::<Vec<_>>();
            ensure!(
                matches.len() == 1,
                "Function inference profile selection is ambiguous for exact context {} and device {:?}",
                policy.max_context_tokens,
                policy.device
            );
            matches.into_iter().next()
        }
        _ => None,
    };
    let model_reference = bundle.model.as_ref().map(|model| model.node_key.clone());
    let model_package_digest = bundle.model.as_ref().and_then(|model| {
        bundle
            .graph
            .nodes
            .get(&model.node_key)
            .map(|node| node.package_tree_digest.to_string())
    });
    let bindings_path = agl_config::model_bindings_path(&runtime.paths.config_dir);
    let (binding, binding_error) = match bind_inference_preset(preset.clone(), &bindings_path) {
        Ok(bound) => (
            Some(FunctionModelBindingDiagnostics {
                model_id: bound.backend.model_id.to_string(),
                model_path: bound.backend.model,
                multimodal_projector_id: bound
                    .backend
                    .multimodal_projector_id
                    .map(|id| id.to_string()),
                multimodal_projector_path: bound.backend.multimodal_projector,
            }),
            None,
        ),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    Ok(Some(FunctionInferenceDiagnostics {
        preset,
        selected_profile,
        model_reference,
        model_package_digest,
        model_bindings_path: bindings_path,
        binding,
        binding_error,
    }))
}

fn daemon_runtime_diagnostics(runtime: &AgentLibreRuntimeConfig) -> DaemonRuntimeDiagnostics {
    let socket_path = agl_daemon::default_socket_path(&runtime.paths);
    let async_runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return DaemonRuntimeDiagnostics {
                status: "error",
                runtime: None,
                error: Some(format!(
                    "failed to build daemon diagnostic runtime: {error}"
                )),
            };
        }
    };
    match async_runtime.block_on(crate::runtime::connect_daemon(&socket_path)) {
        Ok(client) => match client.hello() {
            Ok(hello) => DaemonRuntimeDiagnostics {
                status: "matched",
                runtime: Some(hello.daemon_runtime),
                error: None,
            },
            Err(error) => DaemonRuntimeDiagnostics {
                status: "error",
                runtime: None,
                error: Some(error.to_string()),
            },
        },
        Err(ClientError::RuntimeIdentityMismatch { daemon, .. }) => DaemonRuntimeDiagnostics {
            status: "mismatch",
            runtime: Some(*daemon),
            error: Some("first-party client and daemon runtime identities differ".to_owned()),
        },
        Err(ClientError::DaemonUnavailable(_)) => DaemonRuntimeDiagnostics {
            status: "unavailable",
            runtime: None,
            error: None,
        },
        Err(error) => DaemonRuntimeDiagnostics {
            status: "error",
            runtime: None,
            error: Some(error.to_string()),
        },
    }
}

pub(crate) fn resolve_loaded_function(
    runtime: &AgentLibreRuntimeConfig,
    workspace_root: &std::path::Path,
    reference: &str,
) -> Result<LoadedFunction> {
    let composition = agl_runtime::compose_packages(&runtime.paths, workspace_root)?;
    let graph = composition.resolve_function_reference(workspace_root, reference)?;
    let root = graph
        .nodes
        .get(&graph.root)
        .context("resolved Function graph has no root candidate")?;
    load_function_candidate(&root.candidate)
}

fn run_function_init(
    options: FunctionInitOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let (source, root, workspace_root) = if options.workspace {
        let workspace_root = runtime.resolve_workspace_root(None)?;
        (
            FunctionPackageSource::Workspace,
            workspace_functions_root(&workspace_root),
            Some(workspace_root),
        )
    } else {
        (
            FunctionPackageSource::Global,
            runtime.paths.data_dir.join("functions"),
            None,
        )
    };
    let function_dir = root.join(&options.id);
    let path = function_dir.join(FUNCTION_FILE_NAME);
    let system_prompt_path = function_dir.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME);
    let subagents_dir = function_dir.join("subagents");
    std::fs::create_dir_all(&subagents_dir).with_context(|| {
        format!(
            "failed to create function directory {}",
            subagents_dir.display()
        )
    })?;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("failed to create function {}", path.display()))?;
    file.write_all(function_template(&options.id, options.model_profile.as_deref()).as_bytes())
        .with_context(|| format!("failed to write function {}", path.display()))?;
    let mut system_prompt_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&system_prompt_path)
        .with_context(|| {
            format!(
                "failed to create function system prompt {}",
                system_prompt_path.display()
            )
        })?;
    system_prompt_file
        .write_all(function_system_prompt_template(&options.id).as_bytes())
        .with_context(|| {
            format!(
                "failed to write function system prompt {}",
                system_prompt_path.display()
            )
        })?;
    if let Some(workspace_root) = workspace_root {
        ensure_workspace_package_source(&workspace_root)?;
    }

    let report = FunctionInitReport {
        id: options.id,
        source: source.as_str().to_string(),
        path,
        system_prompt_path,
        subagents_dir,
        wrote: true,
        next_steps: vec![
            "agl function status <id>".to_string(),
            "start the daemon with the selected function, then run `agl`".to_string(),
        ],
    };
    crate::print_json_or(options.json, &report, || {
        println!("state=ok");
        println!("function.id={}", report.id);
        println!("function.source={}", report.source);
        println!("function.path={}", report.path.display());
        println!(
            "function.system_path={}",
            report.system_prompt_path.display()
        );
        println!("function.subagents_dir={}", report.subagents_dir.display());
        println!("wrote={}", report.wrote);
        for next_step in &report.next_steps {
            println!("next_step={next_step}");
        }
    })
}

fn ensure_workspace_package_source(workspace_root: &std::path::Path) -> Result<()> {
    let path = workspace_root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
    let mut manifest = if path.is_file() {
        agl_repo::read_workspace_manifest(&path)?
    } else {
        WorkspaceManifest {
            version: WorkspaceManifest::VERSION,
            default_function: PackageRef::parse(agl_repo::DEFAULT_FUNCTION)?,
            sources: Vec::new(),
            policy: WorkspacePolicy::default(),
            config: WorkspaceConfigReferences::default(),
        }
    };
    if manifest.sources.iter().any(|source| {
        source.tier == PackageSourceTier::Workspace
            && source.kind == PackageSourceKind::Directory
            && source.path.as_deref() == Some(std::path::Path::new(".agl"))
    }) {
        return Ok(());
    }
    ensure!(
        manifest
            .sources
            .iter()
            .all(|source| source.id.as_str() != "workspace"),
        "WorkspaceManifest source id `workspace` already names a different package source"
    );
    manifest.sources.push(PackageSourceDeclaration {
        id: PackageSourceId::new("workspace")?,
        tier: PackageSourceTier::Workspace,
        kind: PackageSourceKind::Directory,
        path: Some(PathBuf::from(".agl")),
        url: None,
        rev: None,
    });
    agl_repo::write_workspace_manifest(path, &manifest)
}

fn function_source(tier: PackageSourceTier) -> FunctionPackageSource {
    match tier {
        PackageSourceTier::Explicit => FunctionPackageSource::Explicit,
        PackageSourceTier::Workspace => FunctionPackageSource::Workspace,
        PackageSourceTier::Builtin => FunctionPackageSource::Builtin,
        PackageSourceTier::User | PackageSourceTier::System => FunctionPackageSource::Global,
    }
}

fn run_function_doctor(
    options: FunctionDoctorOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let (_, resolution) =
        resolve_function_diagnostics(runtime, &workspace_root, &options.reference)?;
    let timeout = doctor_timeout(&options.reference, &workspace_root, runtime)?;
    let report = run_function_smoke(
        runtime,
        FunctionSmokeRequest {
            reference: options.reference,
            workspace_root,
            bindings_path: None,
            runtime_plan_override: None,
            timeout,
            max_output_tokens: 32,
        },
    )?;
    let output = FunctionDoctorOutput {
        resolution,
        doctor: report,
    };
    crate::print_json_or(options.json, &output, || {
        print_function_resolution(&output.resolution);
        print_function_status_report(&output.doctor.static_status);
        println!("doctor.smoke_prompt={}", output.doctor.prompt);
        println!("doctor.answer={}", output.doctor.answer.trim());
        println!("doctor.elapsed_ms={}", output.doctor.elapsed_ms);
        println!("doctor.state=passed");
    })
}

fn doctor_timeout(
    reference: &str,
    workspace_root: &std::path::Path,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<Duration> {
    let composition = agl_runtime::compose_packages(&runtime.paths, workspace_root)?;
    let bundle = composition.resolve_runtime_bundle(
        workspace_root,
        &runtime.paths.config_dir,
        reference,
        true,
        &[],
    )?;
    let seconds = bundle
        .function
        .inference_config_toml
        .as_deref()
        .and_then(|toml| {
            agl_config::load_inference_preset_from_str("function doctor inference.toml", toml).ok()
        })
        .and_then(|preset| {
            bundle.model.as_ref().and_then(|model| {
                model
                    .package
                    .artifact(&preset.backend.model_id)
                    .is_some()
                    .then_some(&model.package)
                    .and_then(|package| {
                        package
                            .profiles
                            .iter()
                            .map(|profile| profile.smoke_timeout_seconds)
                            .max()
                    })
            })
        })
        .unwrap_or(300);
    Ok(Duration::from_secs(seconds))
}

fn print_function_list_entry(function: &FunctionListEntry) {
    println!(
        "function id={} source={} path={} valid={}",
        function.id,
        function.source.as_str(),
        function.path.display(),
        function.valid
    );
    if let Some(title) = &function.title {
        println!("function.{}.title={title}", function.id);
    }
    if let Some(error) = &function.error {
        println!("function.{}.error={error}", function.id);
    }
}

fn print_function_resolution(report: &FunctionResolutionDiagnostics) {
    println!(
        "runtime.workspace_root={}",
        report.target_workspace.display()
    );
    println!("runtime.source_contract={}", report.runtime_source_contract);
    println!("runtime.client.kind={:?}", report.client_runtime.kind);
    println!(
        "runtime.client.generation_id={}",
        report.client_runtime.generation_id
    );
    println!(
        "runtime.client.catalog_digest={}",
        report.client_runtime.builtin_catalog_digest
    );
    println!(
        "runtime.client.executable_digest={}",
        report.client_runtime.executable_digest
    );
    println!("runtime.daemon.status={}", report.daemon_runtime.status);
    if let Some(runtime) = &report.daemon_runtime.runtime {
        println!("runtime.daemon.kind={:?}", runtime.kind);
        println!("runtime.daemon.generation_id={}", runtime.generation_id);
        println!(
            "runtime.daemon.catalog_digest={}",
            runtime.builtin_catalog_digest
        );
        println!(
            "runtime.daemon.executable_digest={}",
            runtime.executable_digest
        );
    }
    if let Some(error) = &report.daemon_runtime.error {
        println!("runtime.daemon.error={error}");
    }
    println!("artifact.root={}", report.artifacts.root);
    println!("artifact.lock.state={:?}", report.artifacts.lock.state);
    if let Some(digest) = &report.artifacts.lock.sha256 {
        println!("artifact.lock.sha256={digest}");
    }
    for (key, node) in &report.artifacts.nodes {
        println!(
            "artifact.node key={} reference={} digest={} tier={:?} kind={:?} source={}",
            key,
            node.reference,
            node.package_tree_digest,
            node.source_tier,
            node.source_kind,
            node.source_id
        );
        if let Some(embedded) = &node.embedded_runtime {
            println!(
                "artifact.node.{}.runtime generation={} catalog={}",
                key, embedded.generation_id, embedded.builtin_catalog_digest
            );
        }
    }
    if let Some(inference) = &report.inference {
        println!("inference.model_id={}", inference.preset.backend.model_id);
        if let Some(reference) = &inference.model_reference {
            println!("inference.model_reference={reference}");
        }
        if let Some(digest) = &inference.model_package_digest {
            println!("inference.model_digest={digest}");
        }
        if let Some(policy) = inference.preset.runtime.auto_policy() {
            println!(
                "inference.requested_context_tokens={}",
                policy.max_context_tokens
            );
            println!("inference.requested_device={:?}", policy.device);
        }
        if let Some(profile) = &inference.selected_profile {
            println!("inference.profile_id={}", profile.id);
            println!("inference.context_tokens={}", profile.context_tokens);
            println!("inference.batch_size={}", profile.batch_size);
            println!("inference.ubatch_size={}", profile.ubatch_size);
            println!("inference.gpu_layers={}", profile.gpu_layers);
            println!(
                "inference.device_private_bytes={}",
                profile.device_private_bytes
            );
            if let Some(id) = &profile.pci_device_id {
                println!("inference.pci_device_id={id}");
            }
            if let Some(id) = &profile.pci_subsystem_id {
                println!("inference.pci_subsystem_id={id}");
            }
        }
        println!(
            "inference.model_bindings_path={}",
            inference.model_bindings_path.display()
        );
        if let Some(binding) = &inference.binding {
            println!("inference.model_path={}", binding.model_path.display());
            if let Some(path) = &binding.multimodal_projector_path {
                println!("inference.projector_path={}", path.display());
            }
        }
        if let Some(error) = &inference.binding_error {
            println!("inference.binding_error={error}");
        }
    }
}

fn print_loaded_function(function: &LoadedFunction) {
    println!("function.id={}", function.front_matter.id());
    println!("function.title={}", function.front_matter.title);
    println!("function.source={}", function.locator.source.as_str());
    println!("function.path={}", function.locator.path.display());
    if let Some(description) = &function.front_matter.description {
        println!("function.description={description}");
    }
    if let Some(profile) = function.front_matter.model_profile() {
        println!("function.model.profile={profile}");
    }
    if let Some(path) = &function.inference_config_path {
        println!("function.model.config_path={}", path.display());
    }
    if let Some(tool_mode) = function.front_matter.runtime_tool_mode() {
        println!("function.runtime.tool_mode={}", tool_mode.as_str());
    }
    if let Some(max_output_tokens) = function.front_matter.runtime_max_output_tokens() {
        println!("function.runtime.max_output_tokens={max_output_tokens}");
    }
    if let Some(max_tool_calls) = function.front_matter.runtime_max_tool_calls() {
        println!("function.runtime.max_tool_calls={max_tool_calls}");
    }
    let tool_policy = function.front_matter.tool_policy();
    print_function_tool_policy(tool_policy.as_ref());
    println!(
        "function.system_path={}",
        function.system_prompt_path.display()
    );
    for skill in function.front_matter.selected_skills() {
        println!("function.skill={skill}");
    }
    for subagent in &function.subagents {
        println!(
            "function.subagent id={} title={} path={}",
            subagent.front_matter.id,
            subagent.front_matter.title,
            subagent.path.display()
        );
    }
    println!("--- {} ---", FUNCTION_SYSTEM_PROMPT_FILE_NAME);
    println!("{}", function.system_prompt.trim());
    if let Some(config) = function
        .inference_config_toml
        .as_deref()
        .filter(|config| !config.trim().is_empty())
    {
        println!("--- inference.toml ---");
        println!("{}", config.trim());
    }
}

fn print_function_status_report(report: &FunctionStatusReport) {
    println!("state={}", report.state);
    println!("function.reference={}", report.reference);
    if let Some(source) = &report.source {
        println!("function.source={source}");
    }
    if let Some(path) = &report.path {
        println!("function.path={}", path.display());
    }
    if let Some(path) = &report.system_prompt_path {
        println!("function.system_path={}", path.display());
    }
    if let Some(id) = &report.id {
        println!("function.id={id}");
    }
    if let Some(title) = &report.title {
        println!("function.title={title}");
    }
    if let Some(profile) = &report.profile {
        println!("function.model.profile={profile}");
    }
    if let Some(profile_path) = &report.profile_path {
        println!("function.model.profile_path={}", profile_path.display());
    }
    if let Some(config_path) = &report.inference_config_path {
        println!("function.model.config_path={}", config_path.display());
        println!(
            "function.model.config_embedded={}",
            report.inference_config_embedded
        );
    }
    if let Some(model_path) = &report.inference_model_path {
        println!("function.model.path={}", model_path.display());
    }
    if let Some(model_id) = &report.inference_model_id {
        println!("function.model.id={model_id}");
    }
    if let Some(projector_id) = &report.inference_multimodal_projector_id {
        println!("function.model.multimodal_projector_id={projector_id}");
    }
    if let Some(projector_path) = &report.inference_multimodal_projector_path {
        println!(
            "function.model.multimodal_projector_path={}",
            projector_path.display()
        );
    }
    if let Some(draft_model_id) = &report.inference_draft_model_id {
        println!("function.model.draft_model_id={draft_model_id}");
    }
    if let Some(draft_model_path) = &report.inference_draft_model_path {
        println!(
            "function.model.draft_model_path={}",
            draft_model_path.display()
        );
    }
    if let Some(model_exists) = report.inference_model_exists {
        println!("function.model.exists={model_exists}");
    }
    if report.id.is_some() {
        print_function_tool_policy(report.tool_policy.as_ref());
    }
    for skill in &report.skills {
        println!("function.skill={skill}");
    }
    for subagent in &report.subagents {
        println!(
            "function.subagent id={} title={} description={}",
            subagent.id, subagent.title, subagent.description
        );
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

fn print_function_tool_policy(policy: Option<&FunctionToolPolicy>) {
    let Some(policy) = policy else {
        println!("function.tools.policy=inherit");
        return;
    };
    println!("function.tools.policy=explicit");
    println!(
        "function.tools.allow={}",
        policy
            .allow
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "function.tools.deny={}",
        policy
            .deny
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn function_template(id: &str, model_profile: Option<&str>) -> String {
    let title = title_from_id(id);
    let model_profile = model_profile.unwrap_or("local");
    format!(
        r#"---
package:
  schema: agentlibre.package/v1
  type: function
  id: {id}
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: {title}
model:
  profile: {model_profile}
runtime:
  tool_mode: read-only
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Summarize the current workspace and report visible tools."
---
"#
    )
}

fn function_system_prompt_template(id: &str) -> String {
    format!(
        r#"You are the `{id}` agentFUNCTION.

Inspect available agentLIBRE context before acting.
Keep changes small and explain repair steps when something is missing.
Use declared skills and subagents only when they are visible in the function context.
"#
    )
}

fn title_from_id(id: &str) -> String {
    let mut title = id.replace(['-', '_', '.'], " ");
    if let Some(first) = title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    title
}

#[derive(Serialize)]
struct FunctionListReport {
    workspace_root: PathBuf,
    workspace_functions_root: PathBuf,
    user_functions_root: PathBuf,
    functions: Vec<FunctionListEntry>,
}

#[derive(Serialize)]
struct FunctionInitReport {
    id: String,
    source: String,
    path: PathBuf,
    system_prompt_path: PathBuf,
    subagents_dir: PathBuf,
    wrote: bool,
    next_steps: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_gemma_functions_report_one_exact_profile_and_graph() {
        let root =
            std::env::temp_dir().join(format!("agl-function-diagnostics-{}", std::process::id()));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: agl_runtime::AgentLibrePaths::from_agl_home(root.join("home")),
            logging: agl_runtime::AgentLibreLoggingConfig::default(),
            history: agl_runtime::AgentLibreHistoryConfig::default(),
            workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let composition = agl_runtime::compose_packages(&runtime.paths, &workspace).unwrap();

        for (reference, context_tokens, profile_id, required_vram_bytes) in [
            (
                "gemma4-31b-32k",
                32_768,
                "gpu-rx7900xtx-32768",
                22_041_067_520_u64,
            ),
            (
                "gemma4-31b-64k",
                65_536,
                "gpu-rx7900xtx-65536",
                23_488_102_400_u64,
            ),
        ] {
            let bundle = composition
                .resolve_runtime_bundle(&workspace, &runtime.paths.config_dir, reference, true, &[])
                .unwrap();
            let diagnostic = function_inference_diagnostics(&runtime, &bundle)
                .unwrap()
                .unwrap();
            let profile = diagnostic.selected_profile.unwrap();
            assert_eq!(profile.id, profile_id);
            assert_eq!(profile.context_tokens, context_tokens);
            assert_eq!(profile.required_vram_bytes, required_vram_bytes);
            assert_eq!(profile.gpu_layers, 999);
            assert_eq!(profile.pci_device_id.as_deref(), Some("1002:744c"));
            assert_eq!(profile.pci_subsystem_id.as_deref(), Some("1da2:471e"));
            let identity = bundle.identity();
            assert_eq!(identity.root, format!("function:{reference}@1.2.0"));
            assert_eq!(identity.model.as_deref(), Some("model:gemma4-31b@1.2.0"));
            assert_eq!(identity.nodes.len(), 4);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
