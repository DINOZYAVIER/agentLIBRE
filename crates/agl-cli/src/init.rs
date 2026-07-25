use std::io::{self, IsTerminal as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agl_config::{
    InferencePresetRuntimeConfig, ModelId, load_inference_preset_from_str,
    load_model_bindings_or_empty, model_bindings_path, write_model_bindings,
};
use agl_inference::{InferenceDeviceInfo, InferenceDeviceKind};
use agl_models::{
    HostResources, InstallRecordState, InstallSource, LlamaDeviceInfo, LlamaDeviceKind,
    ModelArtifactRole, ModelBindingPatch, ModelCacheStatus, ModelCatalog, ModelDownloadRequest,
    ModelDownloadWorker, ModelFit, ModelFitKind, ModelInstallRecord, ModelInstallStore,
    ModelPackage, ModelPackageId, ModelProgressEvent, PlannedArtifactRole, RuntimePlan,
    RuntimePlanSet, RuntimePlanner, SetupCheckpoint, SetupCheckpointStore, SetupPhase,
    setup_plan_hash, validate_gguf,
};
use agl_repo::{
    RepoInitOptions, RepoInitReport, RepoStatusOptions, init_repo_workspace_with_default,
    read_workspace_default_function, status_repo_workspace, write_workspace_default_function,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail, ensure};
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
    fit: ModelFit,
}

