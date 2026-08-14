use std::collections::BTreeSet;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use agl_config::{ModelId, load_model_bindings_or_empty, model_bindings_path};
use agl_daemon::default_socket_path;
use agl_model::{
    ArtifactDownloadSpec, ArtifactFileDownloadSpec, HfSource, HfSourceKind, HubFileCandidate,
    InstallSource, ModelArtifactRole, ModelBindingPatch, ModelDownloadRequest, ModelDownloader,
    ModelInspector, ModelInstallRecord, ModelInstallStore, ModelInstallTransaction,
    ModelInstallTransactionInput, ModelLifecyclePlan, ModelLifecycleService, ModelProgressEvent,
    SetupCheckpointStore, derive_hf_model_id, import_local_model,
};
use agl_protocol::{ModelUnloadOutcome, ModelUnloadRequest};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::args::{
    ModelCommand, ModelImportOptions, ModelListOptions, ModelMutationOptions, ModelPruneOptions,
    ModelPullOptions, ModelStatusOptions, ModelUnloadOptions,
};

#[derive(Clone, Debug, Serialize)]
struct PullArtifactPlan {
    model_id: ModelId,
    role: ModelArtifactRole,
    repository: String,
    revision: String,
    filename: String,
    byte_size: u64,
    sha256: Option<String>,
    cached_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct SelectedArtifact {
    model_id: ModelId,
    role: ModelArtifactRole,
    files: Vec<HubFileCandidate>,
}

impl SelectedArtifact {
    fn primary(&self) -> &HubFileCandidate {
        &self.files[0]
    }
}

#[derive(Clone, Debug, Serialize)]
struct ModelPullPlan {
    version: u32,
    operation: &'static str,
    artifacts: Vec<PullArtifactPlan>,
    bytes_to_download: u64,
    total_bytes: u64,
    bindings_path: PathBuf,
    binding_changes: Vec<ModelId>,
    install_record_changes: Vec<ModelId>,
    replace: bool,
    offline: bool,
}

#[derive(Debug, Serialize)]
struct ModelPullReport {
    version: u32,
    state: &'static str,
    plan: ModelPullPlan,
    install_records: Vec<ModelInstallRecord>,
}

#[derive(Debug, Serialize)]
struct ModelImportReport {
    version: u32,
    state: &'static str,
    install_records: Vec<ModelInstallRecord>,
    bindings_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ModelLifecycleReport {
    version: u32,
    state: &'static str,
    plan: ModelLifecyclePlan,
}

#[derive(Clone, Debug, Default)]
struct PullPreflight {
    binding_changes: Vec<ModelId>,
    install_record_changes: Vec<ModelId>,
}

pub(crate) fn run_model(command: ModelCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        ModelCommand::Pull(options) => run_pull(options, runtime),
        ModelCommand::Import(options) => run_import(options, runtime),
        ModelCommand::List(options) => run_list(options, runtime),
        ModelCommand::Status(options) => run_status(options, runtime),
        ModelCommand::Verify(options) => run_verify(options, runtime),
        ModelCommand::Unbind(options) => run_unbind(options, runtime),
        ModelCommand::Remove(options) => run_remove(options, runtime),
        ModelCommand::Prune(options) => run_prune(options, runtime),
        ModelCommand::Unload(options) => run_unload(options, runtime),
    }
}

fn run_unload(options: ModelUnloadOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let socket_path = default_socket_path(&runtime.paths);
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build model unload runtime")?;
    let client = async_runtime
        .block_on(crate::runtime::connect_daemon(&socket_path))
        .with_context(|| {
            format!(
                "agentLIBRE daemon is unavailable at {}; start the user daemon before unloading resident models",
                socket_path.display()
            )
        })?;
    let event = async_runtime
        .block_on(client.model_unload(ModelUnloadRequest {
            target: options.target,
        }))
        .context("model unload request failed")?;
    println!(
        "outcome={}",
        match event.outcome {
            ModelUnloadOutcome::Released => "released",
            ModelUnloadOutcome::NotResident => "not_resident",
        }
    );
    println!("matched_models={}", event.matched_models);
    println!("released_models={}", event.released_models);
    println!("released_contexts={}", event.released_contexts);
    Ok(())
}

fn run_pull(mut options: ModelPullOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    options.offline |= agl_model::hugging_face_offline();
    let worker = ModelDownloader::spawn().context("failed to start model download worker")?;
    let handle = worker.handle();
    let main_source = HfSource::parse(&options.source).context("invalid Hugging Face model URL")?;
    let main_files = choose_candidate(
        &handle.inspect(main_source.clone(), options.offline)?,
        main_source.kind,
        options.non_interactive,
        "model",
    )?;
    let main_id = options
        .id
        .as_deref()
        .map(ModelId::new)
        .transpose()?
        .unwrap_or(derive_hf_model_id(
            &main_files[0].repository,
            &main_files[0].filename,
        )?);

    let mut selected = vec![SelectedArtifact {
        model_id: main_id.clone(),
        role: ModelArtifactRole::Main,
        files: main_files,
    }];
    if let Some(mmproj) = &options.mmproj {
        let source = HfSource::parse(mmproj).context("invalid Hugging Face projector URL")?;
        ensure!(
            source.kind == HfSourceKind::File,
            "--mmproj requires an exact Hugging Face blob or resolve GGUF URL"
        );
        let files = choose_candidate(
            &handle.inspect(source, options.offline)?,
            HfSourceKind::File,
            true,
            "projector",
        )?;
        selected.push(SelectedArtifact {
            model_id: ModelId::new(format!("{main_id}-mmproj"))?,
            role: ModelArtifactRole::Projector,
            files,
        });
    }

    ensure_distinct_ids(&selected)?;
    let bindings_path = model_bindings_path(&runtime.paths.config_dir);
    let store = ModelInstallStore::new(runtime.paths.model_install_root());
    let preflight = preflight_pull(&selected, &store, &bindings_path, options.replace)?;
    let plan = pull_plan(&selected, &bindings_path, &options, preflight);
    ensure_download_disk_space(&plan)?;
    print_pull_plan(&plan, options.json)?;
    if options.dry_run {
        return print_pull_report(
            options.json,
            ModelPullReport {
                version: 1,
                state: "planned",
                plan,
                install_records: Vec::new(),
            },
        );
    }
    if plan.bytes_to_download > 0
        || !plan.binding_changes.is_empty()
        || !plan.install_record_changes.is_empty()
    {
        confirm(
            options.yes,
            options.non_interactive,
            "Download and bind this model plan? [y/N] ",
        )?;
    }

    let request = ModelDownloadRequest {
        artifacts: selected
            .iter()
            .map(|artifact| {
                let candidate = artifact.primary();
                ArtifactDownloadSpec {
                    package_id: None,
                    model_id: artifact.model_id.clone(),
                    role: artifact.role,
                    repository: candidate.repository.clone(),
                    revision: candidate.revision.clone(),
                    filename: candidate.filename.clone(),
                    byte_size: candidate.byte_size,
                    sha256: candidate.sha256.clone().unwrap_or_default(),
                    additional_files: artifact
                        .files
                        .iter()
                        .skip(1)
                        .map(|file| ArtifactFileDownloadSpec {
                            filename: file.filename.clone(),
                            byte_size: file.byte_size,
                            sha256: file.sha256.clone().unwrap_or_default(),
                        })
                        .collect(),
                }
            })
            .collect(),
        offline: options.offline,
    };
    let result = handle
        .submit(request)?
        .wait_with_progress(|event| render_pull_progress(event, plan.bytes_to_download == 0))?;
    let records = result
        .artifacts
        .iter()
        .map(ModelInstallRecord::from_downloaded)
        .collect::<Result<Vec<_>>>()?;
    ModelInstallTransaction::new(store, &bindings_path)?
        .commit(ModelInstallTransactionInput::new(
            records.clone(),
            result.binding_patch,
            options.replace,
        ))
        .context("failed to commit model bindings")?;
    print_pull_report(
        options.json,
        ModelPullReport {
            version: 1,
            state: "ready",
            plan,
            install_records: records,
        },
    )
}

fn run_import(options: ModelImportOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let main_id = options.id.as_deref().map(ModelId::new).transpose()?;
    let main = import_local_model(&options.path, main_id, ModelArtifactRole::Main)?;
    let mut imported = vec![main];
    if let Some(projector_path) = &options.mmproj {
        let projector_id = ModelId::new(format!("{}-mmproj", imported[0].record.model_id))?;
        imported.push(import_local_model(
            projector_path,
            Some(projector_id),
            ModelArtifactRole::Projector,
        )?);
    }
    let mut patch = ModelBindingPatch::default();
    for item in &imported {
        patch.models.extend(item.binding_patch.models.clone());
    }
    let bindings_path = model_bindings_path(&runtime.paths.config_dir);
    let mut prospective = load_model_bindings_or_empty(&bindings_path)?;
    patch.merge_into(&mut prospective, options.replace)?;
    let store = ModelInstallStore::new(runtime.paths.model_install_root());
    preflight_import_records(&imported, &store, options.replace)?;
    let records = imported
        .iter()
        .map(|item| item.record.clone())
        .collect::<Vec<_>>();
    ModelInstallTransaction::new(store, &bindings_path)?.commit(
        ModelInstallTransactionInput::new(records, patch, options.replace),
    )?;
    let report = ModelImportReport {
        version: 1,
        state: "ready",
        install_records: imported.into_iter().map(|item| item.record).collect(),
        bindings_path,
    };
    crate::print_json_or(options.json, &report, || {
        println!(
            "Imported and bound {} model artifact(s).",
            report.install_records.len()
        );
        for record in &report.install_records {
            println!("  {} -> {}", record.model_id, record.path.display());
        }
    })
}

fn run_list(options: ModelListOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let reports = inspector(runtime).list()?;
    crate::print_json_or(options.json, &reports, || {
        if reports.is_empty() {
            println!("No explicit model bindings or agentLIBRE install records.");
            return;
        }
        for report in &reports {
            let state = if report.healthy { "ready" } else { "attention" };
            let path = report
                .binding_path
                .as_deref()
                .or_else(|| {
                    report
                        .install_record
                        .as_ref()
                        .map(|record| record.path.as_path())
                })
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_string());
            println!("{}  {}  {}", report.model_id, state, path);
        }
    })
}

