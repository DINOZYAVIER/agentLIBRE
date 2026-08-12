use std::path::{Path, PathBuf};
use std::time::Duration;

use agl_config::{
    ModelId, load_model_bindings_or_empty, model_bindings_path, write_model_bindings,
};
use agl_model::{
    HostCapabilities, InstallRecordState, InstallSource, ModelArtifactRole, ModelBindingPatch,
    ModelCacheStatus, ModelCatalog, ModelDownloadRequest, ModelDownloader, ModelExecutionPlan,
    ModelInstallRecord, ModelInstallStore, ModelInstallTransaction, ModelInstallTransactionInput,
    ModelPackage, ModelPackageId, ModelProgressEvent, PlannedArtifactRole, SetupCheckpoint,
    SetupCheckpointStore, SetupPhase, setup_plan_hash, validate_gguf,
};
use agl_package::{PackageRef, WorkspaceConfigReferences, WorkspaceManifest, WorkspacePolicy};
use agl_repo::read_workspace_default_function;
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::args::SetupInitOptions;
use crate::doctor::{FunctionSmokeReport, FunctionSmokeRequest, run_function_smoke};
use crate::model::{confirm, human_bytes, render_progress};

const SETUP_PENDING_FUNCTION: &str = "setup-pending";

#[derive(Clone, Debug, Serialize)]
struct PackageChoiceReport {
    package_id: ModelPackageId,
    display_name: String,
    default: bool,
    total_bytes: u64,
    bytes_to_download: u64,
    cache: Vec<ModelCacheStatus>,
    compatible: bool,
    compatibility: String,
}

