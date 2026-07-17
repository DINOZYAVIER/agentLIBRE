use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agl_config::ModelId;
use futures_util::StreamExt as _;
use hf_hub::progress::{DownloadEvent, FileStatus, ProgressEvent, ProgressHandler};
use hf_hub::repository::{FileMetadataInfo, HFRepository, RepoTreeEntry, RepoTypeModel};
use hf_hub::{HFClient, HFError, split_id};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::install::validate_gguf;
use crate::{
    HfSource, HfSourceKind, ModelArtifactRole, ModelBindingPatch, ModelPackage, ModelPackageId,
};

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CACHE_POINTER: AtomicU64 = AtomicU64::new(1);
const QUEUE_CAPACITY: usize = 8;
const PROGRESS_CAPACITY: usize = 128;
const CACHE_LOCK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DOWNLOAD_RETRY_ATTEMPTS: usize = 4;

#[derive(Clone, Debug, Default)]
struct WorkerClientConfig {
    cache_dir: Option<PathBuf>,
    endpoint: Option<String>,
    token: Option<String>,
    retry_max_attempts: Option<usize>,
    retry_base_delay: Option<Duration>,
    offline: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDownloadSpec {
    pub package_id: Option<ModelPackageId>,
    pub model_id: ModelId,
    pub role: ModelArtifactRole,
    pub repository: String,
    pub revision: String,
    pub filename: String,
    pub byte_size: u64,
    pub sha256: String,
    #[serde(default)]
    pub additional_files: Vec<ArtifactFileDownloadSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFileDownloadSpec {
    pub filename: String,
    pub byte_size: u64,
    pub sha256: String,
}

impl ArtifactDownloadSpec {
    pub fn from_package(package: &ModelPackage) -> Vec<Self> {
        package
            .required_artifacts()
            .map(|artifact| Self {
                package_id: Some(package.id.clone()),
                model_id: artifact.model_id.clone(),
                role: artifact.role,
                repository: package.repository.clone(),
                revision: package.revision.clone(),
                filename: artifact.filename.clone(),
                byte_size: artifact.byte_size,
                sha256: artifact.sha256.clone(),
                additional_files: Vec::new(),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDownloadRequest {
    pub artifacts: Vec<ArtifactDownloadSpec>,
    pub offline: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCacheStatus {
    pub model_id: ModelId,
    pub role: ModelArtifactRole,
    pub repository: String,
    pub revision: String,
    pub filename: String,
    pub expected_byte_size: u64,
    pub cached_path: Option<PathBuf>,
    pub cached_byte_size: Option<u64>,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubFileCandidate {
    pub repository: String,
    pub revision: String,
    pub filename: String,
    pub byte_size: u64,
    pub sha256: Option<String>,
    pub cached_path: Option<PathBuf>,
}

impl HubFileCandidate {
    pub fn exact_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repository, self.revision, self.filename
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubInspection {
    pub repository: String,
    pub revision: String,
    pub candidates: Vec<HubFileCandidate>,
}

impl HubInspection {
    pub fn candidate_groups(&self) -> Result<Vec<Vec<HubFileCandidate>>, ModelDownloadError> {
        let mut groups = Vec::new();
        let mut consumed = std::collections::BTreeSet::new();
        for candidate in &self.candidates {
            hf_model_cache_folder(&candidate.repository)?;
            ensure_cache_relative_path(&candidate.filename, "Hugging Face candidate filename")?;
            if candidate.repository != self.repository
                || candidate.revision != self.revision
                || !is_commit_hash(&candidate.revision)
                || candidate.byte_size <= 4
            {
                return Err(ModelDownloadError::Validation {
                    filename: candidate.filename.clone(),
                    message:
                        "Hugging Face candidate identity, immutable revision, or size is invalid"
                            .to_string(),
                });
            }
            if candidate
                .sha256
                .as_ref()
                .is_some_and(|sha256| !is_sha256(sha256))
            {
                return Err(ModelDownloadError::Validation {
                    filename: candidate.filename.clone(),
                    message: "Hugging Face candidate has an invalid SHA-256".to_string(),
                });
            }
            if consumed.contains(&candidate.filename) {
                continue;
            }
            let Some(descriptor) = SplitDescriptor::parse(&candidate.filename) else {
                consumed.insert(candidate.filename.clone());
                groups.push(vec![candidate.clone()]);
                continue;
            };
            let mut group = self
                .candidates
                .iter()
                .filter_map(|other| {
                    let other_descriptor = SplitDescriptor::parse(&other.filename)?;
                    (other_descriptor.group_key() == descriptor.group_key())
                        .then_some((other_descriptor.index, other.clone()))
                })
                .collect::<Vec<_>>();
            group.sort_by_key(|(index, _)| *index);
            if group.len() != descriptor.total
                || group
                    .iter()
                    .enumerate()
                    .any(|(index, (part, _))| *part != index + 1)
            {
                return Err(ModelDownloadError::IncompleteSplit {
                    filename: candidate.filename.clone(),
                    expected_shards: descriptor.total,
                    found_shards: group.len(),
                });
            }
            let group = group
                .into_iter()
                .map(|(_, candidate)| {
                    consumed.insert(candidate.filename.clone());
                    candidate
                })
                .collect();
            groups.push(group);
        }
        Ok(groups)
    }
}

impl ModelDownloadRequest {
    pub fn for_package(package: &ModelPackage, offline: bool) -> Self {
        Self {
            artifacts: ArtifactDownloadSpec::from_package(package),
            offline,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelProgressEvent {
    Queued {
        job_id: u64,
    },
    Started {
        job_id: u64,
        total_files: usize,
        total_bytes: u64,
    },
    File {
        job_id: u64,
        filename: String,
        bytes_completed: u64,
        total_bytes: u64,
        complete: bool,
    },
    Aggregate {
        job_id: u64,
        bytes_completed: u64,
        total_bytes: u64,
    },
    Verifying {
        job_id: u64,
        filename: String,
    },
    Complete {
        job_id: u64,
    },
    Cancelled {
        job_id: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadedArtifact {
    pub spec: ArtifactDownloadSpec,
    pub path: PathBuf,
    pub byte_size: u64,
    pub sha256: String,
    pub additional_files: Vec<DownloadedArtifactFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadedArtifactFile {
    pub spec: ArtifactFileDownloadSpec,
    pub path: PathBuf,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDownloadResult {
    pub job_id: u64,
    pub artifacts: Vec<DownloadedArtifact>,
    pub binding_patch: ModelBindingPatch,
}

#[derive(Debug, Error)]
pub enum ModelDownloadError {
    #[error("download request contains no artifacts")]
    EmptyPlan,
    #[error("model download queue is full")]
    QueueFull,
    #[error("model download worker is unavailable")]
    WorkerUnavailable,
    #[error("model download was cancelled")]
    Cancelled,
    #[error("Hugging Face authentication is required for {repository}")]
    AuthRequired { repository: String },
    #[error("Hugging Face access is forbidden for {repository}")]
    Forbidden { repository: String },
    #[error("Hugging Face repository was not found: {repository}")]
    RepositoryNotFound { repository: String },
    #[error("Hugging Face revision was not found: {revision} in {repository}")]
    RevisionNotFound {
        repository: String,
        revision: String,
    },
    #[error("Hugging Face file was not found: {filename} in {repository}")]
    FileNotFound {
        repository: String,
        filename: String,
    },
    #[error("Hugging Face rate limit reached for {repository}")]
    RateLimited { repository: String },
    #[error("offline cache miss for {filename} in {repository}@{revision}")]
    OfflineCacheMiss {
        repository: String,
        revision: String,
        filename: String,
    },
    #[error("Hugging Face cache lock timed out for {path}")]
    CacheLockTimeout { path: PathBuf },
    #[error(
        "split GGUF `{filename}` is incomplete: expected {expected_shards} shards, found {found_shards}"
    )]
    IncompleteSplit {
        filename: String,
        expected_shards: usize,
        found_shards: usize,
    },
    #[error(
        "repository metadata for {repository} is unavailable in offline mode; use an exact file URL"
    )]
    OfflineMetadataUnavailable { repository: String },
    #[error("download failed for {repository}/{filename}: {message}")]
    Hub {
        repository: String,
        filename: String,
        message: String,
    },
    #[error("downloaded artifact validation failed for {filename}: {message}")]
    Validation { filename: String, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SplitDescriptor {
    prefix: String,
    total: usize,
    index: usize,
}

impl SplitDescriptor {
    fn parse(filename: &str) -> Option<Self> {
        let without_extension = filename.strip_suffix(".gguf")?;
        let (left, total) = without_extension.rsplit_once("-of-")?;
        let (prefix, index) = left.rsplit_once('-')?;
        if index.len() < 2
            || total.len() != index.len()
            || !index.bytes().all(|byte| byte.is_ascii_digit())
            || !total.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let index = index.parse().ok()?;
        let total = total.parse().ok()?;
        (total > 1 && index > 0 && index <= total).then(|| Self {
            prefix: prefix.to_string(),
            total,
            index,
        })
    }

    fn group_key(&self) -> (&str, usize) {
        (&self.prefix, self.total)
    }
}

enum WorkerCommand {
    Download {
        job_id: u64,
        request: ModelDownloadRequest,
        cancelled: Arc<AtomicBool>,
        progress: mpsc::SyncSender<ModelProgressEvent>,
        result: mpsc::SyncSender<Result<ModelDownloadResult, ModelDownloadError>>,
    },
    Inspect {
        source: HfSource,
        offline: bool,
        result: mpsc::SyncSender<Result<HubInspection, ModelDownloadError>>,
    },
    CacheStatus {
        request: ModelDownloadRequest,
        result: mpsc::SyncSender<Result<Vec<ModelCacheStatus>, ModelDownloadError>>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct ModelDownloadHandle {
    sender: mpsc::SyncSender<WorkerCommand>,
    shutdown: Arc<AtomicBool>,
}

impl ModelDownloadHandle {
    pub fn cache_status(
        &self,
        request: ModelDownloadRequest,
    ) -> Result<Vec<ModelCacheStatus>, ModelDownloadError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(ModelDownloadError::WorkerUnavailable);
        }
        validate_download_request(&request)?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(WorkerCommand::CacheStatus { request, result })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ModelDownloadError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => ModelDownloadError::WorkerUnavailable,
            })?;
        receiver
            .recv()
            .map_err(|_| ModelDownloadError::WorkerUnavailable)?
    }

    pub fn inspect(
        &self,
        source: HfSource,
        offline: bool,
    ) -> Result<HubInspection, ModelDownloadError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(ModelDownloadError::WorkerUnavailable);
        }
        let (result, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(WorkerCommand::Inspect {
                source,
                offline,
                result,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ModelDownloadError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => ModelDownloadError::WorkerUnavailable,
            })?;
        receiver
            .recv()
            .map_err(|_| ModelDownloadError::WorkerUnavailable)?
    }

    pub fn submit(
        &self,
        request: ModelDownloadRequest,
    ) -> Result<ModelDownloadJob, ModelDownloadError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(ModelDownloadError::WorkerUnavailable);
        }
        validate_download_request(&request)?;
        let job_id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (progress_sender, progress_receiver) = mpsc::sync_channel(PROGRESS_CAPACITY);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        progress_sender
            .try_send(ModelProgressEvent::Queued { job_id })
            .expect("new progress channel has capacity");
        self.sender
            .try_send(WorkerCommand::Download {
                job_id,
                request,
                cancelled: Arc::clone(&cancelled),
                progress: progress_sender,
                result: result_sender,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ModelDownloadError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => ModelDownloadError::WorkerUnavailable,
            })?;
        Ok(ModelDownloadJob {
            id: job_id,
            cancelled,
            progress: progress_receiver,
            result: Some(result_receiver),
            finished: false,
        })
    }
}

#[derive(Debug)]
pub struct ModelDownloadJob {
    id: u64,
    cancelled: Arc<AtomicBool>,
    progress: mpsc::Receiver<ModelProgressEvent>,
    result: Option<mpsc::Receiver<Result<ModelDownloadResult, ModelDownloadError>>>,
    finished: bool,
}

impl ModelDownloadJob {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn try_progress(&self) -> Option<ModelProgressEvent> {
        self.progress.try_recv().ok()
    }

    pub fn progress_timeout(&self, timeout: Duration) -> Option<ModelProgressEvent> {
        self.progress.recv_timeout(timeout).ok()
    }

    pub fn wait(mut self) -> Result<ModelDownloadResult, ModelDownloadError> {
        let receiver = self
            .result
            .take()
            .expect("download result receiver is present");
        let result = receiver
            .recv()
            .map_err(|_| ModelDownloadError::WorkerUnavailable)?;
        self.finished = true;
        result
    }

    pub fn wait_with_progress(
        mut self,
        mut on_progress: impl FnMut(ModelProgressEvent),
    ) -> Result<ModelDownloadResult, ModelDownloadError> {
        let receiver = self
            .result
            .take()
            .expect("download result receiver is present");
        loop {
            while let Ok(event) = self.progress.try_recv() {
                on_progress(event);
            }
            match receiver.try_recv() {
                Ok(result) => {
                    while let Ok(event) = self.progress.try_recv() {
                        on_progress(event);
                    }
                    self.finished = true;
                    return result;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(ModelDownloadError::WorkerUnavailable);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match self.progress.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => on_progress(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let result = receiver
                        .recv()
                        .map_err(|_| ModelDownloadError::WorkerUnavailable)?;
                    self.finished = true;
                    return result;
                }
            }
        }
    }
}

impl Drop for ModelDownloadJob {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel();
        }
    }
}

pub struct ModelDownloadWorker {
    handle: ModelDownloadHandle,
    thread: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl ModelDownloadWorker {
    pub fn spawn() -> Result<Self, ModelDownloadError> {
        Self::spawn_with_client_config(WorkerClientConfig {
            offline: crate::hugging_face_offline(),
            ..WorkerClientConfig::default()
        })
    }

    fn spawn_with_client_config(config: WorkerClientConfig) -> Result<Self, ModelDownloadError> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("agl-model-download".to_string())
            .spawn(move || worker_main(receiver, worker_shutdown, config))
            .map_err(|_| ModelDownloadError::WorkerUnavailable)?;
        Ok(Self {
            handle: ModelDownloadHandle {
                sender,
                shutdown: Arc::clone(&shutdown),
            },
            thread: Some(thread),
            shutdown,
        })
    }

    pub fn handle(&self) -> ModelDownloadHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<(), ModelDownloadError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), ModelDownloadError> {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.handle.sender.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| ModelDownloadError::WorkerUnavailable)?;
        }
        Ok(())
    }
}

impl Drop for ModelDownloadWorker {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn worker_main(
    receiver: mpsc::Receiver<WorkerCommand>,
    shutdown: Arc<AtomicBool>,
    config: WorkerClientConfig,
) {
    let force_offline = config.offline;
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("agl-model-download-async")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "failed to start model download async runtime");
            return;
        }
    };
    let mut builder = HFClient::builder();
    if let Some(cache_dir) = config.cache_dir {
        builder = builder.cache_dir(cache_dir);
    }
    if let Some(endpoint) = config.endpoint {
        builder = builder.endpoint(endpoint);
    }
    if let Some(token) = config.token {
        builder = builder.token(token);
    }
    if let Some(attempts) = config.retry_max_attempts {
        builder = builder.retry_max_attempts(attempts);
    }
    if let Some(delay) = config.retry_base_delay {
        builder = builder.retry_base_delay(delay);
    }
    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "failed to initialize Hugging Face client");
            return;
        }
    };
    while let Ok(command) = receiver.recv() {
        match command {
            WorkerCommand::Download {
                job_id,
                mut request,
                cancelled,
                progress,
                result,
            } => {
                request.offline |= force_offline;
                let value = runtime.block_on(run_download(
                    &client, job_id, request, &cancelled, &shutdown, &progress,
                ));
                let _ = result.send(value);
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
            }
            WorkerCommand::Inspect {
                source,
                offline,
                result,
            } => {
                let value =
                    runtime.block_on(run_inspection(&client, source, offline || force_offline));
                let _ = result.send(value);
            }
            WorkerCommand::CacheStatus { request, result } => {
                let value = runtime.block_on(run_cache_status(&client, request));
                let _ = result.send(value);
            }
            WorkerCommand::Shutdown => break,
        }
    }
}