fn run_status(options: ModelStatusOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let id = ModelId::new(options.model_id)?;
    let report = inspector(runtime).status(&id)?;
    crate::print_json_or(options.json, &report, || {
        println!("Model: {}", report.model_id);
        println!(
            "Binding: {}",
            report
                .binding_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "not bound".to_string())
        );
        println!(
            "Install record: {}",
            if report.install_record.is_some() {
                "present"
            } else {
                "absent"
            }
        );
        println!(
            "State: {}",
            if report.healthy { "ready" } else { "attention" }
        );
        for problem in &report.problems {
            println!("  - {problem}");
        }
    })
}

fn run_verify(options: ModelStatusOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let id = ModelId::new(options.model_id)?;
    let report = inspector(runtime).verify(&id)?;
    crate::print_json_or(options.json, &report, || {
        println!("Verified {}.", report.status.model_id);
        if let (Some(size), Some(digest)) = (report.byte_size, &report.sha256) {
            println!("  Size: {}", human_bytes(size));
            println!("  SHA-256: {digest}");
        }
    })?;
    ensure!(
        report.verified,
        "model `{id}` has inconsistent binding or install metadata"
    );
    Ok(())
}

fn run_unbind(options: ModelMutationOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let id = ModelId::new(options.model_id)?;
    let service = lifecycle_service(runtime)?;
    let plan = service.plan_unbind(&id)?;
    run_lifecycle_plan(plan, options.yes, options.dry_run, options.json, |plan| {
        service.execute_unbind(plan)
    })
}