#[derive(Clone, Debug, Serialize)]
struct SetupPlanReport {
    version: u32,
    workspace_root: PathBuf,
    host: HostCapabilities,
    packages: Vec<PackageChoiceReport>,
    selected_package: ModelPackageId,
    repository: String,
    revision: String,
    runtime: SetupRuntimeReport,
    staged_bindings_path: PathBuf,
    published_bindings_path: PathBuf,
    binding_changes: Vec<SetupBindingChange>,
    target_default_function: String,
    default_function_change: bool,
    smoke_required: bool,
    offline: bool,
    plan_hash: String,
    resuming: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SetupBindingChange {
    model_id: ModelId,
    current_path: Option<PathBuf>,
    planned_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
struct SetupIntentFingerprint {
    version: u32,
    package_id: ModelPackageId,
    repository: String,
    revision: String,
    artifacts: Vec<SetupIntentArtifact>,
    host: HostCapabilities,
    runtime: SetupRuntimeReport,
    low_memory_consent: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SetupIntentArtifact {
    role: ModelArtifactRole,
    model_id: ModelId,
    files: Vec<SetupIntentArtifactFile>,
}

#[derive(Clone, Debug, Serialize)]
struct SetupIntentArtifactFile {
    filename: String,
    byte_size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct SetupRuntimeReport {
    profile_id: String,
    selected_device: Option<String>,
    context_tokens: u32,
    gpu_layers: u32,
    smoke_timeout_seconds: u64,
    expected_speed: String,
}

#[derive(Debug, Serialize)]
struct SetupReport {
    version: u32,
    state: &'static str,
    plan: SetupPlanReport,
    completed_phases: Vec<SetupPhase>,
    repository: Option<SetupWorkspaceReport>,
    smoke: Option<FunctionSmokeReport>,
    ready_command: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct SetupWorkspaceReport {
    workspace_root: PathBuf,
    manifest_path: PathBuf,
    created: bool,
}

#[derive(Debug, Serialize)]
struct SetupFailureReport {
    version: u32,
    state: &'static str,
    error: String,
    repair: String,
}

pub(crate) fn run_init(options: SetupInitOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let json = options.json;
    match run_init_inner(options, runtime) {
        Ok(report) => crate::print_json_or(json, &report, || print_ready_report(&report)),
        Err(error) => {
            if json {
                crate::print_json(&SetupFailureReport {
                    version: 1,
                    state: "failed",
                    error: format!("{error:#}"),
                    repair: repair_for_setup_error(&error),
                })?;
            }
            Err(error)
        }
    }
}

fn run_init_inner(
    mut options: SetupInitOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<SetupReport> {
    tracing::info!(target: "agentlibre::app", command = "init", "starting guided setup");
    options.offline |= agl_model::hugging_face_offline();
    let workspace_start = runtime.resolve_workspace_root(None)?;
    let workspace_root = agl_repo::resolve_repo_root(&workspace_start)?;
    let checkpoint_store = SetupCheckpointStore::new(runtime.paths.setup_state_root());
    let previous_checkpoint = checkpoint_store.load(&workspace_root)?;
    let catalog = ModelCatalog::from_builtin_resolved()?;
    let workspace_default = read_workspace_default_function(&workspace_root)?;
    let recommended_function = workspace_default
        .as_deref()
        .unwrap_or(agl_repo::DEFAULT_FUNCTION);
    let recommended_model = agl_runtime::compose_packages(&runtime.paths, &workspace_root)?
        .resolve_runtime_bundle(
            &workspace_root,
            &runtime.paths.config_dir,
            recommended_function,
            true,
            &[],
        )?
        .model
        .context("recommended Function has no Model dependency")?
        .package
        .id;
    let requested_package_id = select_package_id(
        options.model.as_deref(),
        previous_checkpoint.as_ref(),
        &catalog,
        &recommended_model,
    )?;
    let package = catalog
        .package(&requested_package_id)
        .with_context(|| format!("model package `{requested_package_id}` is not in the catalog"))?;

    let worker = ModelDownloader::spawn().context("failed to start model downloader")?;
    let handle = worker.handle();
    let package_cache = catalog
        .packages
        .iter()
        .map(|package| {
            handle
                .cache_status(ModelDownloadRequest::for_package(package, true))
                .map(|status| (package.id.clone(), status))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let host = agl_inference::project_host_capabilities(
        crate::daemon_first_inference_inventory(runtime)
            .context("failed to inspect daemon-first inference devices")?,
    )?;
    let effective_low_memory_consent = options.allow_low_memory
        || previous_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.low_memory_consent);
    let package_reports = catalog
        .packages
        .iter()
        .map(|candidate| {
            let cache = cache_for_package(&package_cache, &candidate.id);
            let bytes_to_download = missing_cache_bytes(cache);
            let compatibility = resolve_setup_execution_plan(
                candidate.id.as_str(),
                &workspace_root,
                runtime,
                &host,
            );
            PackageChoiceReport {
                package_id: candidate.id.clone(),
                display_name: candidate.display_name.clone(),
                default: candidate.id == recommended_model,
                total_bytes: candidate.total_required_bytes(),
                bytes_to_download,
                cache: cache.to_vec(),
                compatible: compatibility.is_ok(),
                compatibility: compatibility
                    .map(|plan| format!("exact profile {}", plan.profile_id()))
                    .unwrap_or_else(|error| error.to_string()),
            }
        })
        .collect::<Vec<_>>();
    let selected_choice = package_reports
        .iter()
        .find(|choice| choice.package_id == package.id)
        .expect("selected catalog package has a compatibility report");
    ensure!(
        selected_choice.compatible,
        "selected Model package is incompatible with this host: {}",
        selected_choice.compatibility
    );
    let selected_bytes_to_download = selected_choice.bytes_to_download;

    let execution_plan =
        resolve_setup_execution_plan(package.id.as_str(), &workspace_root, runtime, &host)?;
    let runtime_plan = setup_runtime_report(package, &execution_plan)?;
    let fingerprint = setup_fingerprint(
        package,
        &host,
        runtime_plan.clone(),
        effective_low_memory_consent,
    );
    let plan_hash = setup_plan_hash(&fingerprint)?;
    let staged_bindings_path = checkpoint_store.staged_bindings_path(&workspace_root)?;
    let published_bindings_path = model_bindings_path(&runtime.paths.config_dir);
    let published_bindings = load_model_bindings_or_empty(&published_bindings_path)?;
    let selected_cache = cache_for_package(&package_cache, &package.id);
    let binding_changes = package
        .required_artifacts()
        .filter_map(|artifact| {
            let current_path = published_bindings
                .models
                .get(&artifact.model_id)
                .map(|binding| binding.path.clone());
            let planned_path = selected_cache
                .iter()
                .find(|status| status.model_id == artifact.model_id && status.complete)
                .and_then(|status| status.cached_path.clone());
            (!matches!(
                (&current_path, &planned_path),
                (Some(current), Some(planned)) if current == planned
            ))
            .then(|| SetupBindingChange {
                model_id: artifact.model_id.clone(),
                current_path,
                planned_path,
            })
        })
        .collect::<Vec<_>>();
    let current_default = workspace_default;
    let default_function_change = current_default.as_deref() != Some(package.id.as_str());
    let same_checkpoint = previous_checkpoint.as_ref().is_some_and(|checkpoint| {
        checkpoint.package_id == package.id && checkpoint.plan_hash == plan_hash
    });
    let mut plan = SetupPlanReport {
        version: 2,
        workspace_root: workspace_root.clone(),
        host,
        packages: package_reports,
        selected_package: package.id.clone(),
        repository: package.repository.clone(),
        revision: package.revision.clone(),
        runtime: runtime_plan,
        staged_bindings_path: staged_bindings_path.clone(),
        published_bindings_path,
        binding_changes,
        target_default_function: package.id.as_str().to_owned(),
        default_function_change,
        smoke_required: true,
        offline: options.offline,
        plan_hash: plan_hash.clone(),
        resuming: same_checkpoint,
    };
    print_setup_plan(&plan, options.json)?;

    if options.dry_run {
        return Ok(SetupReport {
            version: 1,
            state: "planned",
            completed_phases: previous_checkpoint
                .map(|checkpoint| checkpoint.completed_phases)
                .unwrap_or_default(),
            plan,
            repository: None,
            smoke: None,
            ready_command: None,
        });
    }

    let needs_confirmation = !same_checkpoint
        && (selected_bytes_to_download > 0
            || !plan.binding_changes.is_empty()
            || plan.default_function_change
            || previous_checkpoint.is_some());
    if needs_confirmation {
        confirm(
            options.yes,
            options.non_interactive,
            "Continue with this complete setup plan? [y/N] ",
        )?;
    }

    let mut checkpoint = if same_checkpoint {
        previous_checkpoint.expect("same_checkpoint requires a checkpoint")
    } else {
        let mut checkpoint = SetupCheckpoint::new(
            &workspace_root,
            package.id.clone(),
            package
                .required_artifacts()
                .map(|artifact| PlannedArtifactRole {
                    role: artifact.role,
                    model_id: artifact.model_id.clone(),
                })
                .collect(),
            effective_low_memory_consent,
            plan_hash,
        )?;
        checkpoint.advance(SetupPhase::Confirmed)?;
        checkpoint_store.save(&checkpoint)?;
        checkpoint
    };

    let install_store = ModelInstallStore::new(runtime.paths.model_install_root());
    let records = match verified_package_records(package, &install_store)? {
        Some(records) => records,
        None => {
            if !options.json {
                println!("Acquiring and validating model artifacts...");
            }
            let result = handle
                .submit(ModelDownloadRequest::for_package(package, options.offline))?
                .wait_with_progress(|event| {
                    render_setup_progress(event, selected_bytes_to_download == 0)
                })?;
            result
                .artifacts
                .iter()
                .map(ModelInstallRecord::from_downloaded)
                .collect::<Result<Vec<_>>>()?
        }
    };
    advance_checkpoint(
        &checkpoint_store,
        &mut checkpoint,
        SetupPhase::ArtifactsReady,
    )?;

    let staged_default = current_default.as_deref().unwrap_or(SETUP_PENDING_FUNCTION);
    let repo_report = ensure_workspace_manifest(&workspace_root, staged_default)?;
    advance_checkpoint(
        &checkpoint_store,
        &mut checkpoint,
        SetupPhase::WorkspaceReady,
    )?;

    let patch = binding_patch_from_records(&records);
    let mut staged = load_model_bindings_or_empty(&plan.published_bindings_path)?;
    patch.merge_into(&mut staged, true)?;
    write_model_bindings(&staged_bindings_path, &staged)?;
    advance_checkpoint(
        &checkpoint_store,
        &mut checkpoint,
        SetupPhase::BindingsStaged,
    )?;

    commit_model_install(
        &install_store,
        &records,
        &plan.published_bindings_path,
        &staged_bindings_path,
    )?;

    let smoke = run_setup_smoke(
        runtime,
        package,
        &plan,
        &staged_bindings_path,
        &checkpoint_store,
        &mut checkpoint,
        &options,
    )?;
    advance_checkpoint(&checkpoint_store, &mut checkpoint, SetupPhase::SmokePassed)?;

    write_workspace_default(&workspace_root, package.id.as_str())
        .context("failed to commit workspace default after verified smoke")?;
    advance_checkpoint(&checkpoint_store, &mut checkpoint, SetupPhase::Committed)?;
    let completed_phases = checkpoint.completed_phases.clone();
    checkpoint_store.remove(&workspace_root)?;
    cleanup_staged_state(&staged_bindings_path);
    plan.resuming = false;

    Ok(SetupReport {
        version: 1,
        state: "ready",
        plan,
        completed_phases,
        repository: Some(repo_report),
        smoke: Some(smoke),
        ready_command: Some("agl"),
    })
}

fn select_package_id(
    explicit: Option<&str>,
    checkpoint: Option<&SetupCheckpoint>,
    catalog: &ModelCatalog,
    recommended_model: &ModelPackageId,
) -> Result<ModelPackageId> {
    if let Some(explicit) = explicit {
        let id = ModelPackageId::new(explicit)?;
        ensure!(
            catalog.package(&id).is_some(),
            "unknown model package `{id}`; choose {}",
            catalog
                .packages
                .iter()
                .map(|package| package.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(id);
    }
    Ok(checkpoint
        .map(|checkpoint| checkpoint.package_id.clone())
        .unwrap_or_else(|| recommended_model.clone()))
}

fn cache_for_package<'a>(
    cache: &'a [(ModelPackageId, Vec<ModelCacheStatus>)],
    package_id: &ModelPackageId,
) -> &'a [ModelCacheStatus] {
    cache
        .iter()
        .find(|(id, _)| id == package_id)
        .map(|(_, status)| status.as_slice())
        .expect("every catalog package was inspected")
}

fn missing_cache_bytes(cache: &[ModelCacheStatus]) -> u64 {
    cache
        .iter()
        .filter(|status| !status.complete)
        .map(|status| status.expected_byte_size)
        .sum()
}

fn resolve_setup_execution_plan(
    function_ref: &str,
    workspace_root: &Path,
    runtime: &AgentLibreRuntimeConfig,
    host: &HostCapabilities,
) -> Result<ModelExecutionPlan> {
    let composition = agl_runtime::compose_packages(&runtime.paths, workspace_root.to_path_buf())?;
    let bundle = composition.resolve_runtime_bundle(
        workspace_root,
        &runtime.paths.config_dir,
        function_ref,
        true,
        &[],
    )?;
    let (function, model) = bundle
        .model_execution_inputs(format!("sha256:{}", "0".repeat(64)))?
        .context("Function has no package-bound Model")?;
    Ok(agl_model::resolve_execution_plan(&function, &model, host)?)
}

fn setup_runtime_report(
    package: &ModelPackage,
    plan: &ModelExecutionPlan,
) -> Result<SetupRuntimeReport> {
    let profile = package
        .profiles
        .iter()
        .find(|profile| profile.id == plan.profile_id())
        .context("selected Model profile disappeared")?;
    Ok(SetupRuntimeReport {
        profile_id: profile.id.clone(),
        selected_device: plan.selected_device().map(|device| device.identity.clone()),
        context_tokens: profile.context_tokens,
        gpu_layers: profile.gpu_layers,
        smoke_timeout_seconds: profile.smoke_timeout_seconds,
        expected_speed: profile.expected_speed.clone(),
    })
}

fn setup_fingerprint(
    package: &ModelPackage,
    host: &HostCapabilities,
    runtime: SetupRuntimeReport,
    low_memory_consent: bool,
) -> SetupIntentFingerprint {
    SetupIntentFingerprint {
        version: 1,
        package_id: package.id.clone(),
        repository: package.repository.clone(),
        revision: package.revision.clone(),
        artifacts: package
            .required_artifacts()
            .map(|artifact| SetupIntentArtifact {
                role: artifact.role,
                model_id: artifact.model_id.clone(),
                files: artifact
                    .files
                    .iter()
                    .map(|file| SetupIntentArtifactFile {
                        filename: file.filename.clone(),
                        byte_size: file.byte_size,
                        sha256: file.sha256.clone(),
                    })
                    .collect(),
            })
            .collect(),
        host: host.clone(),
        runtime,
        low_memory_consent,
    }
}

fn verified_package_records(
    package: &ModelPackage,
    store: &ModelInstallStore,
) -> Result<Option<Vec<ModelInstallRecord>>> {
    let mut records = Vec::new();
    for artifact in package.required_artifacts() {
        let primary = artifact.primary_file();
        let Some(record) = store.get(&artifact.model_id)? else {
            return Ok(None);
        };
        let provenance_matches = record.package_id.as_ref() == Some(&package.id)
            && record.role == artifact.role
            && record.state == InstallRecordState::Active
            && matches!(
                &record.source,
                InstallSource::HuggingFace {
                    repository,
                    revision,
                    filename,
                } if repository == &package.repository
                    && revision == &package.revision
                    && filename == &primary.filename
            )
            && record.additional_files.len() + 1 == artifact.files.len()
            && record
                .additional_files
                .iter()
                .zip(artifact.files.iter().skip(1))
                .all(|(recorded, declared)| {
                    recorded.filename == declared.filename
                        && recorded.byte_size == declared.byte_size
                        && recorded.sha256 == declared.sha256
                });
        if !provenance_matches
            || validate_gguf(&record.path, Some(primary.byte_size), Some(&primary.sha256)).is_err()
            || record.additional_files.iter().any(|file| {
                validate_gguf(&file.path, Some(file.byte_size), Some(&file.sha256)).is_err()
            })
        {
            return Ok(None);
        }
        records.push(record);
    }
    Ok(Some(records))
}

fn binding_patch_from_records(records: &[ModelInstallRecord]) -> ModelBindingPatch {
    let mut patch = ModelBindingPatch::default();
    for record in records {
        patch.insert(record.model_id.clone(), record.path.clone());
    }
    patch
}

fn advance_checkpoint(
    store: &SetupCheckpointStore,
    checkpoint: &mut SetupCheckpoint,
    phase: SetupPhase,
) -> Result<()> {
    checkpoint.advance(phase)?;
    store.save(checkpoint)
}

fn run_setup_smoke(
    runtime: &AgentLibreRuntimeConfig,
    package: &ModelPackage,
    plan: &SetupPlanReport,
    _staged_bindings_path: &Path,
    _checkpoint_store: &SetupCheckpointStore,
    _checkpoint: &mut SetupCheckpoint,
    _options: &SetupInitOptions,
) -> Result<FunctionSmokeReport> {
    let request = FunctionSmokeRequest {
        reference: package.id.as_str().to_owned(),
        workspace_root: plan.workspace_root.clone(),
        timeout: Duration::from_secs(plan.runtime.smoke_timeout_seconds),
        max_output_tokens: 32,
    };
    run_function_smoke(runtime, request)
}

fn commit_model_install(
    install_store: &ModelInstallStore,
    records: &[ModelInstallRecord],
    published_bindings_path: &Path,
    staged_bindings_path: &Path,
) -> Result<()> {
    let staged = agl_config::load_model_bindings(staged_bindings_path)?;
    let patch = binding_patch_from_records(records);
    for (id, binding) in &patch.models {
        ensure!(
            staged.models.get(id) == Some(binding),
            "staged model binding `{id}` changed before commit"
        );
    }
    ModelInstallTransaction::new(install_store.clone(), published_bindings_path)?.commit(
        ModelInstallTransactionInput::new(records.to_vec(), patch, true),
    )?;
    Ok(())
}

fn ensure_workspace_manifest(
    workspace_root: &Path,
    default_function: &str,
) -> Result<SetupWorkspaceReport> {
    let manifest_path = workspace_root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
    let created = !manifest_path.exists();
    if created {
        let manifest = WorkspaceManifest {
            version: WorkspaceManifest::VERSION,
            default_function: PackageRef::parse(default_function)?,
            sources: Vec::new(),
            policy: WorkspacePolicy::default(),
            config: WorkspaceConfigReferences::default(),
        };
        agl_repo::write_workspace_manifest(&manifest_path, &manifest)?;
    }
    Ok(SetupWorkspaceReport {
        workspace_root: workspace_root.to_path_buf(),
        manifest_path,
        created,
    })
}

fn write_workspace_default(workspace_root: &Path, function_id: &str) -> Result<()> {
    let path = workspace_root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
    let mut manifest = agl_repo::read_workspace_manifest(&path)?;
    manifest.default_function = PackageRef::parse(function_id)?;
    agl_repo::write_workspace_manifest(path, &manifest)
}

fn cleanup_staged_state(staged_bindings_path: &Path) {
    let _ = std::fs::remove_file(staged_bindings_path);
    if let Some(parent) = staged_bindings_path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

fn print_setup_plan(plan: &SetupPlanReport, json: bool) -> Result<()> {
    if json {
        eprintln!("{}", serde_json::to_string_pretty(plan)?);
        return Ok(());
    }
    println!("agentLIBRE setup plan");
    println!("Workspace: {}", plan.workspace_root.display());
    println!(
        "Host memory: {} physical",
        human_bytes(plan.host.physical_host_bytes)
    );
    println!("Available model packages:");
    for package in &plan.packages {
        let selected = if package.package_id == plan.selected_package {
            " [selected]"
        } else {
            ""
        };
        println!(
            "  {}{} — {}; {}, download {}",
            package.package_id,
            selected,
            if package.compatible {
                "compatible"
            } else {
                "incompatible"
            },
            package.compatibility,
            human_bytes(package.bytes_to_download)
        );
    }
    println!(
        "Runtime: {} (gpu_layers={}, context={}, expected {})",
        plan.runtime.profile_id,
        plan.runtime.gpu_layers,
        plan.runtime.context_tokens,
        plan.runtime.expected_speed
    );
    if plan.binding_changes.is_empty() {
        println!("Bindings: already match the selected package");
    } else {
        println!("Binding changes:");
        for change in &plan.binding_changes {
            println!(
                "  {}: {} -> {}",
                change.model_id,
                change
                    .current_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "unbound".to_string()),
                change
                    .planned_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "downloaded validated cache path".to_string())
            );
        }
    }
    println!(
        "Workspace default: {}{}",
        plan.target_default_function,
        if plan.default_function_change {
            " [change]"
        } else {
            " [unchanged]"
        }
    );
    println!("Required artifacts:");
    let selected = plan
        .packages
        .iter()
        .find(|package| package.package_id == plan.selected_package)
        .expect("selected package report exists");
    for artifact in &selected.cache {
        println!(
            "  {:?} {} — {} [{}]",
            artifact.role,
            artifact.filename,
            human_bytes(artifact.expected_byte_size),
            if artifact.complete {
                "cached"
            } else {
                "download"
            }
        );
    }
    println!("A real bounded function smoke is required before commit.");
    if plan.resuming {
        println!("An existing confirmed setup will resume from its durable checkpoint.");
    }
    Ok(())
}

fn print_ready_report(report: &SetupReport) {
    if report.state == "planned" {
        println!("Dry run complete; no setup files or model bytes were changed.");
        return;
    }
    println!("Ready. The model bindings, default function, and smoke are verified.");
    if let Some(smoke) = &report.smoke {
        println!(
            "Smoke: {} ms, answer: {}",
            smoke.elapsed_ms,
            smoke.answer.trim()
        );
    }
    println!("Start with: agl");
}

fn render_setup_progress(event: ModelProgressEvent, cached_only: bool) {
    if !cached_only {
        render_progress(event);
        return;
    }
    match event {
        ModelProgressEvent::Started { total_files, .. } => {
            eprintln!("Validating {total_files} cached model file(s)...")
        }
        ModelProgressEvent::Complete { .. } => eprintln!("Cached validation complete."),
        other => render_progress(other),
    }
}

fn repair_for_setup_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    if message.contains("low_memory_not_recommended") {
        "agl init --allow-low-memory".to_string()
    } else if message.contains("offline cache miss") {
        "run agl init without --offline after configuring standard Hugging Face access".to_string()
    } else if message.contains("authentication") || message.contains("forbidden") {
        "accept repository access on huggingface.co and configure HF_TOKEN using standard Hugging Face settings"
            .to_string()
    } else if message.contains("workspace") {
        "agl repo init --force".to_string()
    } else {
        "fix the reported condition and run the same agl init command again; confirmed setup state is resumable"
            .to_string()
    }
}
