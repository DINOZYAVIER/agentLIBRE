use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::Component;
use std::path::{Path, PathBuf};

use agl_config::{
    ModelId, load_model_bindings_or_empty, model_bindings_path, write_model_bindings,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{InstallRecordState, InstallSource, ModelInstallStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelLifecycleOperation {
    Unbind,
    Remove,
    Prune,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLifecycleTarget {
    pub model_id: ModelId,
    pub binding_path: Option<PathBuf>,
    pub install_record_path: Option<PathBuf>,
    pub cache_path: Option<PathBuf>,
    pub additional_cache_paths: Vec<PathBuf>,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPruneEntry {
    pub model_id: ModelId,
    pub repository: String,
    pub revision: String,
    pub cache_path: PathBuf,
    pub additional_cache_paths: Vec<PathBuf>,
    pub blobs: Vec<ModelPruneBlob>,
    pub reclaimable_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPruneBlob {
    pub path: PathBuf,
    pub lock_path: PathBuf,
    pub reclaimable_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLifecyclePlan {
    pub operation: ModelLifecycleOperation,
    pub targets: Vec<ModelLifecycleTarget>,
    pub prune_entries: Vec<ModelPruneEntry>,
    pub total_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct ModelLifecycleService {
    store: ModelInstallStore,
    bindings_path: PathBuf,
    hf_cache_dir: PathBuf,
    lease_root: Option<PathBuf>,
    protected_ids: BTreeSet<ModelId>,
}

impl ModelLifecycleService {
    pub fn new(store: ModelInstallStore, config_dir: impl AsRef<Path>) -> Self {
        Self {
            store,
            bindings_path: model_bindings_path(config_dir),
            hf_cache_dir: crate::hugging_face_cache_dir(),
            lease_root: None,
            protected_ids: BTreeSet::new(),
        }
    }

    pub fn with_hf_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.hf_cache_dir = path.into();
        self
    }

    pub fn with_lease_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.lease_root = Some(path.into());
        self
    }

    pub fn with_protected_ids(mut self, ids: impl IntoIterator<Item = ModelId>) -> Self {
        self.protected_ids.extend(ids);
        self
    }

    pub fn plan_unbind(&self, id: &ModelId) -> Result<ModelLifecyclePlan> {
        self.ensure_not_protected(id)?;
        let bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        let binding = bindings
            .models
            .get(id)
            .with_context(|| format!("model `{id}` is not bound"))?;
        Ok(ModelLifecyclePlan {
            operation: ModelLifecycleOperation::Unbind,
            targets: vec![ModelLifecycleTarget {
                model_id: id.clone(),
                binding_path: Some(binding.path.clone()),
                install_record_path: None,
                cache_path: None,
                additional_cache_paths: Vec::new(),
                bytes: 0,
            }],
            prune_entries: Vec::new(),
            total_bytes: 0,
        })
    }

    pub fn execute_unbind(&self, plan: &ModelLifecyclePlan) -> Result<()> {
        ensure!(
            plan.operation == ModelLifecycleOperation::Unbind && plan.targets.len() == 1,
            "invalid unbind plan"
        );
        let target = &plan.targets[0];
        self.ensure_not_protected(&target.model_id)?;
        let mut bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        let current = bindings
            .models
            .get(&target.model_id)
            .with_context(|| format!("model `{}` is no longer bound", target.model_id))?;
        ensure!(
            Some(&current.path) == target.binding_path.as_ref(),
            "model binding changed after the unbind plan was created"
        );
        bindings.models.remove(&target.model_id);
        write_model_bindings(&self.bindings_path, &bindings)
    }

    pub fn plan_remove(&self, id: &ModelId) -> Result<ModelLifecyclePlan> {
        self.ensure_not_protected(id)?;
        let bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        ensure!(
            !bindings.models.contains_key(id),
            "model `{id}` is still bound; unbind it first"
        );
        let record = self
            .store
            .get(id)?
            .with_context(|| format!("model `{id}` has no agentLIBRE install record"))?;
        ensure!(
            record.state == InstallRecordState::Active,
            "model `{id}` is already removed"
        );
        let additional_cache_paths = record
            .additional_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let total_bytes = record.byte_size
            + record
                .additional_files
                .iter()
                .map(|file| file.byte_size)
                .sum::<u64>();
        Ok(ModelLifecyclePlan {
            operation: ModelLifecycleOperation::Remove,
            targets: vec![ModelLifecycleTarget {
                model_id: id.clone(),
                binding_path: None,
                install_record_path: Some(self.store.record_path(id)),
                cache_path: Some(record.path),
                additional_cache_paths,
                bytes: total_bytes,
            }],
            prune_entries: Vec::new(),
            total_bytes,
        })
    }

    pub fn execute_remove(&self, plan: &ModelLifecyclePlan) -> Result<()> {
        ensure!(
            plan.operation == ModelLifecycleOperation::Remove && plan.targets.len() == 1,
            "invalid remove plan"
        );
        let target = &plan.targets[0];
        self.ensure_not_protected(&target.model_id)?;
        let bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        ensure!(
            !bindings.models.contains_key(&target.model_id),
            "model `{}` became bound after the remove plan was created",
            target.model_id
        );
        let record = self
            .store
            .get(&target.model_id)?
            .context("install record disappeared after remove planning")?;
        ensure!(
            record.state == InstallRecordState::Active
                && Some(&record.path) == target.cache_path.as_ref(),
            "install record changed after the remove plan was created"
        );
        self.store.mark_removed(&target.model_id)?;
        Ok(())
    }

    pub fn plan_prune(&self) -> Result<ModelLifecyclePlan> {
        let bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        let records = self.store.list()?;
        let leased_paths = active_model_lease_paths(self.lease_root.as_deref())?;
        let referenced_paths = bindings
            .models
            .values()
            .map(|binding| binding.path.clone())
            .chain(
                records
                    .iter()
                    .filter(|record| record.state == InstallRecordState::Active)
                    .flat_map(record_paths),
            )
            .chain(leased_paths)
            .collect::<BTreeSet<_>>();
        let candidate_paths = records
            .iter()
            .filter(|record| record.state == InstallRecordState::Removed)
            .filter(|record| matches!(record.source, InstallSource::HuggingFace { .. }))
            .flat_map(record_paths)
            .collect::<BTreeSet<_>>();

        let mut drafts = Vec::new();
        let mut unique_blobs = BTreeMap::<PathBuf, HfBlobCandidate>::new();
        for record in records {
            if record.state != InstallRecordState::Removed {
                continue;
            }
            self.ensure_not_protected(&record.model_id)?;
            ensure!(
                !bindings.models.contains_key(&record.model_id),
                "removed model `{}` became bound and cannot be pruned",
                record.model_id
            );
            let InstallSource::HuggingFace {
                repository,
                revision,
                filename,
            } = &record.source
            else {
                continue;
            };
            let mut pointer_specs = vec![(record.path.clone(), filename.clone())];
            pointer_specs.extend(
                record
                    .additional_files
                    .iter()
                    .map(|file| (file.path.clone(), file.filename.clone())),
            );
            let pointers = pointer_specs
                .iter()
                .map(|(path, filename)| {
                    ensure!(
                        !referenced_paths.contains(path),
                        "removed model `{}` still points at cache data referenced by an active binding, install record, or model lease: {}",
                        record.model_id,
                        path.display()
                    );
                    inspect_hf_pointer(
                        &self.hf_cache_dir,
                        repository,
                        revision,
                        filename,
                        path,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            for pointer in &pointers {
                if let Some(blob) = &pointer.blob {
                    unique_blobs
                        .entry(blob.path.clone())
                        .or_insert_with(|| blob.clone());
                }
            }
            let additional_cache_paths = record
                .additional_files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>();
            drafts.push(PruneDraft {
                model_id: record.model_id,
                repository: repository.clone(),
                revision: revision.clone(),
                cache_path: record.path,
                additional_cache_paths,
                pointers,
            });
        }

        for blob in unique_blobs.values_mut() {
            let references = snapshot_references(&blob.repo_root, &blob.path)?;
            blob.reclaimable_bytes = if references.iter().all(|path| candidate_paths.contains(path))
            {
                regular_file_len(&blob.path)?
            } else {
                0
            };
        }

        let mut unassigned_blobs = unique_blobs;
        let mut targets = Vec::new();
        let mut prune_entries = Vec::new();
        for draft in drafts {
            let mut blobs = Vec::new();
            for pointer in &draft.pointers {
                let Some(blob) = &pointer.blob else {
                    continue;
                };
                if let Some(blob) = unassigned_blobs.remove(&blob.path) {
                    blobs.push(ModelPruneBlob {
                        path: blob.path,
                        lock_path: blob.lock_path,
                        reclaimable_bytes: blob.reclaimable_bytes,
                    });
                }
            }
            let reclaimable_bytes = draft
                .pointers
                .iter()
                .map(|pointer| pointer.snapshot_bytes)
                .sum::<u64>()
                + blobs.iter().map(|blob| blob.reclaimable_bytes).sum::<u64>();
            targets.push(ModelLifecycleTarget {
                model_id: draft.model_id.clone(),
                binding_path: None,
                install_record_path: Some(self.store.record_path(&draft.model_id)),
                cache_path: Some(draft.cache_path.clone()),
                additional_cache_paths: draft.additional_cache_paths.clone(),
                bytes: reclaimable_bytes,
            });
            prune_entries.push(ModelPruneEntry {
                model_id: draft.model_id,
                repository: draft.repository,
                revision: draft.revision,
                cache_path: draft.cache_path,
                additional_cache_paths: draft.additional_cache_paths,
                blobs,
                reclaimable_bytes,
            });
        }
        let total_bytes = targets.iter().map(|target| target.bytes).sum();
        Ok(ModelLifecyclePlan {
            operation: ModelLifecycleOperation::Prune,
            targets,
            prune_entries,
            total_bytes,
        })
    }

    pub fn execute_prune(&self, plan: &ModelLifecyclePlan) -> Result<()> {
        ensure!(
            plan.operation == ModelLifecycleOperation::Prune,
            "invalid prune plan"
        );
        let current = self.plan_prune()?;
        ensure!(
            current.targets == plan.targets,
            "prune candidates changed after the plan was created"
        );
        let _locks = acquire_cache_locks(plan)?;
        let locked_current = self.plan_prune()?;
        ensure!(
            locked_current.targets == plan.targets
                && locked_current.prune_entries == plan.prune_entries,
            "prune candidates changed while cache locks were being acquired; retry"
        );
        let planned_paths = plan
            .targets
            .iter()
            .flat_map(|target| {
                target
                    .cache_path
                    .iter()
                    .chain(target.additional_cache_paths.iter())
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        for entry in &plan.prune_entries {
            let repo_root = hf_repo_root(&self.hf_cache_dir, &entry.repository)?;
            for blob in &entry.blobs {
                if blob.reclaimable_bytes == 0 {
                    continue;
                }
                let references = snapshot_references(&repo_root, &blob.path)?;
                ensure!(
                    references.iter().all(|path| planned_paths.contains(path)),
                    "Hugging Face blob {} gained an unrelated snapshot reference; retry prune",
                    blob.path.display()
                );
                let current_bytes = regular_file_len(&blob.path)?;
                ensure!(
                    current_bytes == blob.reclaimable_bytes,
                    "Hugging Face blob {} changed after prune planning; retry",
                    blob.path.display()
                );
            }
        }
        // Remove blobs before their snapshot pointers. If a later pointer removal
        // fails, the remaining dangling pointer still identifies the exact blob,
        // so a subsequent invocation can safely finish the same tombstoned record.
        for blob in plan
            .prune_entries
            .iter()
            .flat_map(|entry| entry.blobs.iter())
            .filter(|blob| blob.reclaimable_bytes > 0)
        {
            remove_regular_file_if_present(&blob.path, "Hugging Face blob")?;
        }
        for target in &plan.targets {
            let path = target
                .cache_path
                .as_ref()
                .context("prune target has no cache path")?;
            for path in std::iter::once(path).chain(target.additional_cache_paths.iter()) {
                if !(path.exists() || std::fs::symlink_metadata(path).is_ok()) {
                    continue;
                }
                let metadata = std::fs::symlink_metadata(path)?;
                ensure!(
                    metadata.file_type().is_symlink() || metadata.is_file(),
                    "refusing to prune non-file cache path {}",
                    path.display()
                );
                std::fs::remove_file(path)
                    .with_context(|| format!("failed to prune cache path {}", path.display()))?;
            }
            self.store.delete_record(&target.model_id)?;
        }
        Ok(())
    }

    fn ensure_not_protected(&self, id: &ModelId) -> Result<()> {
        if self.protected_ids.contains(id) {
            bail!("model `{id}` is referenced by an active function, setup, job, or lease");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct PruneDraft {
    model_id: ModelId,
    repository: String,
    revision: String,
    cache_path: PathBuf,
    additional_cache_paths: Vec<PathBuf>,
    pointers: Vec<HfPointer>,
}

#[derive(Clone, Debug)]
struct HfPointer {
    snapshot_bytes: u64,
    blob: Option<HfBlobCandidate>,
}

#[derive(Clone, Debug)]
struct HfBlobCandidate {
    path: PathBuf,
    lock_path: PathBuf,
    repo_root: PathBuf,
    reclaimable_bytes: u64,
}

fn record_paths(record: &crate::ModelInstallRecord) -> impl Iterator<Item = PathBuf> + '_ {
    std::iter::once(record.path.clone())
        .chain(record.additional_files.iter().map(|file| file.path.clone()))
}

fn inspect_hf_pointer(
    cache_dir: &Path,
    repository: &str,
    revision: &str,
    filename: &str,
    recorded_path: &Path,
) -> Result<HfPointer> {
    ensure!(
        revision.len() == 40
            && revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "refusing to prune Hugging Face record with an unpinned revision"
    );
    ensure_safe_relative_path(filename, "Hugging Face filename")?;
    let repo_root = hf_repo_root(cache_dir, repository)?;
    let expected_path = repo_root.join("snapshots").join(revision).join(filename);
    ensure!(
        recorded_path == expected_path,
        "refusing to prune cache path outside its recorded Hugging Face provenance: expected {}, found {}",
        expected_path.display(),
        recorded_path.display()
    );
    let metadata = match std::fs::symlink_metadata(recorded_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(HfPointer {
                snapshot_bytes: 0,
                blob: None,
            });
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(recorded_path)?;
        let target = if target.is_absolute() {
            target
        } else {
            recorded_path
                .parent()
                .context("Hugging Face snapshot pointer has no parent")?
                .join(target)
        };
        let blob_path = lexical_normalize(&target)?;
        let blob_root = repo_root.join("blobs");
        ensure_direct_child(&blob_root, &blob_path, "Hugging Face blob")?;
        let etag = blob_path
            .file_name()
            .context("Hugging Face blob path has no ETag")?;
        let lock_path = cache_dir
            .join(".locks")
            .join(
                repo_root
                    .file_name()
                    .context("invalid Hugging Face repo root")?,
            )
            .join(PathBuf::from(etag).with_extension("lock"));
        return Ok(HfPointer {
            snapshot_bytes: 0,
            blob: Some(HfBlobCandidate {
                path: blob_path,
                lock_path,
                repo_root,
                reclaimable_bytes: 0,
            }),
        });
    }
    ensure!(
        metadata.is_file(),
        "refusing to prune non-file Hugging Face cache path {}",
        recorded_path.display()
    );
    Ok(HfPointer {
        // hf-hub uses a snapshot copy rather than a symlink on platforms where
        // symlink creation is unavailable. Removing that exact copy is safe,
        // but no blob identity is inferred from a filename or digest.
        snapshot_bytes: metadata.len(),
        blob: None,
    })
}

fn hf_repo_root(cache_dir: &Path, repository: &str) -> Result<PathBuf> {
    let parts = repository.split('/').collect::<Vec<_>>();
    ensure!(
        matches!(parts.len(), 1 | 2)
            && parts.iter().all(|part| {
                !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "refusing to prune invalid Hugging Face repository identity `{repository}`"
    );
    Ok(cache_dir.join(format!("models--{}", parts.join("--"))))
}

fn ensure_safe_relative_path(path: &str, label: &str) -> Result<()> {
    ensure!(!path.is_empty(), "{label} cannot be empty");
    ensure!(
        Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "{label} must be a safe relative path"
    );
    Ok(())
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "cache path must be absolute");
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(normalized.pop(), "cache path escapes its filesystem root");
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn ensure_direct_child(root: &Path, path: &Path, label: &str) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{label} is outside {}", root.display()))?;
    ensure!(
        relative.components().count() == 1
            && matches!(relative.components().next(), Some(Component::Normal(_))),
        "{label} must be a direct child of {}",
        root.display()
    );
    Ok(())
}

fn snapshot_references(repo_root: &Path, blob_path: &Path) -> Result<BTreeSet<PathBuf>> {
    let snapshots = repo_root.join("snapshots");
    if !snapshots.exists() {
        return Ok(BTreeSet::new());
    }
    let mut references = BTreeSet::new();
    let mut pending = vec![snapshots];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).with_context(|| {
            format!(
                "failed to inspect Hugging Face snapshots below {}",
                directory.display()
            )
        })?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path)?;
                let target = if target.is_absolute() {
                    target
                } else {
                    path.parent()
                        .context("snapshot pointer has no parent")?
                        .join(target)
                };
                if lexical_normalize(&target)? == blob_path {
                    references.insert(path);
                }
            }
        }
    }
    Ok(references)
}

fn regular_file_len(path: &Path) -> Result<u64> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Hugging Face blob is not a regular file: {}",
                path.display()
            );
            Ok(metadata.len())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn remove_regular_file_if_present(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "refusing to delete non-file {label} {}",
                path.display()
            );
            std::fs::remove_file(path)
                .with_context(|| format!("failed to delete {label} {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct CacheLocks {
    _files: Vec<File>,
}

fn acquire_cache_locks(plan: &ModelLifecyclePlan) -> Result<CacheLocks> {
    let lock_paths = plan
        .prune_entries
        .iter()
        .flat_map(|entry| entry.blobs.iter().map(|blob| blob.lock_path.clone()))
        .collect::<BTreeSet<_>>();
    let mut files = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        let parent = path
            .parent()
            .context("Hugging Face cache lock has no parent")?;
        std::fs::create_dir_all(parent)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| {
                format!("failed to open Hugging Face cache lock {}", path.display())
            })?;
        file.try_lock().with_context(|| {
            format!(
                "Hugging Face cache entry is active ({}); wait for the download or model job and retry prune",
                path.display()
            )
        })?;
        files.push(file);
    }
    Ok(CacheLocks { _files: files })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveModelLease {
    version: u32,
    model_key: String,
    pid: u32,
    paths: Vec<PathBuf>,
}

fn active_model_lease_paths(root: Option<&Path>) -> Result<BTreeSet<PathBuf>> {
    let Some(root) = root else {
        return Ok(BTreeSet::new());
    };
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = BTreeSet::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => continue,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect active model lease {}", path.display())
                });
            }
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let lease: ActiveModelLease = serde_json::from_slice(&bytes)
            .with_context(|| format!("active model lease is invalid: {}", path.display()))?;
        ensure!(
            lease.version == 1
                && lease.pid > 0
                && lease.model_key.len() == 64
                && lease
                    .model_key
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "active model lease metadata is invalid: {}",
            path.display()
        );
        for leased_path in lease.paths {
            ensure!(
                leased_path.is_absolute(),
                "active model lease contains a relative cache path: {}",
                path.display()
            );
            paths.insert(leased_path);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agl_config::{ModelBinding, ModelBindings};

    use super::*;
    use crate::{InstallRecordState, ModelArtifactRole, ModelInstallRecord};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn roots() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "agl-model-lifecycle-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        (root.join("data"), root.join("config"))
    }

    #[cfg(unix)]
    fn hf_removed_record(
        data: &Path,
        cache: &Path,
        id: &str,
        revision: &str,
        etag: &str,
    ) -> (ModelInstallStore, ModelId, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let repo_root = cache.join("models--owner--repo");
        let blob = repo_root.join("blobs").join(etag);
        let snapshot = repo_root
            .join("snapshots")
            .join(revision)
            .join("model.gguf");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"GGUFpayload").unwrap();
        symlink(format!("../../blobs/{etag}"), &snapshot).unwrap();

        let id = ModelId::new(id).unwrap();
        let store = ModelInstallStore::from_data_dir(data);
        store
            .write(&ModelInstallRecord {
                version: 1,
                model_id: id.clone(),
                package_id: None,
                role: ModelArtifactRole::Main,
                source: InstallSource::HuggingFace {
                    repository: "owner/repo".to_string(),
                    revision: revision.to_string(),
                    filename: "model.gguf".to_string(),
                },
                path: snapshot.clone(),
                byte_size: 11,
                sha256: "b".repeat(64),
                additional_files: Vec::new(),
                installed_at_unix_ms: 1,
                state: InstallRecordState::Removed,
            })
            .unwrap();
        (store, id, snapshot, blob)
    }

    #[test]
    fn unbind_then_remove_preserves_model_bytes() {
        let (data, config) = roots();
        let model = data.join("cache/model.gguf");
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        std::fs::write(&model, b"GGUFpayload").unwrap();
        let id = ModelId::new("model").unwrap();
        let store = ModelInstallStore::from_data_dir(&data);
        store
            .write(&ModelInstallRecord {
                version: 1,
                model_id: id.clone(),
                package_id: None,
                role: ModelArtifactRole::Main,
                source: InstallSource::HuggingFace {
                    repository: "owner/repo".to_string(),
                    revision: "a".repeat(40),
                    filename: "model.gguf".to_string(),
                },
                path: std::fs::canonicalize(&model).unwrap(),
                byte_size: 11,
                sha256: "b".repeat(64),
                additional_files: Vec::new(),
                installed_at_unix_ms: 1,
                state: InstallRecordState::Active,
            })
            .unwrap();
        let mut bindings = ModelBindings::empty();
        bindings.models.insert(
            id.clone(),
            ModelBinding {
                path: std::fs::canonicalize(&model).unwrap(),
            },
        );
        write_model_bindings(model_bindings_path(&config), &bindings).unwrap();
        let service = ModelLifecycleService::new(store.clone(), &config);
        let unbind = service.plan_unbind(&id).unwrap();
        service.execute_unbind(&unbind).unwrap();
        let remove = service.plan_remove(&id).unwrap();
        service.execute_remove(&remove).unwrap();
        assert!(model.exists());
        assert_eq!(
            store.get(&id).unwrap().unwrap().state,
            InstallRecordState::Removed
        );
    }

    #[cfg(unix)]
    #[test]
    fn prune_reclaims_only_agl_provenanced_unreferenced_blob() {
        let (data, config) = roots();
        let cache = data.join("hf");
        let revision = "a".repeat(40);
        let (store, id, snapshot, blob) =
            hf_removed_record(&data, &cache, "removed", &revision, "etag-one");
        let service =
            ModelLifecycleService::new(store.clone(), &config).with_hf_cache_dir(cache.clone());

        let plan = service.plan_prune().unwrap();
        assert_eq!(plan.total_bytes, 11);
        assert_eq!(plan.prune_entries[0].blobs[0].path, blob);
        assert_eq!(plan.prune_entries[0].blobs[0].reclaimable_bytes, 11);
        service.execute_prune(&plan).unwrap();

        assert!(!snapshot.exists());
        assert!(!blob.exists());
        assert!(store.get(&id).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn prune_preserves_blob_shared_by_an_unrelated_revision() {
        use std::os::unix::fs::symlink;

        let (data, config) = roots();
        let cache = data.join("hf");
        let revision = "a".repeat(40);
        let (store, id, snapshot, blob) =
            hf_removed_record(&data, &cache, "removed", &revision, "shared-etag");
        let unrelated = cache
            .join("models--owner--repo/snapshots")
            .join("c".repeat(40))
            .join("model.gguf");
        std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        symlink("../../blobs/shared-etag", &unrelated).unwrap();
        let service =
            ModelLifecycleService::new(store.clone(), &config).with_hf_cache_dir(cache.clone());

        let plan = service.plan_prune().unwrap();
        assert_eq!(plan.total_bytes, 0);
        assert_eq!(plan.prune_entries[0].blobs[0].reclaimable_bytes, 0);
        service.execute_prune(&plan).unwrap();

        assert!(std::fs::symlink_metadata(&snapshot).is_err());
        assert!(blob.exists());
        assert!(unrelated.exists());
        assert!(store.get(&id).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn prune_rejects_a_cache_blob_held_by_the_hf_lock() {
        let (data, config) = roots();
        let cache = data.join("hf");
        let revision = "a".repeat(40);
        let (store, id, snapshot, blob) =
            hf_removed_record(&data, &cache, "removed", &revision, "locked-etag");
        let service =
            ModelLifecycleService::new(store.clone(), &config).with_hf_cache_dir(cache.clone());
        let plan = service.plan_prune().unwrap();
        let lock_path = &plan.prune_entries[0].blobs[0].lock_path;
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.lock().unwrap();

        let error = service.execute_prune(&plan).unwrap_err().to_string();
        assert!(error.contains("cache entry is active"), "{error}");
        assert!(std::fs::symlink_metadata(&snapshot).is_ok());
        assert!(blob.exists());
        assert!(store.get(&id).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn prune_rejects_a_path_used_by_an_active_binding() {
        let (data, config) = roots();
        let cache = data.join("hf");
        let revision = "a".repeat(40);
        let (store, _id, snapshot, blob) =
            hf_removed_record(&data, &cache, "removed", &revision, "active-etag");
        let active_id = ModelId::new("active-alias").unwrap();
        let mut bindings = ModelBindings::empty();
        bindings.models.insert(
            active_id,
            ModelBinding {
                path: snapshot.clone(),
            },
        );
        write_model_bindings(model_bindings_path(&config), &bindings).unwrap();
        let service = ModelLifecycleService::new(store, &config).with_hf_cache_dir(cache);

        let error = service.plan_prune().unwrap_err().to_string();
        assert!(error.contains("active binding, install record"), "{error}");
        assert!(std::fs::symlink_metadata(&snapshot).is_ok());
        assert!(blob.exists());
    }

    #[cfg(unix)]
    #[test]
    fn prune_rejects_a_path_held_by_an_active_model_lease() {
        use std::io::Write as _;

        let (data, config) = roots();
        let cache = data.join("hf");
        let leases = data.join("leases");
        let revision = "a".repeat(40);
        let (store, _id, snapshot, blob) =
            hf_removed_record(&data, &cache, "removed", &revision, "leased-etag");
        std::fs::create_dir_all(&leases).unwrap();
        let lease_path = leases.join("active.json");
        let mut lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lease_path)
            .unwrap();
        lease.lock().unwrap();
        serde_json::to_writer(
            &mut lease,
            &serde_json::json!({
                "version": 1,
                "model_key": "d".repeat(64),
                "pid": std::process::id(),
                "paths": [snapshot],
            }),
        )
        .unwrap();
        lease.flush().unwrap();
        let service = ModelLifecycleService::new(store, &config)
            .with_hf_cache_dir(cache)
            .with_lease_root(leases);

        let error = service.plan_prune().unwrap_err().to_string();
        assert!(error.contains("model lease"), "{error}");
        assert!(blob.exists());
    }
}