fn run_remove(options: ModelMutationOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let id = ModelId::new(options.model_id)?;
    let service = lifecycle_service(runtime)?;
    let plan = service.plan_remove(&id)?;
    run_lifecycle_plan(plan, options.yes, options.dry_run, options.json, |plan| {
        service.execute_remove(plan)
    })
}

fn run_prune(options: ModelPruneOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let service = lifecycle_service(runtime)?;
    let plan = service.plan_prune()?;
    run_lifecycle_plan(plan, options.yes, options.dry_run, options.json, |plan| {
        service.execute_prune(plan)
    })
}

fn pull_plan(
    selected: &[SelectedArtifact],
    bindings_path: &Path,
    options: &ModelPullOptions,
    preflight: PullPreflight,
) -> ModelPullPlan {
    let artifacts = selected
        .iter()
        .flat_map(|artifact| {
            artifact.files.iter().map(|candidate| PullArtifactPlan {
                model_id: artifact.model_id.clone(),
                role: artifact.role,
                repository: candidate.repository.clone(),
                revision: candidate.revision.clone(),
                filename: candidate.filename.clone(),
                byte_size: candidate.byte_size,
                sha256: candidate.sha256.clone(),
                cached_path: candidate.cached_path.clone(),
            })
        })
        .collect::<Vec<_>>();
    let bytes_to_download = artifacts
        .iter()
        .filter(|artifact| artifact.cached_path.is_none())
        .map(|artifact| artifact.byte_size)
        .sum();
    let total_bytes = artifacts.iter().map(|artifact| artifact.byte_size).sum();
    ModelPullPlan {
        version: 1,
        operation: "pull",
        artifacts,
        bytes_to_download,
        total_bytes,
        bindings_path: bindings_path.to_path_buf(),
        binding_changes: preflight.binding_changes,
        install_record_changes: preflight.install_record_changes,
        replace: options.replace,
        offline: options.offline,
    }
}

