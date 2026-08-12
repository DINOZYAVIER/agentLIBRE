use super::*;

pub(super) fn is_commit_hash(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn hf_model_cache_folder(repository: &str) -> Result<String, ModelDownloadError> {
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

pub(super) fn ensure_cache_component(value: &str, label: &str) -> Result<(), ModelDownloadError> {
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

pub(super) fn ensure_cache_relative_path(
    value: &str,
    label: &str,
) -> Result<(), ModelDownloadError> {
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

pub(super) fn regular_file_size(path: &Path) -> Result<Option<u64>, ModelDownloadError> {
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

pub(super) fn verify_downloaded_file(
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

pub(super) fn remove_invalid_cache_file(
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

pub(super) fn download_retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(100_u64.saturating_mul(1_u64 << attempt.min(5)))
}

pub(super) fn hf_error_is_transient(error: &HFError) -> bool {
    match error {
        HFError::Request { source, .. } => source.is_timeout() || source.is_connect(),
        HFError::Http { context } => matches!(context.status.as_u16(), 429 | 500 | 502 | 503 | 504),
        HFError::Xet { .. } => true,
        _ => false,
    }
}

pub(super) fn hf_error_missing_repo_commit(error: &HFError) -> bool {
    matches!(
        error,
        HFError::MalformedResponse { what, .. }
            if what.starts_with("missing X-Repo-Commit header")
    )
}

pub(super) fn pinned_file_metadata(
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

pub(super) fn is_cancelled(cancelled: &AtomicBool, shutdown: &AtomicBool) -> bool {
    cancelled.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
}

pub(super) fn map_inspection_error(
    error: HFError,
    repository: &str,
    revision: &str,
    filename: Option<&str>,
) -> ModelDownloadError {
    map_hub_error(error, repository, revision, filename.unwrap_or(""))
}

pub(super) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn send_progress(
    sender: &mpsc::SyncSender<ModelProgressEvent>,
    event: ModelProgressEvent,
) {
    let _ = sender.try_send(event);
}

pub(super) struct DownloadProgressHandler {
    pub(super) job_id: u64,
    pub(super) sender: mpsc::SyncSender<ModelProgressEvent>,
    pub(super) last_aggregate_percent: Mutex<Option<u64>>,
    pub(super) last_file_percent: Mutex<std::collections::BTreeMap<String, u64>>,
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

pub(super) fn progress_percent(completed: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        completed.saturating_mul(100).saturating_div(total).min(100)
    }
}

pub(super) fn map_hub_error(
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