async fn run_cache_status(
    client: &HFClient,
    request: ModelDownloadRequest,
) -> Result<Vec<ModelCacheStatus>, ModelDownloadError> {
    let cache = client
        .scan_cache()
        .send()
        .await
        .map_err(|error| ModelDownloadError::Hub {
            repository: String::new(),
            filename: String::new(),
            message: error.to_string(),
        })?;
    let mut statuses = Vec::new();
    for artifact in request.artifacts {
        let files = std::iter::once(ArtifactFileDownloadSpec {
            filename: artifact.filename.clone(),
            byte_size: artifact.byte_size,
            sha256: artifact.sha256.clone(),
        })
        .chain(artifact.additional_files.clone());
        for file in files {
            let cached = cache
                .repos
                .iter()
                .find(|repo| repo.repo_type == "model" && repo.repo_id == artifact.repository)
                .and_then(|repo| {
                    repo.revisions
                        .iter()
                        .find(|revision| revision.commit_hash == artifact.revision)
                })
                .and_then(|revision| {
                    revision
                        .files
                        .iter()
                        .find(|cached| cached.file_name == file.filename)
                });
            statuses.push(ModelCacheStatus {
                model_id: artifact.model_id.clone(),
                role: artifact.role,
                repository: artifact.repository.clone(),
                revision: artifact.revision.clone(),
                filename: file.filename,
                expected_byte_size: file.byte_size,
                cached_path: cached.map(|file| file.file_path.clone()),
                cached_byte_size: cached.map(|file| file.size_on_disk),
                complete: cached.is_some_and(|cached| {
                    file.byte_size == 0 || cached.size_on_disk == file.byte_size
                }),
            });
        }
    }
    Ok(statuses)
}