fn choose_candidate(
    inspection: &agl_model::HubInspection,
    source_kind: HfSourceKind,
    non_interactive: bool,
    label: &str,
) -> Result<Vec<HubFileCandidate>> {
    ensure!(
        !inspection.candidates.is_empty(),
        "Hugging Face repository contains no GGUF candidates"
    );
    let groups = inspection.candidate_groups()?;
    if source_kind == HfSourceKind::File {
        ensure!(
            groups.len() == 1,
            "exact Hugging Face file inspection returned an ambiguous result"
        );
        return Ok(groups.into_iter().next().expect("one group was checked"));
    }
    if non_interactive || !io::stdin().is_terminal() {
        let candidates = groups
            .iter()
            .map(|group| group[0].exact_url())
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!(
            "repository URL is ambiguous in non-interactive mode; pass one exact GGUF URL:\n  {candidates}"
        );
    }
    eprintln!("Choose the {label} GGUF:");
    for (index, group) in groups.iter().enumerate() {
        let candidate = &group[0];
        let cached = if group.iter().all(|file| file.cached_path.is_some()) {
            " (cached)"
        } else {
            ""
        };
        eprintln!(
            "  {}) {} — {}{}",
            index + 1,
            candidate.filename,
            human_bytes(group.iter().map(|file| file.byte_size).sum()),
            cached
        );
        if group.len() > 1 {
            eprintln!("     {} required shards", group.len());
        }
    }
    eprint!("Selection [1-{}]: ", groups.len());
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let index = answer
        .trim()
        .parse::<usize>()
        .context("selection must be a candidate number")?;
    groups
        .into_iter()
        .nth(index.saturating_sub(1))
        .context("selection is outside the candidate list")
}

fn ensure_distinct_ids(selected: &[SelectedArtifact]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for artifact in selected {
        let id = &artifact.model_id;
        ensure!(
            ids.insert(id),
            "download plan contains duplicate model id `{id}`"
        );
    }
    Ok(())
}

fn preflight_pull(
    selected: &[SelectedArtifact],
    store: &ModelInstallStore,
    bindings_path: &Path,
    replace: bool,
) -> Result<PullPreflight> {
    let bindings = load_model_bindings_or_empty(bindings_path)?;
    let mut report = PullPreflight::default();
    for artifact in selected {
        let id = &artifact.model_id;
        let candidate = artifact.primary();
        let existing_record = store.get(id)?;
        let same_install = existing_record.as_ref().is_some_and(|record| {
            let additional_filenames = record
                .additional_files
                .iter()
                .map(|file| (file.filename.as_str(), file.byte_size, file.sha256.as_str()))
                .collect::<Vec<_>>();
            let selected_filenames = artifact
                .files
                .iter()
                .skip(1)
                .map(|file| {
                    (
                        file.filename.as_str(),
                        file.byte_size,
                        file.sha256.as_deref(),
                    )
                })
                .collect::<Vec<_>>();
            record.package_id.is_none()
                && record.role == artifact.role
                && record.byte_size == candidate.byte_size
                && candidate
                    .sha256
                    .as_ref()
                    .is_none_or(|sha256| sha256 == &record.sha256)
                && additional_filenames.len() == selected_filenames.len()
                && additional_filenames.iter().zip(&selected_filenames).all(
                    |((record_name, record_size, record_sha), (name, size, sha))| {
                        record_name == name
                            && record_size == size
                            && sha.is_none_or(|sha| sha == *record_sha)
                    },
                )
                && matches!(
                    &record.source,
                    InstallSource::HuggingFace {
                        repository,
                        revision,
                        filename,
                    } if repository == &candidate.repository
                        && revision == &candidate.revision
                        && filename == &candidate.filename
                )
        });
        ensure!(
            existing_record.is_none() || same_install || replace,
            "model id `{id}` already has an install record for different content; pass --id to keep both or --replace to update it"
        );
        if existing_record.as_ref().is_none_or(|record| {
            !same_install || record.state != agl_model::InstallRecordState::Active
        }) {
            report.install_record_changes.push(id.clone());
        }

        let same_binding = bindings
            .models
            .get(id)
            .is_some_and(|binding| candidate.cached_path.as_ref() == Some(&binding.path));
        ensure!(
            !bindings.models.contains_key(id) || same_binding || replace,
            "model binding `{id}` already points to {}; pass --id to keep both or --replace to update it",
            bindings
                .models
                .get(id)
                .expect("guard checked an existing binding")
                .path
                .display()
        );
        if !same_binding {
            report.binding_changes.push(id.clone());
        }
    }
    Ok(report)
}