#[derive(Clone, Debug, Serialize)]
struct SetupPlanReport {
    version: u32,
    workspace_root: PathBuf,
    host: HostResources,
    packages: Vec<PackageChoiceReport>,
    selected_package: ModelPackageId,
    repository: String,
    revision: String,
    runtime: RuntimePlanSet,
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
    nominal_memory_class_bytes: u64,
    physical_cores: usize,
    devices: Vec<SetupIntentDevice>,
    runtime: RuntimePlanSet,
    low_memory_consent: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SetupIntentArtifact {
    role: ModelArtifactRole,
    model_id: ModelId,
    filename: String,
    byte_size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct SetupIntentDevice {
    name: String,
    kind: LlamaDeviceKind,
    total_memory_bytes: u64,
    usable: bool,
    supports_gpu_offload: bool,
}

#[derive(Debug, Serialize)]
struct SetupReport {
    version: u32,
    state: &'static str,
    plan: SetupPlanReport,
    completed_phases: Vec<SetupPhase>,
    repository: Option<RepoInitReport>,
    smoke: Option<FunctionSmokeReport>,
    ready_command: Option<&'static str>,
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
    options.offline |= agl_models::hugging_face_offline();
    let workspace_start = runtime.resolve_workspace_root(None)?;
    let workspace_probe = status_repo_workspace(
        &workspace_start,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )?;
    let workspace_root = workspace_probe.workspace_root;
    let checkpoint_store = SetupCheckpointStore::new(runtime.paths.setup_state_root());
    let previous_checkpoint = checkpoint_store.load(&workspace_root)?;
    let catalog = ModelCatalog::builtin()?;
    let requested_package_id = select_package_id(
        options.model.as_deref(),
        previous_checkpoint.as_ref(),
        &catalog,
    )?;
    let package = catalog
        .package(&requested_package_id)
        .with_context(|| format!("model package `{requested_package_id}` is not in the catalog"))?;

    let worker = ModelDownloadWorker::spawn().context("failed to start model download worker")?;
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
    let devices = crate::daemon_first_inference_inventory(runtime)
        .context("failed to inspect daemon-first inference devices")?
        .into_iter()
        .map(model_device_info)
        .collect::<Vec<_>>();
    let host = HostResources::inspect(agl_models::hugging_face_cache_dir(), devices)?;
    let effective_low_memory_consent = options.allow_low_memory
        || previous_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.low_memory_consent);
    if host.below_recommended_floor() && !effective_low_memory_consent {
        bail!(
            "low_memory_not_recommended: Detected memory is below the recommended 8 GB minimum. Continuing may cause heavy swapping, an out-of-memory kill, or system stalls. To attempt best-effort setup: agl init --allow-low-memory"
        );
    }

    let planner = RuntimePlanner;
    let package_reports = catalog
        .packages
        .iter()
        .map(|candidate| {
            let cache = cache_for_package(&package_cache, &candidate.id);
            let bytes_to_download = missing_cache_bytes(cache);
            PackageChoiceReport {
                package_id: candidate.id.clone(),
                display_name: candidate.display_name.clone(),
                default: candidate.default,
                total_bytes: candidate.total_required_bytes(),
                bytes_to_download,
                cache: cache.to_vec(),
                fit: planner.fit(
                    candidate,
                    &host,
                    bytes_to_download,
                    effective_low_memory_consent,
                ),
            }
        })
        .collect::<Vec<_>>();
    let selected_choice = package_reports
        .iter()
        .find(|choice| choice.package_id == package.id)
        .expect("selected catalog package has a fit report");
    ensure_supported_fit(&selected_choice.fit, &host, package)?;
    let selected_bytes_to_download = selected_choice.bytes_to_download;

    let auto_policy = package_auto_policy(package, &workspace_root, runtime)?;
    let runtime_plans =
        planner.plan_set(package, &host, &auto_policy, effective_low_memory_consent)?;
    let fingerprint = setup_fingerprint(
        package,
        &host,
        runtime_plans.clone(),
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
    let current_default = read_workspace_default_function(&workspace_root)?;
    let default_function_change = current_default.as_deref() != Some(&package.function_id);
    let same_checkpoint = previous_checkpoint.as_ref().is_some_and(|checkpoint| {
        checkpoint.package_id == package.id && checkpoint.plan_hash == plan_hash
    });
    let mut plan = SetupPlanReport {
        version: 1,
        workspace_root: workspace_root.clone(),
        host,
        packages: package_reports,
        selected_package: package.id.clone(),
        repository: package.repository.clone(),
        revision: package.revision.clone(),
        runtime: runtime_plans,
        staged_bindings_path: staged_bindings_path.clone(),
        published_bindings_path,
        binding_changes,
        target_default_function: package.function_id.clone(),
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
        if (!io::stdin().is_terminal() || options.non_interactive)
            && options.yes
            && plan.runtime.cpu_fallback.is_some()
        {
            checkpoint.consent_to_cpu_fallback();
        }
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
    let repo_report = init_repo_workspace_with_default(
        &workspace_root,
        &RepoInitOptions::default(),
        staged_default,
    )?;
    let repo_status = status_repo_workspace(
        &workspace_root,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )?;
    ensure!(
        repo_status.errors.is_empty(),
        "workspace initialization is incomplete: {}; repair with `agl repo init --force`",
        repo_status.errors.join("; ")
    );
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

    commit_staged_setup(
        &workspace_root,
        &install_store,
        &records,
        &plan.published_bindings_path,
        &staged_bindings_path,
        &package.function_id,
    )?;
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
        .unwrap_or_else(|| catalog.default_package().id.clone()))
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

fn ensure_supported_fit(
    fit: &ModelFit,
    host: &HostResources,
    package: &ModelPackage,
) -> Result<()> {
    match fit.kind {
        ModelFitKind::Recommended | ModelFitKind::Fits | ModelFitKind::Slow => Ok(()),
        ModelFitKind::InsufficientMemory
            if !host.below_recommended_floor()
                && host.available_memory_bytes < host.detected_total_memory_bytes / 2 =>
        {
            bail!(
                "temporarily_low_available_memory: model `{}` cannot start safely now: {}. Close memory-heavy applications and run `agl init` again",
                package.id,
                fit.reason
            )
        }
        ModelFitKind::InsufficientDisk => bail!(
            "insufficient_disk: model `{}` cannot be acquired: {}",
            package.id,
            fit.reason
        ),
        ModelFitKind::UnsupportedBackend => bail!(
            "unsupported_backend: model `{}` is not offered because {}",
            package.id,
            fit.reason
        ),
        ModelFitKind::InsufficientMemory => bail!(
            "insufficient_memory: model `{}` cannot start safely: {}",
            package.id,
            fit.reason
        ),
    }
}

fn package_auto_policy(
    package: &ModelPackage,
    workspace_root: &Path,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<agl_config::AutoRuntimePolicy> {
    let function = agl_function::resolve_runtime_function(
        &package.function_id,
        workspace_root,
        &runtime.paths.config_dir,
    )?;
    let toml = function
        .inference_config_toml
        .context("catalog function has no embedded inference preset")?;
    let preset = load_inference_preset_from_str("catalog function inference.toml", &toml)?;
    let InferencePresetRuntimeConfig::Auto(policy) = preset.runtime else {
        bail!(
            "catalog function `{}` must use runtime mode = \"auto\"",
            package.function_id
        );
    };
    ensure!(
        preset.backend.model_id
            == package
                .required_artifacts()
                .find(|artifact| artifact.role == ModelArtifactRole::Main)
                .expect("validated package has a main artifact")
                .model_id,
        "catalog function main model does not match package"
    );
    Ok(policy)
}

fn setup_fingerprint(
    package: &ModelPackage,
    host: &HostResources,
    runtime: RuntimePlanSet,
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
                filename: artifact.filename.clone(),
                byte_size: artifact.byte_size,
                sha256: artifact.sha256.clone(),
            })
            .collect(),
        nominal_memory_class_bytes: host.nominal_memory_class_bytes,
        physical_cores: host.cpu.physical_cores,
        devices: host
            .devices
            .iter()
            .map(|device| SetupIntentDevice {
                name: device.name.clone(),
                kind: device.kind,
                total_memory_bytes: device.total_memory_bytes,
                usable: device.usable,
                supports_gpu_offload: device.supports_gpu_offload,
            })
            .collect(),
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
                    && filename == &artifact.filename
            )
            && record.additional_files.is_empty();
        if !provenance_matches
            || validate_gguf(
                &record.path,
                Some(artifact.byte_size),
                Some(&artifact.sha256),
            )
            .is_err()
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
    staged_bindings_path: &Path,
    checkpoint_store: &SetupCheckpointStore,
    checkpoint: &mut SetupCheckpoint,
    options: &SetupInitOptions,
) -> Result<FunctionSmokeReport> {
    let request = |runtime_plan: RuntimePlan| FunctionSmokeRequest {
        reference: package.function_id.clone(),
        workspace_root: plan.workspace_root.clone(),
        bindings_path: Some(staged_bindings_path.to_path_buf()),
        timeout: Duration::from_secs(runtime_plan.smoke_timeout_seconds),
        runtime_plan_override: Some(runtime_plan),
        max_output_tokens: 32,
    };
    match run_function_smoke(runtime, request(plan.runtime.selected.clone())) {
        Ok(report) => Ok(report),
        Err(gpu_error)
            if plan.runtime.selected.runtime.gpu_layers > 0
                && plan.runtime.cpu_fallback.is_some()
                && is_gpu_load_failure(&gpu_error) =>
        {
            let cpu_plan = plan
                .runtime
                .cpu_fallback
                .clone()
                .expect("guard checked CPU fallback");
            let offer = RuntimePlanner.cpu_fallback_offer(
                package,
                &plan.host,
                &package_auto_policy(package, &plan.workspace_root, runtime)?,
                checkpoint.low_memory_consent,
                format!("{gpu_error:#}"),
            )?;
            eprintln!("GPU setup failed: {}", offer.gpu_failure);
            eprintln!(
                "CPU fallback: context {}, expected speed {}, {}",
                offer.context_tokens, offer.expected_speed, offer.memory_fit
            );
            let consented = checkpoint.cpu_fallback_consent_plan_hash.as_deref()
                == Some(checkpoint.plan_hash.as_str());
            if !consented {
                confirm(
                    options.yes,
                    options.non_interactive,
                    "Retry this setup with the displayed CPU plan? [y/N] ",
                )?;
                checkpoint.consent_to_cpu_fallback();
                checkpoint_store.save(checkpoint)?;
            }
            run_function_smoke(runtime, request(cpu_plan))
                .context("CPU fallback smoke failed after explicit consent")
        }
        Err(error) => Err(error),
    }
}

fn is_gpu_load_failure(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("failed to load")
        || message.contains("explicit runtime plan is not a current")
        || ((message.contains("gpu") || message.contains("vulkan") || message.contains("cuda"))
            && (message.contains("memory") || message.contains("device")))
}

fn commit_staged_setup(
    workspace_root: &Path,
    install_store: &ModelInstallStore,
    records: &[ModelInstallRecord],
    published_bindings_path: &Path,
    staged_bindings_path: &Path,
    function_id: &str,
) -> Result<()> {
    let staged = agl_config::load_model_bindings(staged_bindings_path)?;
    let patch = binding_patch_from_records(records);
    for (id, binding) in &patch.models {
        ensure!(
            staged.models.get(id) == Some(binding),
            "staged model binding `{id}` changed before commit"
        );
    }
    let receipt =
        install_store.commit_with_bindings(records, &patch, published_bindings_path, true)?;
    if let Err(error) = write_workspace_default_function(workspace_root, function_id) {
        if let Err(rollback) = receipt.rollback() {
            return Err(anyhow::anyhow!(
                "failed to commit workspace default after staged smoke: {error:#}; model rollback also failed: {rollback:#}"
            ));
        }
        return Err(error).context("failed to commit workspace default after staged smoke");
    }
    Ok(())
}

fn cleanup_staged_state(staged_bindings_path: &Path) {
    let _ = std::fs::remove_file(staged_bindings_path);
    if let Some(parent) = staged_bindings_path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

fn model_device_info(device: InferenceDeviceInfo) -> LlamaDeviceInfo {
    LlamaDeviceInfo {
        name: device.backend_name,
        description: device.description,
        kind: match device.kind {
            InferenceDeviceKind::Cpu => LlamaDeviceKind::Cpu,
            InferenceDeviceKind::DiscreteGpu => LlamaDeviceKind::DiscreteGpu,
            InferenceDeviceKind::IntegratedGpu => LlamaDeviceKind::IntegratedGpu,
            InferenceDeviceKind::Accelerator => LlamaDeviceKind::Accelerator,
            InferenceDeviceKind::Metadata => LlamaDeviceKind::Metadata,
            InferenceDeviceKind::Unknown => LlamaDeviceKind::Unknown,
        },
        pci_device_id: device.pci_device_id,
        pci_subsystem_id: device.pci_subsystem_id,
        free_memory_bytes: device.free_memory_bytes,
        total_memory_bytes: device.total_memory_bytes,
        usable: device.usable,
        supports_gpu_offload: device.supports_gpu_offload,
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
        "Memory: {} total, {} currently available",
        human_bytes(plan.host.detected_total_memory_bytes),
        human_bytes(plan.host.available_memory_bytes)
    );
    println!("Available model packages:");
    for package in &plan.packages {
        let selected = if package.package_id == plan.selected_package {
            " [selected]"
        } else {
            ""
        };
        println!(
            "  {}{} — {:?}; {}, download {}",
            package.package_id,
            selected,
            package.fit.kind,
            package.fit.reason,
            human_bytes(package.bytes_to_download)
        );
    }
    println!(
        "Runtime: {} (gpu_layers={}, context={}, expected {})",
        plan.runtime.selected.profile_id,
        plan.runtime.selected.runtime.gpu_layers,
        plan.runtime.selected.runtime.context_tokens,
        plan.runtime.selected.expected_speed
    );
    if let Some(cpu) = &plan.runtime.cpu_fallback {
        println!(
            "Eligible CPU fallback: {} (context={}, expected {})",
            cpu.profile_id, cpu.runtime.context_tokens, cpu.expected_speed
        );
    }
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