async fn run_inspection(
    client: &HFClient,
    source: HfSource,
    offline: bool,
) -> Result<HubInspection, ModelDownloadError> {
    if offline {
        let (Some(revision), Some(filename)) = (source.revision, source.file) else {
            return Err(ModelDownloadError::OfflineMetadataUnavailable {
                repository: source.repository,
            });
        };
        let candidates = cached_candidates(client, &source.repository, &revision, &filename)
            .await
            .map_err(|error| ModelDownloadError::Hub {
                repository: source.repository.clone(),
                filename: filename.clone(),
                message: error.to_string(),
            })?;
        if candidates.is_empty() {
            return Err(ModelDownloadError::OfflineCacheMiss {
                repository: source.repository.clone(),
                revision: revision.clone(),
                filename: filename.clone(),
            });
        }
        let resolved_revision = candidates[0].revision.clone();
        if !is_commit_hash(&resolved_revision) {
            return Err(ModelDownloadError::Validation {
                filename,
                message: "cached Hugging Face revision is not an immutable commit SHA".to_string(),
            });
        }
        let inspection = HubInspection {
            repository: source.repository.clone(),
            revision: resolved_revision,
            candidates,
        };
        inspection.candidate_groups()?;
        return Ok(inspection);
    }

    let (owner, name) = split_id(&source.repository);
    let repository = client.model(owner, name);
    let requested_revision = source.revision.as_deref().unwrap_or("main");
    if source.kind == HfSourceKind::File {
        let filename = source.file.as_deref().expect("file source has filename");
        let info = repository
            .info()
            .revision(requested_revision)
            .send()
            .await
            .map_err(|error| {
                map_inspection_error(
                    error,
                    &source.repository,
                    requested_revision,
                    Some(filename),
                )
            })?;
        let commit_hash =
            info.sha
                .filter(|sha| is_commit_hash(sha))
                .ok_or_else(|| ModelDownloadError::Hub {
                    repository: source.repository.clone(),
                    filename: filename.to_string(),
                    message: "Hub metadata did not return a valid immutable commit SHA".to_string(),
                })?;
        let all_candidates =
            list_gguf_candidates(&repository, &source.repository, &commit_hash).await?;
        let selected_descriptor = SplitDescriptor::parse(filename);
        let mut candidates: Vec<HubFileCandidate> = if selected_descriptor.is_some() {
            all_candidates
                .into_iter()
                .filter(|candidate| {
                    SplitDescriptor::parse(&candidate.filename)
                        .zip(selected_descriptor.as_ref())
                        .is_some_and(|(candidate, selected)| {
                            candidate.group_key() == selected.group_key()
                        })
                })
                .collect()
        } else {
            all_candidates
                .into_iter()
                .filter(|candidate| candidate.filename == filename)
                .collect()
        };
        if candidates.is_empty() {
            return Err(ModelDownloadError::FileNotFound {
                repository: source.repository,
                filename: filename.to_string(),
            });
        }
        mark_cache_hits(client, &source.repository, &commit_hash, &mut candidates).await;
        let inspection = HubInspection {
            repository: source.repository.clone(),
            revision: commit_hash,
            candidates,
        };
        inspection.candidate_groups()?;
        return Ok(inspection);
    }

    let info = repository
        .info()
        .revision(requested_revision)
        .send()
        .await
        .map_err(|error| {
            map_inspection_error(error, &source.repository, requested_revision, None)
        })?;
    let revision =
        info.sha
            .filter(|sha| is_commit_hash(sha))
            .ok_or_else(|| ModelDownloadError::Hub {
                repository: source.repository.clone(),
                filename: String::new(),
                message: "Hub metadata did not return a valid immutable commit SHA".to_string(),
            })?;
    let mut candidates = list_gguf_candidates(&repository, &source.repository, &revision).await?;
    mark_cache_hits(client, &source.repository, &revision, &mut candidates).await;
    candidates.sort_by(|left, right| left.filename.cmp(&right.filename));
    Ok(HubInspection {
        repository: source.repository,
        revision,
        candidates,
    })
}

async fn list_gguf_candidates(
    repository: &HFRepository<RepoTypeModel>,
    repository_id: &str,
    revision: &str,
) -> Result<Vec<HubFileCandidate>, ModelDownloadError> {
    let stream = repository
        .list_tree()
        .revision(revision.to_string())
        .recursive(true)
        .expand(true)
        .send()
        .map_err(|error| map_inspection_error(error, repository_id, revision, None))?;
    futures_util::pin_mut!(stream);
    let mut candidates = Vec::new();
    while let Some(entry) = stream.next().await {
        let entry =
            entry.map_err(|error| map_inspection_error(error, repository_id, revision, None))?;
        let RepoTreeEntry::File {
            path, size, lfs, ..
        } = entry
        else {
            continue;
        };
        if !path.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        ensure_cache_relative_path(&path, "Hugging Face tree filename")?;
        if size <= 4 {
            return Err(ModelDownloadError::Validation {
                filename: path,
                message: "Hub tree metadata did not provide a valid GGUF size".to_string(),
            });
        }
        let sha256 = lfs
            .and_then(|value| value.sha256)
            .filter(|value| is_sha256(value));
        candidates.push(HubFileCandidate {
            repository: repository_id.to_string(),
            revision: revision.to_string(),
            filename: path,
            byte_size: size,
            sha256,
            cached_path: None,
        });
    }
    candidates.sort_by(|left, right| left.filename.cmp(&right.filename));
    Ok(candidates)
}

async fn mark_cache_hits(
    client: &HFClient,
    repository: &str,
    revision: &str,
    candidates: &mut [HubFileCandidate],
) {
    let Ok(cache) = client.scan_cache().send().await else {
        return;
    };
    let Some(cached_revision) = cache
        .repos
        .iter()
        .find(|repo| repo.repo_type == "model" && repo.repo_id == repository)
        .and_then(|repo| {
            repo.revisions
                .iter()
                .find(|cached_revision| cached_revision.commit_hash == revision)
        })
    else {
        return;
    };
    for candidate in candidates {
        candidate.cached_path = cached_revision
            .files
            .iter()
            .find(|file| file.file_name == candidate.filename)
            .map(|file| file.file_path.clone());
    }
}

async fn cached_candidates(
    client: &HFClient,
    repository: &str,
    revision: &str,
    filename: &str,
) -> Result<Vec<HubFileCandidate>, HFError> {
    let cache = client.scan_cache().send().await?;
    let cached_revision = cache
        .repos
        .iter()
        .find(|repo| repo.repo_type == "model" && repo.repo_id == repository)
        .and_then(|repo| {
            repo.revisions.iter().find(|cached_revision| {
                cached_revision.commit_hash == revision
                    || cached_revision.refs.iter().any(|value| value == revision)
            })
        });
    let files = cached_revision
        .map(|revision| revision.files.as_slice())
        .unwrap_or_default();
    let resolved_revision = cached_revision
        .map(|revision| revision.commit_hash.as_str())
        .unwrap_or(revision);
    let selected_descriptor = SplitDescriptor::parse(filename);
    let mut candidates = files
        .iter()
        .filter(|file| {
            if let Some(selected) = &selected_descriptor {
                SplitDescriptor::parse(&file.file_name)
                    .is_some_and(|candidate| candidate.group_key() == selected.group_key())
            } else {
                file.file_name == filename
            }
        })
        .map(|file| HubFileCandidate {
            repository: repository.to_string(),
            revision: resolved_revision.to_string(),
            filename: file.file_name.clone(),
            byte_size: file.size_on_disk,
            sha256: file
                .blob_path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|value| is_sha256(value))
                .map(str::to_string),
            cached_path: Some(file.file_path.clone()),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.filename.cmp(&right.filename));
    Ok(candidates)
}