fn preflight_import_records(
    imported: &[agl_model::ImportedModel],
    store: &ModelInstallStore,
    replace: bool,
) -> Result<()> {
    for item in imported {
        let incoming = &item.record;
        let existing = store.get(&incoming.model_id)?;
        let same = existing.as_ref().is_some_and(|record| {
            record.package_id.is_none()
                && record.role == incoming.role
                && record.source == incoming.source
                && record.path == incoming.path
                && record.byte_size == incoming.byte_size
                && record.sha256 == incoming.sha256
                && record.additional_files == incoming.additional_files
        });
        ensure!(
            existing.is_none() || same || replace,
            "model id `{}` already has an install record for different content; pass --id to keep both or --replace to update it",
            incoming.model_id
        );
    }
    Ok(())
}

fn ensure_download_disk_space(plan: &ModelPullPlan) -> Result<()> {
    if plan.bytes_to_download == 0 {
        return Ok(());
    }
    let cache_dir = agl_model::hugging_face_cache_dir();
    let probe = cache_dir
        .ancestors()
        .find(|path| path.exists())
        .context("model cache path has no existing ancestor")?;
    let available_bytes = available_filesystem_bytes(probe)?;
    ensure!(
        available_bytes >= plan.bytes_to_download,
        "model download needs {} but only {} is free on {}",
        human_bytes(plan.bytes_to_download),
        human_bytes(available_bytes),
        probe.display()
    );
    Ok(())
}

#[cfg(unix)]
fn available_filesystem_bytes(path: &std::path::Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .context("filesystem probe path contains a NUL byte")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable storage.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect model-cache filesystem");
    }
    // SAFETY: successful statvfs initializes the output structure.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
fn available_filesystem_bytes(_path: &std::path::Path) -> Result<u64> {
    Ok(u64::MAX)
}