async fn run_download(
    client: &HFClient,
    job_id: u64,
    request: ModelDownloadRequest,
    cancelled: &AtomicBool,
    shutdown: &AtomicBool,
    progress: &mpsc::SyncSender<ModelProgressEvent>,
) -> Result<ModelDownloadResult, ModelDownloadError> {
    let download = DownloadContext {
        client,
        job_id,
        offline: request.offline,
        cancelled,
        shutdown,
        progress,
    };
    let total_bytes = request
        .artifacts
        .iter()
        .map(|artifact| {
            artifact.byte_size
                + artifact
                    .additional_files
                    .iter()
                    .map(|file| file.byte_size)
                    .sum::<u64>()
        })
        .sum();
    let total_files = request
        .artifacts
        .iter()
        .map(|artifact| 1 + artifact.additional_files.len())
        .sum();
    send_progress(
        progress,
        ModelProgressEvent::Started {
            job_id,
            total_files,
            total_bytes,
        },
    );
    let mut downloaded = Vec::new();
    let mut binding_patch = ModelBindingPatch::default();
    for artifact in request.artifacts {
        if is_cancelled(cancelled, shutdown) {
            send_progress(progress, ModelProgressEvent::Cancelled { job_id });
            return Err(ModelDownloadError::Cancelled);
        }
        let primary = ArtifactFileDownloadSpec {
            filename: artifact.filename.clone(),
            byte_size: artifact.byte_size,
            sha256: artifact.sha256.clone(),
        };
        let (path, byte_size, sha256) = download
            .file(&artifact.repository, &artifact.revision, &primary)
            .await?;
        let mut additional_files = Vec::new();
        for file in &artifact.additional_files {
            let (path, byte_size, sha256) = download
                .file(&artifact.repository, &artifact.revision, file)
                .await?;
            additional_files.push(DownloadedArtifactFile {
                spec: file.clone(),
                path,
                byte_size,
                sha256,
            });
        }
        binding_patch.insert(artifact.model_id.clone(), path.clone());
        downloaded.push(DownloadedArtifact {
            spec: artifact,
            path,
            byte_size,
            sha256,
            additional_files,
        });
    }
    send_progress(progress, ModelProgressEvent::Complete { job_id });
    Ok(ModelDownloadResult {
        job_id,
        artifacts: downloaded,
        binding_patch,
    })
}

fn validate_download_request(request: &ModelDownloadRequest) -> Result<(), ModelDownloadError> {
    if request.artifacts.is_empty() {
        return Err(ModelDownloadError::EmptyPlan);
    }
    let mut model_ids = std::collections::BTreeSet::new();
    for artifact in &request.artifacts {
        if !model_ids.insert(&artifact.model_id) {
            return Err(ModelDownloadError::Validation {
                filename: artifact.filename.clone(),
                message: format!(
                    "duplicate model id `{}` in download plan",
                    artifact.model_id
                ),
            });
        }
        hf_model_cache_folder(&artifact.repository)?;
        if !is_commit_hash(&artifact.revision) {
            return Err(ModelDownloadError::Validation {
                filename: artifact.filename.clone(),
                message: "download revision must be a full lowercase commit SHA".to_string(),
            });
        }
        let mut filenames = std::collections::BTreeSet::new();
        for file in std::iter::once(ArtifactFileDownloadSpec {
            filename: artifact.filename.clone(),
            byte_size: artifact.byte_size,
            sha256: artifact.sha256.clone(),
        })
        .chain(artifact.additional_files.clone())
        {
            ensure_cache_relative_path(&file.filename, "Hugging Face filename")?;
            if !file.filename.to_ascii_lowercase().ends_with(".gguf")
                || file.byte_size <= 4
                || (!file.sha256.is_empty() && !is_sha256(&file.sha256))
                || !filenames.insert(file.filename.clone())
            {
                return Err(ModelDownloadError::Validation {
                    filename: file.filename,
                    message: "download file must be a unique GGUF with valid size and optional lowercase SHA-256"
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

struct DownloadContext<'a> {
    client: &'a HFClient,
    job_id: u64,
    offline: bool,
    cancelled: &'a AtomicBool,
    shutdown: &'a AtomicBool,
    progress: &'a mpsc::SyncSender<ModelProgressEvent>,
}

impl DownloadContext<'_> {
    async fn file(
        &self,
        repository_id: &str,
        revision: &str,
        file: &ArtifactFileDownloadSpec,
    ) -> Result<(PathBuf, u64, String), ModelDownloadError> {
        let (owner, name) = split_id(repository_id);
        let repository = self.client.model(owner, name);
        let validated = if self.offline {
            let handler = DownloadProgressHandler {
                job_id: self.job_id,
                sender: self.progress.clone(),
                last_aggregate_percent: Mutex::new(None),
                last_file_percent: Mutex::new(std::collections::BTreeMap::new()),
            };
            let path = repository
                .download_file()
                .filename(file.filename.clone())
                .revision(revision.to_string())
                .local_files_only(true)
                .progress(handler)
                .send()
                .await
                .map_err(|error| map_hub_error(error, repository_id, revision, &file.filename))?;
            verify_downloaded_file(self.job_id, file, path, self.progress)?
        } else {
            self.download_file_resumable(&repository, repository_id, revision, file)
                .await?
        };
        if is_cancelled(self.cancelled, self.shutdown) {
            send_progress(
                self.progress,
                ModelProgressEvent::Cancelled {
                    job_id: self.job_id,
                },
            );
            return Err(ModelDownloadError::Cancelled);
        }
        Ok(validated)
    }
}

impl DownloadContext<'_> {
    async fn download_file_resumable(
        &self,
        repository: &HFRepository<RepoTypeModel>,
        repository_id: &str,
        revision: &str,
        file: &ArtifactFileDownloadSpec,
    ) -> Result<(PathBuf, u64, String), ModelDownloadError> {
        let repo_folder = hf_model_cache_folder(repository_id)?;
        ensure_cache_relative_path(&file.filename, "Hugging Face filename")?;
        let repo_root = self.client.cache_dir().join(&repo_folder);
        let snapshot = repo_root
            .join("snapshots")
            .join(revision)
            .join(&file.filename);
        if std::fs::symlink_metadata(&snapshot).is_ok()
            && let Ok(validated) =
                verify_downloaded_file(self.job_id, file, snapshot.clone(), self.progress)
        {
            return Ok(validated);
        }

        let metadata_future = repository
            .get_file_metadata()
            .filepath(file.filename.clone())
            .revision(revision)
            .send();
        tokio::pin!(metadata_future);
        let metadata = loop {
            tokio::select! {
                result = &mut metadata_future => {
                    break match result {
                        Ok(metadata) => metadata,
                        Err(error)
                            if is_commit_hash(revision)
                                && hf_error_missing_repo_commit(&error) =>
                        {
                            pinned_file_metadata(repository_id, revision, file)
                        }
                        Err(error) => {
                            return Err(map_hub_error(
                                error,
                                repository_id,
                                revision,
                                &file.filename,
                            ));
                        }
                    };
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if is_cancelled(self.cancelled, self.shutdown) {
                        return Err(ModelDownloadError::Cancelled);
                    }
                }
            }
        };
        if is_commit_hash(revision) && metadata.commit_hash != revision {
            return Err(ModelDownloadError::Validation {
                filename: file.filename.clone(),
                message: format!(
                    "Hugging Face resolved pinned revision {revision} to unexpected commit {}",
                    metadata.commit_hash
                ),
            });
        }
        if !is_commit_hash(&metadata.commit_hash) {
            return Err(ModelDownloadError::Validation {
                filename: file.filename.clone(),
                message: format!(
                    "Hugging Face returned invalid immutable revision {}",
                    metadata.commit_hash
                ),
            });
        }
        if file.byte_size > 0 && metadata.file_size > 0 && file.byte_size != metadata.file_size {
            return Err(ModelDownloadError::Validation {
                filename: file.filename.clone(),
                message: format!(
                    "Hub metadata size mismatch: expected {}, found {}",
                    file.byte_size, metadata.file_size
                ),
            });
        }
        let total_bytes = if metadata.file_size > 0 {
            metadata.file_size
        } else {
            file.byte_size
        };
        if total_bytes <= 4 {
            return Err(ModelDownloadError::Validation {
                filename: file.filename.clone(),
                message: "Hub metadata did not provide a valid GGUF size".to_string(),
            });
        }

        ensure_cache_component(&metadata.etag, "Hugging Face ETag")?;
        let blob = repo_root.join("blobs").join(&metadata.etag);
        let incomplete = repo_root
            .join("blobs")
            .join(format!("{}.incomplete", metadata.etag));
        let lock_path = self
            .client
            .cache_dir()
            .join(".locks")
            .join(&repo_folder)
            .join(format!("{}.lock", metadata.etag));
        let _lock = acquire_download_cache_lock(&lock_path, self.cancelled, self.shutdown).await?;

        if regular_file_size(&blob)? == Some(total_bytes) {
            match verify_downloaded_file(self.job_id, file, blob.clone(), self.progress) {
                Ok((_, byte_size, sha256)) => {
                    publish_snapshot_pointer(&blob, &snapshot).map_err(|error| {
                        ModelDownloadError::Hub {
                            repository: repository_id.to_string(),
                            filename: file.filename.clone(),
                            message: format!(
                                "failed to publish HF cache snapshot pointer: {error:#}"
                            ),
                        }
                    })?;
                    return Ok((snapshot, byte_size, sha256));
                }
                Err(ModelDownloadError::Validation { .. }) => {
                    remove_invalid_cache_file(&blob, repository_id, &file.filename)?;
                }
                Err(error) => return Err(error),
            }
        } else if blob.exists() {
            remove_invalid_cache_file(&blob, repository_id, &file.filename)?;
        }

        if regular_file_size(&incomplete)? == Some(total_bytes)
            && validate_gguf(
                &incomplete,
                Some(total_bytes),
                (!file.sha256.is_empty()).then_some(file.sha256.as_str()),
            )
            .is_err()
        {
            remove_invalid_cache_file(&incomplete, repository_id, &file.filename)?;
        }
        self.resume_blob_download(
            repository,
            repository_id,
            revision,
            file,
            total_bytes,
            &incomplete,
        )
        .await?;
        let (_, byte_size, sha256) =
            match verify_downloaded_file(self.job_id, file, incomplete.clone(), self.progress) {
                Ok(validated) => validated,
                Err(error) => {
                    let _ = remove_invalid_cache_file(&incomplete, repository_id, &file.filename);
                    return Err(error);
                }
            };
        std::fs::rename(&incomplete, &blob).map_err(|error| ModelDownloadError::Hub {
            repository: repository_id.to_string(),
            filename: file.filename.clone(),
            message: format!("failed to commit completed HF cache blob: {error}"),
        })?;
        publish_snapshot_pointer(&blob, &snapshot).map_err(|error| ModelDownloadError::Hub {
            repository: repository_id.to_string(),
            filename: file.filename.clone(),
            message: format!("failed to publish HF cache snapshot pointer: {error:#}"),
        })?;
        Ok((snapshot, byte_size, sha256))
    }

    async fn resume_blob_download(
        &self,
        repository: &HFRepository<RepoTypeModel>,
        repository_id: &str,
        revision: &str,
        file: &ArtifactFileDownloadSpec,
        total_bytes: u64,
        incomplete: &Path,
    ) -> Result<(), ModelDownloadError> {
        if let Some(parent) = incomplete.parent() {
            std::fs::create_dir_all(parent).map_err(|error| ModelDownloadError::Hub {
                repository: repository_id.to_string(),
                filename: file.filename.clone(),
                message: format!("failed to create HF cache blob directory: {error}"),
            })?;
        }
        if regular_file_size(incomplete)?.is_some_and(|size| size > total_bytes) {
            File::create(incomplete).map_err(|error| ModelDownloadError::Hub {
                repository: repository_id.to_string(),
                filename: file.filename.clone(),
                message: format!("failed to reset oversized partial download: {error}"),
            })?;
        }

        let mut attempt = 0usize;
        let mut reset_ignored_range = false;
        loop {
            if is_cancelled(self.cancelled, self.shutdown) {
                return Err(ModelDownloadError::Cancelled);
            }
            let offset = regular_file_size(incomplete)?.unwrap_or(0);
            if offset == total_bytes {
                return Ok(());
            }
            attempt += 1;
            let result = if offset > 0 {
                repository
                    .download_file_stream()
                    .filename(file.filename.clone())
                    .revision(revision.to_string())
                    .range(offset..total_bytes)
                    .send()
                    .await
            } else {
                repository
                    .download_file_stream()
                    .filename(file.filename.clone())
                    .revision(revision.to_string())
                    .send()
                    .await
            };
            let (content_length, mut stream) = match result {
                Ok(result) => result,
                Err(error)
                    if hf_error_is_transient(&error) && attempt < DOWNLOAD_RETRY_ATTEMPTS =>
                {
                    tokio::time::sleep(download_retry_delay(attempt)).await;
                    continue;
                }
                Err(error) => {
                    return Err(map_hub_error(
                        error,
                        repository_id,
                        revision,
                        &file.filename,
                    ));
                }
            };
            let remaining = total_bytes.saturating_sub(offset);
            if offset > 0 && content_length.is_some_and(|length| length != remaining) {
                if reset_ignored_range {
                    return Err(ModelDownloadError::Hub {
                        repository: repository_id.to_string(),
                        filename: file.filename.clone(),
                        message: "server did not honor the requested resume range".to_string(),
                    });
                }
                File::create(incomplete).map_err(|error| ModelDownloadError::Hub {
                    repository: repository_id.to_string(),
                    filename: file.filename.clone(),
                    message: format!("failed to restart a non-range download: {error}"),
                })?;
                reset_ignored_range = true;
                attempt = 0;
                continue;
            }

            let mut output = OpenOptions::new()
                .create(true)
                .append(true)
                .open(incomplete)
                .map_err(|error| ModelDownloadError::Hub {
                    repository: repository_id.to_string(),
                    filename: file.filename.clone(),
                    message: format!("failed to open partial HF cache blob: {error}"),
                })?;
            let mut completed = offset;
            let mut stream_error = None;
            loop {
                if is_cancelled(self.cancelled, self.shutdown) {
                    output.sync_all().ok();
                    return Err(ModelDownloadError::Cancelled);
                }
                tokio::select! {
                    chunk = stream.next() => match chunk {
                        Some(Ok(chunk)) => {
                            completed = completed.saturating_add(chunk.len() as u64);
                            if completed > total_bytes {
                                return Err(ModelDownloadError::Hub {
                                    repository: repository_id.to_string(),
                                    filename: file.filename.clone(),
                                    message: "download server returned more bytes than declared metadata".to_string(),
                                });
                            }
                            output.write_all(&chunk).map_err(|error| ModelDownloadError::Hub {
                                repository: repository_id.to_string(),
                                filename: file.filename.clone(),
                                message: format!("failed to write partial HF cache blob: {error}"),
                            })?;
                            send_progress(self.progress, ModelProgressEvent::File {
                                job_id: self.job_id,
                                filename: file.filename.clone(),
                                bytes_completed: completed,
                                total_bytes,
                                complete: completed == total_bytes,
                            });
                            if completed == total_bytes {
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            stream_error = Some(error);
                            break;
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if is_cancelled(self.cancelled, self.shutdown) {
                            output.sync_all().ok();
                            return Err(ModelDownloadError::Cancelled);
                        }
                    }
                }
            }
            output.sync_all().map_err(|error| ModelDownloadError::Hub {
                repository: repository_id.to_string(),
                filename: file.filename.clone(),
                message: format!("failed to sync partial HF cache blob: {error}"),
            })?;
            if completed == total_bytes {
                return Ok(());
            }
            if let Some(error) = stream_error
                && (!hf_error_is_transient(&error) || attempt >= DOWNLOAD_RETRY_ATTEMPTS)
            {
                return Err(map_hub_error(
                    error,
                    repository_id,
                    revision,
                    &file.filename,
                ));
            }
            if attempt >= DOWNLOAD_RETRY_ATTEMPTS {
                return Err(ModelDownloadError::Hub {
                    repository: repository_id.to_string(),
                    filename: file.filename.clone(),
                    message: format!(
                        "download ended early after {attempt} attempts ({completed}/{total_bytes} bytes)"
                    ),
                });
            }
            tokio::time::sleep(download_retry_delay(attempt)).await;
        }
    }
}

async fn acquire_download_cache_lock(
    path: &Path,
    cancelled: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<File, ModelDownloadError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| ModelDownloadError::CacheLockTimeout { path: path.into() })?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| ModelDownloadError::CacheLockTimeout { path: path.into() })?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(_)) => {
                return Err(ModelDownloadError::CacheLockTimeout { path: path.into() });
            }
        }
        if is_cancelled(cancelled, shutdown) {
            return Err(ModelDownloadError::Cancelled);
        }
        if started.elapsed() >= CACHE_LOCK_TIMEOUT {
            return Err(ModelDownloadError::CacheLockTimeout { path: path.into() });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn publish_snapshot_pointer(blob: &Path, snapshot: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(snapshot).is_ok() {
        if std::fs::canonicalize(snapshot).ok() == std::fs::canonicalize(blob).ok() {
            return Ok(());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "snapshot path already points at different cache data: {}",
                snapshot.display()
            ),
        ));
    }
    let parent = snapshot.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "HF snapshot path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let sequence = NEXT_CACHE_POINTER.fetch_add(1, Ordering::Relaxed);
    let filename = snapshot
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("model");
    let temporary = parent.join(format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    #[cfg(unix)]
    {
        let target = if blob.is_absolute() {
            blob.to_path_buf()
        } else {
            std::env::current_dir()?.join(blob)
        };
        std::os::unix::fs::symlink(target, &temporary)?;
    }
    #[cfg(windows)]
    {
        std::fs::copy(blob, &temporary)?;
    }
    if let Err(error) = std::fs::rename(&temporary, snapshot) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn is_commit_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn hf_model_cache_folder(repository: &str) -> Result<String, ModelDownloadError> {
    let parts = repository.split('/').collect::<Vec<_>>();
    if !matches!(parts.len(), 1 | 2)
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(ModelDownloadError::Hub {
            repository: repository.to_string(),
            filename: String::new(),
            message: "invalid Hugging Face repository id".to_string(),
        });
    }
    Ok(format!("models--{}", parts.join("--")))
}

fn ensure_cache_component(value: &str, label: &str) -> Result<(), ModelDownloadError> {
    if value.is_empty()
        || Path::new(value).components().count() != 1
        || !matches!(
            Path::new(value).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err(ModelDownloadError::Hub {
            repository: String::new(),
            filename: String::new(),
            message: format!("invalid {label}"),
        });
    }
    Ok(())
}

fn ensure_cache_relative_path(value: &str, label: &str) -> Result<(), ModelDownloadError> {
    if value.is_empty()
        || !Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(ModelDownloadError::Hub {
            repository: String::new(),
            filename: value.to_string(),
            message: format!("invalid {label}"),
        });
    }
    Ok(())
}

fn regular_file_size(path: &Path) -> Result<Option<u64>, ModelDownloadError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(metadata.len()))
        }
        Ok(_) => Err(ModelDownloadError::Hub {
            repository: String::new(),
            filename: path.display().to_string(),
            message: "HF cache blob path is not a regular file".to_string(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ModelDownloadError::Hub {
            repository: String::new(),
            filename: path.display().to_string(),
            message: error.to_string(),
        }),
    }
}