fn print_pull_plan(plan: &ModelPullPlan, json: bool) -> Result<()> {
    if json {
        return Ok(());
    }
    println!("Model acquisition plan:");
    for artifact in &plan.artifacts {
        let state = if artifact.cached_path.is_some() {
            "cached"
        } else {
            "download"
        };
        println!(
            "  {} ({:?}): {} [{}]",
            artifact.model_id,
            artifact.role,
            human_bytes(artifact.byte_size),
            state
        );
        println!(
            "    https://huggingface.co/{}/resolve/{}/{}",
            artifact.repository, artifact.revision, artifact.filename
        );
    }
    println!("Download: {}", human_bytes(plan.bytes_to_download));
    println!("Bindings: {}", plan.bindings_path.display());
    if !plan.binding_changes.is_empty() {
        println!(
            "Binding changes: {}",
            plan.binding_changes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !plan.install_record_changes.is_empty() {
        println!(
            "Install record changes: {}",
            plan.install_record_changes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn print_pull_report(json: bool, report: ModelPullReport) -> Result<()> {
    crate::print_json_or(json, &report, || {
        if report.state == "planned" {
            println!("Dry run complete; no files were downloaded or changed.");
        } else {
            println!("Model artifacts are ready and explicitly bound.");
            for record in &report.install_records {
                println!("  {} -> {}", record.model_id, record.path.display());
            }
        }
    })
}

pub(crate) fn render_progress(event: ModelProgressEvent) {
    match event {
        ModelProgressEvent::Started {
            total_files,
            total_bytes,
            ..
        } => eprintln!(
            "Downloading {total_files} file(s), {} total...",
            human_bytes(total_bytes)
        ),
        ModelProgressEvent::Aggregate {
            bytes_completed,
            total_bytes,
            ..
        } if total_bytes > 0 => eprintln!(
            "  {:>3}%  {} / {}",
            bytes_completed.saturating_mul(100) / total_bytes,
            human_bytes(bytes_completed),
            human_bytes(total_bytes)
        ),
        ModelProgressEvent::Verifying { filename, .. } => {
            eprintln!("Verifying {filename}...")
        }
        ModelProgressEvent::Cancelled { .. } => eprintln!("Download cancelled."),
        ModelProgressEvent::Complete { .. } => eprintln!("Download complete."),
        ModelProgressEvent::Queued { .. }
        | ModelProgressEvent::File { .. }
        | ModelProgressEvent::Aggregate { .. } => {}
    }
}

fn render_pull_progress(event: ModelProgressEvent, cached_only: bool) {
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

fn inspector(runtime: &AgentLibreRuntimeConfig) -> ModelInspector {
    ModelInspector::new(
        ModelInstallStore::new(runtime.paths.model_install_root()),
        model_bindings_path(&runtime.paths.config_dir),
    )
}

fn lifecycle_service(runtime: &AgentLibreRuntimeConfig) -> Result<ModelLifecycleService> {
    let protected = protected_model_ids(runtime)?;
    Ok(ModelLifecycleService::new(
        ModelInstallStore::new(runtime.paths.model_install_root()),
        &runtime.paths.config_dir,
    )
    .with_lease_root(runtime.paths.model_lease_root())
    .with_protected_ids(protected))
}

fn protected_model_ids(runtime: &AgentLibreRuntimeConfig) -> Result<BTreeSet<ModelId>> {
    let mut protected = BTreeSet::new();
    if let Ok(workspace) = runtime.resolve_workspace_root(None)
        && let Some(reference) = agl_repo::read_workspace_default_function(&workspace)?
        && let Ok(input) = agl_repo::package_composition_input(&workspace)
        && let Ok(function) = agl_runtime::resolve_composed_runtime_function(
            &runtime.paths,
            input,
            &reference.to_string(),
            true,
        )
        && function.model_profile.is_some()
    {
        let composition = agl_runtime::compose_packages(
            &runtime.paths,
            agl_repo::package_composition_input(&workspace)?,
        )?;
        if let Ok(bundle) = composition.resolve_runtime_bundle(
            &workspace,
            &runtime.paths.config_dir,
            &reference.to_string(),
            true,
            &[],
        ) && let Some(model) = bundle.model
        {
            protected.extend(
                model
                    .package
                    .required_artifacts()
                    .map(|artifact| artifact.model_id.clone()),
            );
        }
    }
    let checkpoint_store = SetupCheckpointStore::new(runtime.paths.setup_state_root());
    for checkpoint in checkpoint_store.list()? {
        protected.extend(
            checkpoint
                .planned_artifacts
                .into_iter()
                .map(|artifact| artifact.model_id),
        );
    }
    Ok(protected)
}

fn run_lifecycle_plan(
    plan: ModelLifecyclePlan,
    yes: bool,
    dry_run: bool,
    json: bool,
    execute: impl FnOnce(&ModelLifecyclePlan) -> Result<()>,
) -> Result<()> {
    if !json {
        println!("Model lifecycle plan: {:?}", plan.operation);
        if plan.targets.is_empty() {
            println!("  No eligible records.");
        }
        for target in &plan.targets {
            println!("  {}", target.model_id);
            if let Some(path) = &target.binding_path {
                println!("    binding: {}", path.display());
            }
            if let Some(path) = &target.install_record_path {
                println!("    install record: {}", path.display());
            }
            if let Some(path) = &target.cache_path {
                println!("    cache: {}", path.display());
            }
            for path in &target.additional_cache_paths {
                println!("    cache: {}", path.display());
            }
            println!("    reclaimable: {}", human_bytes(target.bytes));
        }
        for entry in &plan.prune_entries {
            for blob in &entry.blobs {
                println!(
                    "    blob: {} ({})",
                    blob.path.display(),
                    human_bytes(blob.reclaimable_bytes)
                );
            }
        }
        println!("  Reclaimable: {}", human_bytes(plan.total_bytes));
    }
    if dry_run {
        return crate::print_json_or(
            json,
            &ModelLifecycleReport {
                version: 1,
                state: "planned",
                plan,
            },
            || println!("Dry run complete; no files were changed."),
        );
    }
    if !plan.targets.is_empty() {
        confirm(yes, false, "Apply this model lifecycle plan? [y/N] ")?;
        execute(&plan)?;
    }
    crate::print_json_or(
        json,
        &ModelLifecycleReport {
            version: 1,
            state: "complete",
            plan,
        },
        || println!("Model lifecycle operation complete."),
    )
}

pub(crate) fn confirm(yes: bool, non_interactive: bool, prompt: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if non_interactive || !io::stdin().is_terminal() {
        bail!("confirmation is required; inspect the plan and pass --yes")
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    ensure!(
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        "operation cancelled"
    );
    Ok(())
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use agl_model::{InstallRecordState, ModelInstallRecord};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-model-{name}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn selected(path: Option<PathBuf>, repository: &str) -> SelectedArtifact {
        SelectedArtifact {
            model_id: ModelId::new("test-model").unwrap(),
            role: ModelArtifactRole::Main,
            files: vec![HubFileCandidate {
                repository: repository.to_string(),
                revision: "a".repeat(40),
                filename: "model.gguf".to_string(),
                byte_size: 10,
                sha256: Some("b".repeat(64)),
                cached_path: path,
            }],
        }
    }

    fn record(path: PathBuf, repository: &str) -> ModelInstallRecord {
        ModelInstallRecord {
            version: 1,
            model_id: ModelId::new("test-model").unwrap(),
            package_id: None,
            role: ModelArtifactRole::Main,
            source: InstallSource::HuggingFace {
                repository: repository.to_string(),
                revision: "a".repeat(40),
                filename: "model.gguf".to_string(),
            },
            path,
            byte_size: 10,
            sha256: "b".repeat(64),
            additional_files: Vec::new(),
            installed_at_unix_ms: 1,
            state: InstallRecordState::Active,
        }
    }

    fn seed_record(store: &ModelInstallStore, record: &ModelInstallRecord) {
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(
            store.record_path(&record.model_id),
            serde_json::to_vec_pretty(record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn pull_rejects_an_unbound_install_record_collision() {
        let root = root("record-collision");
        let store = ModelInstallStore::new(root.join("records"));
        seed_record(&store, &record(root.join("old.gguf"), "owner/old"));
        let error = preflight_pull(
            &[selected(None, "owner/new")],
            &store,
            &root.join("models.toml"),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("--id"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn repeated_exact_pull_has_no_binding_or_record_changes() {
        let root = root("repeat");
        let model_path = root.join("model.gguf");
        std::fs::write(&model_path, b"GGUFmodel!").unwrap();
        let store = ModelInstallStore::new(root.join("records"));
        let install_record = record(model_path.clone(), "owner/repo");
        let bindings_path = root.join("models.toml");
        let mut patch = ModelBindingPatch::default();
        patch.insert(ModelId::new("test-model").unwrap(), model_path.clone());
        ModelInstallTransaction::new(store.clone(), &bindings_path)
            .unwrap()
            .commit(ModelInstallTransactionInput::new(
                vec![install_record],
                patch,
                false,
            ))
            .unwrap();

        let report = preflight_pull(
            &[selected(Some(model_path), "owner/repo")],
            &store,
            &bindings_path,
            false,
        )
        .unwrap();
        assert!(report.binding_changes.is_empty());
        assert!(report.install_record_changes.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn import_rejects_an_unbound_install_record_collision() {
        let root = root("import-record-collision");
        let first_path = root.join("first.gguf");
        let second_path = root.join("second.gguf");
        std::fs::write(&first_path, b"GGUFfirst").unwrap();
        std::fs::write(&second_path, b"GGUFsecond").unwrap();
        let id = ModelId::new("same-id").unwrap();
        let first =
            import_local_model(&first_path, Some(id.clone()), ModelArtifactRole::Main).unwrap();
        let second = import_local_model(&second_path, Some(id), ModelArtifactRole::Main).unwrap();
        let store = ModelInstallStore::new(root.join("records"));
        seed_record(&store, &first.record);

        let error = preflight_import_records(&[second], &store, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--id"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
}