fn verify_downloaded_file(
    job_id: u64,
    file: &ArtifactFileDownloadSpec,
    path: PathBuf,
    progress: &mpsc::SyncSender<ModelProgressEvent>,
) -> Result<(PathBuf, u64, String), ModelDownloadError> {
    send_progress(
        progress,
        ModelProgressEvent::Verifying {
            job_id,
            filename: file.filename.clone(),
        },
    );
    let expected_size = (file.byte_size > 0).then_some(file.byte_size);
    let expected_sha256 = (!file.sha256.is_empty()).then_some(file.sha256.as_str());
    let (byte_size, sha256) =
        validate_gguf(&path, expected_size, expected_sha256).map_err(|error| {
            ModelDownloadError::Validation {
                filename: file.filename.clone(),
                message: format!("{error:#}"),
            }
        })?;
    Ok((path, byte_size, sha256))
}

fn remove_invalid_cache_file(
    path: &Path,
    repository: &str,
    filename: &str,
) -> Result<(), ModelDownloadError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ModelDownloadError::Hub {
            repository: repository.to_string(),
            filename: filename.to_string(),
            message: format!(
                "failed to replace invalid Hugging Face cache file {}: {error}",
                path.display()
            ),
        }),
    }
}

fn download_retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(100_u64.saturating_mul(1_u64 << attempt.min(5)))
}

fn hf_error_is_transient(error: &HFError) -> bool {
    match error {
        HFError::Request { source, .. } => source.is_timeout() || source.is_connect(),
        HFError::Http { context } => matches!(context.status.as_u16(), 429 | 500 | 502 | 503 | 504),
        HFError::Xet { .. } => true,
        _ => false,
    }
}

fn hf_error_missing_repo_commit(error: &HFError) -> bool {
    matches!(
        error,
        HFError::MalformedResponse { what, .. }
            if what.starts_with("missing X-Repo-Commit header")
    )
}

fn pinned_file_metadata(
    repository: &str,
    revision: &str,
    file: &ArtifactFileDownloadSpec,
) -> FileMetadataInfo {
    let etag = if is_sha256(&file.sha256) {
        file.sha256.clone()
    } else {
        let mut hasher = Sha256::new();
        hasher.update(repository.as_bytes());
        hasher.update([0]);
        hasher.update(revision.as_bytes());
        hasher.update([0]);
        hasher.update(file.filename.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    FileMetadataInfo {
        filename: file.filename.clone(),
        etag,
        commit_hash: revision.to_string(),
        xet_hash: None,
        file_size: file.byte_size,
        location: None,
    }
}

fn is_cancelled(cancelled: &AtomicBool, shutdown: &AtomicBool) -> bool {
    cancelled.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
}

fn map_inspection_error(
    error: HFError,
    repository: &str,
    revision: &str,
    filename: Option<&str>,
) -> ModelDownloadError {
    map_hub_error(error, repository, revision, filename.unwrap_or(""))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn send_progress(sender: &mpsc::SyncSender<ModelProgressEvent>, event: ModelProgressEvent) {
    let _ = sender.try_send(event);
}

struct DownloadProgressHandler {
    job_id: u64,
    sender: mpsc::SyncSender<ModelProgressEvent>,
    last_aggregate_percent: Mutex<Option<u64>>,
    last_file_percent: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl ProgressHandler for DownloadProgressHandler {
    fn on_progress(&self, event: &ProgressEvent) {
        let ProgressEvent::Download(event) = event else {
            return;
        };
        match event {
            DownloadEvent::Progress { files } => {
                for file in files {
                    let percent = progress_percent(file.bytes_completed, file.total_bytes);
                    let should_send = self
                        .last_file_percent
                        .lock()
                        .map(|mut last| {
                            last.insert(file.filename.clone(), percent) != Some(percent)
                        })
                        .unwrap_or(true);
                    if !should_send {
                        continue;
                    }
                    send_progress(
                        &self.sender,
                        ModelProgressEvent::File {
                            job_id: self.job_id,
                            filename: file.filename.clone(),
                            bytes_completed: file.bytes_completed,
                            total_bytes: file.total_bytes,
                            complete: file.status == FileStatus::Complete,
                        },
                    );
                }
            }
            DownloadEvent::AggregateProgress {
                bytes_completed,
                total_bytes,
                ..
            } => {
                let percent = progress_percent(*bytes_completed, *total_bytes);
                let should_send = self
                    .last_aggregate_percent
                    .lock()
                    .map(|mut last| {
                        if *last == Some(percent) {
                            false
                        } else {
                            *last = Some(percent);
                            true
                        }
                    })
                    .unwrap_or(true);
                if should_send {
                    send_progress(
                        &self.sender,
                        ModelProgressEvent::Aggregate {
                            job_id: self.job_id,
                            bytes_completed: *bytes_completed,
                            total_bytes: *total_bytes,
                        },
                    );
                }
            }
            DownloadEvent::Start { .. } | DownloadEvent::Complete => {}
        }
    }
}

fn progress_percent(completed: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        completed.saturating_mul(100).saturating_div(total).min(100)
    }
}

fn map_hub_error(
    error: HFError,
    repository: &str,
    revision: &str,
    filename: &str,
) -> ModelDownloadError {
    match error {
        HFError::AuthRequired { .. } => ModelDownloadError::AuthRequired {
            repository: repository.to_string(),
        },
        HFError::Forbidden { .. } => ModelDownloadError::Forbidden {
            repository: repository.to_string(),
        },
        HFError::RepoNotFound { .. } => ModelDownloadError::RepositoryNotFound {
            repository: repository.to_string(),
        },
        HFError::RevisionNotFound { .. } => ModelDownloadError::RevisionNotFound {
            repository: repository.to_string(),
            revision: revision.to_string(),
        },
        HFError::EntryNotFound { .. } => ModelDownloadError::FileNotFound {
            repository: repository.to_string(),
            filename: filename.to_string(),
        },
        HFError::RateLimited { .. } => ModelDownloadError::RateLimited {
            repository: repository.to_string(),
        },
        HFError::LocalEntryNotFound { .. } => ModelDownloadError::OfflineCacheMiss {
            repository: repository.to_string(),
            revision: revision.to_string(),
            filename: filename.to_string(),
        },
        HFError::CacheLockTimeout { path } => ModelDownloadError::CacheLockTimeout { path },
        other => ModelDownloadError::Hub {
            repository: repository.to_string(),
            filename: filename.to_string(),
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;

    use sha2::{Digest, Sha256};

    use super::*;

    static TEST_ROOT: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        method: String,
        range: Option<String>,
        authorization: Option<String>,
    }

    #[derive(Clone, Debug)]
    struct TestHubResponse {
        revision: String,
        required_token: Option<String>,
        include_commit_header: bool,
    }

    struct TestHub {
        endpoint: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        stop: Arc<AtomicBool>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl TestHub {
        fn start(
            body: Vec<u8>,
            revision: String,
            required_token: Option<&str>,
            slow_first_get: bool,
            failed_gets: usize,
        ) -> Self {
            Self::start_with_commit_header(
                body,
                revision,
                required_token,
                slow_first_get,
                failed_gets,
                true,
            )
        }

        fn start_without_repo_commit(body: Vec<u8>, revision: String) -> Self {
            Self::start_with_commit_header(body, revision, None, false, 0, false)
        }

        fn start_with_commit_header(
            body: Vec<u8>,
            revision: String,
            required_token: Option<&str>,
            slow_first_get: bool,
            failed_gets: usize,
            include_commit_header: bool,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let worker_requests = Arc::clone(&requests);
            let worker_stop = Arc::clone(&stop);
            let response = TestHubResponse {
                revision,
                required_token: required_token.map(str::to_string),
                include_commit_header,
            };
            let body = Arc::new(body);
            let slow = Arc::new(AtomicBool::new(slow_first_get));
            let failures = Arc::new(AtomicUsize::new(failed_gets));
            let join = thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let body = Arc::clone(&body);
                            let requests = Arc::clone(&worker_requests);
                            let response = response.clone();
                            let slow = Arc::clone(&slow);
                            let failures = Arc::clone(&failures);
                            thread::spawn(move || {
                                serve_test_hub_connection(
                                    stream, &body, &response, &requests, &slow, &failures,
                                );
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                endpoint: format!("http://{address}"),
                requests,
                stop,
                join: Some(join),
            }
        }
    }

    impl Drop for TestHub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.endpoint.trim_start_matches("http://"));
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    fn serve_test_hub_connection(
        mut stream: TcpStream,
        body: &[u8],
        response: &TestHubResponse,
        requests: &Mutex<Vec<RecordedRequest>>,
        slow_first_get: &AtomicBool,
        failed_gets: &AtomicUsize,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut raw = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
            let Ok(read) = stream.read(&mut buffer) else {
                return;
            };
            if read == 0 {
                return;
            }
            raw.extend_from_slice(&buffer[..read]);
            if raw.len() > 64 * 1024 {
                return;
            }
        }
        let request = String::from_utf8_lossy(&raw);
        let mut lines = request.lines();
        let method = lines
            .next()
            .and_then(|line| line.split_whitespace().next())
            .unwrap_or("")
            .to_string();
        let header = |name: &str| {
            request.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_string())
            })
        };
        let range = header("range");
        let authorization = header("authorization");
        requests.lock().unwrap().push(RecordedRequest {
            method: method.clone(),
            range: range.clone(),
            authorization: authorization.clone(),
        });

        if response.required_token.as_deref().is_some_and(|token| {
            authorization.as_deref() != Some(format!("Bearer {token}").as_str())
        }) {
            let response =
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        if method == "HEAD" {
            let commit_header = if response.include_commit_header {
                format!("X-Repo-Commit: {}\r\n", response.revision)
            } else {
                String::new()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"test-etag\"\r\n{commit_header}Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        if method != "GET" {
            return;
        }
        if failed_gets
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
        {
            let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            return;
        }

        let start = range
            .as_deref()
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, _)| start.parse::<usize>().ok())
            .unwrap_or(0)
            .min(body.len());
        let response_body = &body[start..];
        let status = if range.is_some() {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let content_range = range.is_some().then(|| {
            format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                body.len().saturating_sub(1),
                body.len()
            )
        });
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
            response_body.len(),
            content_range.as_deref().unwrap_or("")
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        let slow = slow_first_get.swap(false, Ordering::AcqRel);
        for chunk in response_body.chunks(4096) {
            if stream.write_all(chunk).is_err() || stream.flush().is_err() {
                return;
            }
            if slow {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-model-worker-{name}-{}-{}",
            std::process::id(),
            TEST_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_download_request(body: &[u8], revision: &str, offline: bool) -> ModelDownloadRequest {
        let digest = Sha256::digest(body);
        let sha256 = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        ModelDownloadRequest {
            artifacts: vec![ArtifactDownloadSpec {
                package_id: None,
                model_id: ModelId::new("test-model").unwrap(),
                role: ModelArtifactRole::Main,
                repository: "owner/repo".to_string(),
                revision: revision.to_string(),
                filename: "model.gguf".to_string(),
                byte_size: body.len() as u64,
                sha256,
                additional_files: Vec::new(),
            }],
            offline,
        }
    }

    #[test]
    fn package_request_contains_all_required_artifacts() {
        let catalog = crate::ModelCatalog::builtin().unwrap();
        let request = ModelDownloadRequest::for_package(catalog.default_package(), true);
        assert_eq!(request.artifacts.len(), 2);
        assert!(request.offline);
    }

    #[test]
    fn empty_worker_plan_is_rejected_before_admission() {
        let worker = ModelDownloadWorker::spawn().unwrap();
        let error = worker
            .handle()
            .submit(ModelDownloadRequest {
                artifacts: Vec::new(),
                offline: true,
            })
            .unwrap_err();
        assert!(matches!(error, ModelDownloadError::EmptyPlan));

        let invalid = ModelDownloadRequest {
            artifacts: vec![ArtifactDownloadSpec {
                package_id: None,
                model_id: ModelId::new("invalid").unwrap(),
                role: ModelArtifactRole::Main,
                repository: "owner/repo".to_string(),
                revision: "main".to_string(),
                filename: "model.gguf".to_string(),
                byte_size: 10,
                sha256: String::new(),
                additional_files: Vec::new(),
            }],
            offline: true,
        };
        assert!(matches!(
            worker.handle().submit(invalid),
            Err(ModelDownloadError::Validation { .. })
        ));
    }

    #[test]
    fn pinned_download_tolerates_head_without_repo_commit_header() {
        let revision = "f".repeat(40);
        let body = b"GGUFpinned-without-repo-commit-header".to_vec();
        let hub = TestHub::start_without_repo_commit(body.clone(), revision.clone());
        let cache = test_root("missing-repo-commit").join("hub");
        let worker = ModelDownloadWorker::spawn_with_client_config(WorkerClientConfig {
            cache_dir: Some(cache.clone()),
            endpoint: Some(hub.endpoint.clone()),
            token: None,
            retry_max_attempts: Some(1),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        })
        .unwrap();
        let request = test_download_request(&body, &revision, false);
        let expected_etag = request.artifacts[0].sha256.clone();

        let result = worker.handle().submit(request).unwrap().wait().unwrap();

        assert_eq!(std::fs::read(&result.artifacts[0].path).unwrap(), body);
        assert!(
            cache
                .join("models--owner--repo/blobs")
                .join(expected_etag)
                .is_file()
        );
        assert!(
            hub.requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.method == "HEAD")
        );
        worker.shutdown().unwrap();
    }

    #[test]
    fn pinned_download_reuses_valid_snapshot_with_a_different_blob_key() {
        let revision = "9".repeat(40);
        let body = b"GGUFexisting-pinned-snapshot".to_vec();
        let hub = TestHub::start_without_repo_commit(body.clone(), revision.clone());
        let cache = test_root("existing-snapshot").join("hub");
        let repo_root = cache.join("models--owner--repo");
        let blob = repo_root.join("blobs/original-hf-etag");
        let snapshot = repo_root
            .join("snapshots")
            .join(&revision)
            .join("model.gguf");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, &body).unwrap();
        publish_snapshot_pointer(&blob, &snapshot).unwrap();
        let worker = ModelDownloadWorker::spawn_with_client_config(WorkerClientConfig {
            cache_dir: Some(cache),
            endpoint: Some(hub.endpoint.clone()),
            token: None,
            retry_max_attempts: Some(1),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        })
        .unwrap();
        let mut request = test_download_request(&body, &revision, false);
        request.artifacts[0].sha256.clear();

        let result = worker.handle().submit(request).unwrap().wait().unwrap();

        assert_eq!(result.artifacts[0].path, snapshot);
        assert_eq!(std::fs::read(&result.artifacts[0].path).unwrap(), body);
        assert!(hub.requests.lock().unwrap().is_empty());
        worker.shutdown().unwrap();
    }

    #[test]
    fn split_gguf_candidates_are_one_complete_ordered_group() {
        let candidate = |filename: &str| HubFileCandidate {
            repository: "owner/repo".to_string(),
            revision: "a".repeat(40),
            filename: filename.to_string(),
            byte_size: 10,
            sha256: Some("b".repeat(64)),
            cached_path: None,
        };
        let inspection = HubInspection {
            repository: "owner/repo".to_string(),
            revision: "a".repeat(40),
            candidates: vec![
                candidate("model-00002-of-00003.gguf"),
                candidate("other.gguf"),
                candidate("model-00001-of-00003.gguf"),
                candidate("model-00003-of-00003.gguf"),
            ],
        };
        let groups = inspection.candidate_groups().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 3);
        assert_eq!(groups[0][0].filename, "model-00001-of-00003.gguf");
        assert_eq!(groups[0][2].filename, "model-00003-of-00003.gguf");
        assert_eq!(groups[1][0].filename, "other.gguf");
    }

    #[test]
    fn incomplete_split_gguf_is_rejected() {
        let inspection = HubInspection {
            repository: "owner/repo".to_string(),
            revision: "a".repeat(40),
            candidates: vec![HubFileCandidate {
                repository: "owner/repo".to_string(),
                revision: "a".repeat(40),
                filename: "model-00001-of-00002.gguf".to_string(),
                byte_size: 10,
                sha256: None,
                cached_path: None,
            }],
        };
        assert!(matches!(
            inspection.candidate_groups(),
            Err(ModelDownloadError::IncompleteSplit {
                expected_shards: 2,
                found_shards: 1,
                ..
            })
        ));
    }

    #[test]
    fn cancelled_download_resumes_by_range_and_then_reuses_cache_offline() {
        let revision = "a".repeat(40);
        let mut body = b"GGUF".to_vec();
        body.extend((0..512 * 1024).map(|index| (index % 251) as u8));
        let hub = TestHub::start(body.clone(), revision.clone(), Some("test-token"), true, 0);
        let cache = test_root("resume").join("hub");
        let worker = ModelDownloadWorker::spawn_with_client_config(WorkerClientConfig {
            cache_dir: Some(cache.clone()),
            endpoint: Some(hub.endpoint.clone()),
            token: Some("test-token".to_string()),
            retry_max_attempts: Some(1),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        })
        .unwrap();
        let handle = worker.handle();
        let request = test_download_request(&body, &revision, false);
        let job = handle.submit(request.clone()).unwrap();
        loop {
            if let Some(ModelProgressEvent::File {
                bytes_completed, ..
            }) = job.progress_timeout(Duration::from_secs(2))
                && bytes_completed > 0
            {
                job.cancel();
                break;
            }
        }
        assert!(matches!(job.wait(), Err(ModelDownloadError::Cancelled)));
        let incomplete = cache.join("models--owner--repo/blobs/test-etag.incomplete");
        let partial_bytes = std::fs::metadata(&incomplete).unwrap().len();
        assert!(partial_bytes > 0 && partial_bytes < body.len() as u64);

        let resumed = handle.submit(request).unwrap().wait().unwrap();
        assert_eq!(std::fs::read(&resumed.artifacts[0].path).unwrap(), body);
        let requests_after_resume = hub.requests.lock().unwrap().len();
        assert!(hub.requests.lock().unwrap().iter().any(|request| {
            request
                .range
                .as_deref()
                .is_some_and(|range| range.starts_with(&format!("bytes={partial_bytes}-")))
        }));
        assert!(
            hub.requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| { request.authorization.as_deref() == Some("Bearer test-token") })
        );

        let offline = test_download_request(&body, &revision, true);
        handle.submit(offline).unwrap().wait().unwrap();
        assert_eq!(hub.requests.lock().unwrap().len(), requests_after_resume);
        worker.shutdown().unwrap();
    }

    #[test]
    fn online_acquisition_repairs_a_same_size_corrupt_cache_blob() {
        let revision = "e".repeat(40);
        let body = b"GGUFknown-good-payload".to_vec();
        let hub = TestHub::start(body.clone(), revision.clone(), None, false, 0);
        let cache = test_root("repair-corrupt").join("hub");
        let worker = ModelDownloadWorker::spawn_with_client_config(WorkerClientConfig {
            cache_dir: Some(cache.clone()),
            endpoint: Some(hub.endpoint.clone()),
            token: None,
            retry_max_attempts: Some(1),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        })
        .unwrap();
        let request = test_download_request(&body, &revision, false);
        worker
            .handle()
            .submit(request.clone())
            .unwrap()
            .wait()
            .unwrap();
        let blob = cache.join("models--owner--repo/blobs/test-etag");
        let mut corrupt = b"GGUF".to_vec();
        corrupt.resize(body.len(), b'x');
        std::fs::write(&blob, corrupt).unwrap();
        let gets_before_repair = hub
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == "GET")
            .count();

        let repaired = worker.handle().submit(request).unwrap().wait().unwrap();
        assert_eq!(std::fs::read(&repaired.artifacts[0].path).unwrap(), body);
        let gets_after_repair = hub
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == "GET")
            .count();
        assert_eq!(gets_after_repair, gets_before_repair + 1);
        worker.shutdown().unwrap();
    }

    #[test]
    fn worker_maps_standard_hf_auth_and_retries_transient_gets() {
        let revision = "b".repeat(40);
        let body = b"GGUFretry-payload".to_vec();
        let auth_hub = TestHub::start(
            body.clone(),
            revision.clone(),
            Some("correct-token"),
            false,
            0,
        );
        let auth_worker = ModelDownloadWorker::spawn_with_client_config(WorkerClientConfig {
            cache_dir: Some(test_root("auth").join("hub")),
            endpoint: Some(auth_hub.endpoint.clone()),
            token: Some("wrong-token".to_string()),
            retry_max_attempts: Some(0),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        })
        .unwrap();
        assert!(matches!(
            auth_worker
                .handle()
                .submit(test_download_request(&body, &revision, false))
                .unwrap()
                .wait(),
            Err(ModelDownloadError::AuthRequired { .. })
        ));
        auth_worker.shutdown().unwrap();

        let retry_hub = TestHub::start(body.clone(), revision.clone(), None, false, 1);
        let retry_worker = ModelDownloadWorker::spawn_with_client_config(WorkerClientConfig {
            cache_dir: Some(test_root("retry").join("hub")),
            endpoint: Some(retry_hub.endpoint.clone()),
            token: Some("test-token".to_string()),
            retry_max_attempts: Some(2),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        })
        .unwrap();
        retry_worker
            .handle()
            .submit(test_download_request(&body, &revision, false))
            .unwrap()
            .wait()
            .unwrap();
        assert!(
            retry_hub
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method == "GET")
                .count()
                >= 2
        );
        retry_worker.shutdown().unwrap();
    }

    #[test]
    fn concurrent_workers_share_one_blob_download_under_the_hf_lock() {
        let revision = "c".repeat(40);
        let mut body = b"GGUF".to_vec();
        body.extend((0..256 * 1024).map(|index| (index % 239) as u8));
        let hub = TestHub::start(body.clone(), revision.clone(), None, true, 0);
        let cache = test_root("concurrent").join("hub");
        let config = WorkerClientConfig {
            cache_dir: Some(cache),
            endpoint: Some(hub.endpoint.clone()),
            token: Some("test-token".to_string()),
            retry_max_attempts: Some(1),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        };
        let first = ModelDownloadWorker::spawn_with_client_config(config.clone()).unwrap();
        let second = ModelDownloadWorker::spawn_with_client_config(config).unwrap();
        let request = test_download_request(&body, &revision, false);
        let first_job = first.handle().submit(request.clone()).unwrap();
        let second_job = second.handle().submit(request).unwrap();

        let first_result = first_job.wait().unwrap();
        let second_result = second_job.wait().unwrap();
        assert_eq!(
            first_result.artifacts[0].path,
            second_result.artifacts[0].path
        );
        assert_eq!(
            hub.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method == "GET")
                .count(),
            1
        );
        first.shutdown().unwrap();
        second.shutdown().unwrap();
    }

    #[test]
    fn worker_shutdown_cancels_transfer_and_releases_the_cache_lock() {
        let revision = "d".repeat(40);
        let mut body = b"GGUF".to_vec();
        body.extend((0..256 * 1024).map(|index| (index % 233) as u8));
        let hub = TestHub::start(body.clone(), revision.clone(), None, true, 0);
        let cache = test_root("shutdown").join("hub");
        let config = WorkerClientConfig {
            cache_dir: Some(cache.clone()),
            endpoint: Some(hub.endpoint.clone()),
            token: Some("test-token".to_string()),
            retry_max_attempts: Some(1),
            retry_base_delay: Some(Duration::from_millis(1)),
            offline: false,
        };
        let worker = ModelDownloadWorker::spawn_with_client_config(config.clone()).unwrap();
        let job = worker
            .handle()
            .submit(test_download_request(&body, &revision, false))
            .unwrap();
        loop {
            if matches!(
                job.progress_timeout(Duration::from_secs(2)),
                Some(ModelProgressEvent::File {
                    bytes_completed,
                    ..
                }) if bytes_completed > 0
            ) {
                break;
            }
        }
        worker.shutdown().unwrap();
        assert!(matches!(job.wait(), Err(ModelDownloadError::Cancelled)));

        let lock_path = cache.join(".locks/models--owner--repo/test-etag.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        assert!(lock.try_lock().is_ok());
        drop(lock);

        let resumed = ModelDownloadWorker::spawn_with_client_config(config).unwrap();
        resumed
            .handle()
            .submit(test_download_request(&body, &revision, false))
            .unwrap()
            .wait()
            .unwrap();
        resumed.shutdown().unwrap();
    }
}
