use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelBinding, ModelBindings, ModelId};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ModelArtifactRole, ModelPackageId};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstallSource {
    HuggingFace {
        repository: String,
        revision: String,
        filename: String,
    },
    Local {
        canonical_path: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallRecordState {
    Active,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInstallRecord {
    pub version: u32,
    pub model_id: ModelId,
    pub package_id: Option<ModelPackageId>,
    pub role: ModelArtifactRole,
    pub source: InstallSource,
    pub path: PathBuf,
    pub byte_size: u64,
    pub sha256: String,
    #[serde(default)]
    pub additional_files: Vec<InstalledArtifactFile>,
    pub installed_at_unix_ms: u64,
    pub state: InstallRecordState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledArtifactFile {
    pub filename: String,
    pub path: PathBuf,
    pub byte_size: u64,
    pub sha256: String,
}

impl ModelInstallRecord {
    pub fn from_downloaded(artifact: &crate::DownloadedArtifact) -> Result<Self> {
        let path = absolute_path(&artifact.path)?;
        let record = Self {
            version: 1,
            model_id: artifact.spec.model_id.clone(),
            package_id: artifact.spec.package_id.clone(),
            role: artifact.spec.role,
            source: InstallSource::HuggingFace {
                repository: artifact.spec.repository.clone(),
                revision: artifact.spec.revision.clone(),
                filename: artifact.spec.filename.clone(),
            },
            path,
            byte_size: artifact.byte_size,
            sha256: artifact.sha256.clone(),
            additional_files: artifact
                .additional_files
                .iter()
                .map(|file| {
                    Ok(InstalledArtifactFile {
                        filename: file.spec.filename.clone(),
                        path: absolute_path(&file.path)?,
                        byte_size: file.byte_size,
                        sha256: file.sha256.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            installed_at_unix_ms: now_unix_ms()?,
            state: InstallRecordState::Active,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported install record version");
        ensure!(
            self.byte_size > 4,
            "installed model is too small to be GGUF"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "installed model has an invalid SHA-256"
        );
        ensure!(
            self.path.is_absolute(),
            "installed model path must be absolute"
        );
        let primary_filename = match &self.source {
            InstallSource::HuggingFace {
                repository,
                revision,
                filename,
            } => {
                validate_hf_repository(repository)?;
                ensure!(
                    revision.len() == 40
                        && revision
                            .bytes()
                            .all(|byte| { byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() }),
                    "Hugging Face install revision must be a full lowercase commit SHA"
                );
                validate_gguf_relative_path(filename, "Hugging Face filename")?;
                Some(filename.as_str())
            }
            InstallSource::Local { canonical_path } => {
                ensure!(
                    self.package_id.is_none(),
                    "local install record cannot belong to a catalog package"
                );
                ensure!(
                    canonical_path.is_absolute() && canonical_path == &self.path,
                    "local install record path must match its canonical source"
                );
                ensure!(
                    self.additional_files.is_empty(),
                    "local install records do not infer additional files"
                );
                None
            }
        };
        let mut filenames = std::collections::BTreeSet::new();
        for file in &self.additional_files {
            validate_gguf_relative_path(&file.filename, "additional Hugging Face filename")?;
            ensure!(
                Some(file.filename.as_str()) != primary_filename
                    && filenames.insert(file.filename.as_str()),
                "installed model contains duplicate additional filenames"
            );
            ensure!(
                file.path.is_absolute(),
                "additional model path must be absolute"
            );
            ensure!(file.byte_size > 4, "additional GGUF shard is too small");
            ensure!(
                file.sha256.len() == 64
                    && file
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "additional GGUF shard has an invalid SHA-256"
            );
        }
        Ok(())
    }
}

fn validate_hf_repository(repository: &str) -> Result<()> {
    let parts = repository.split('/').collect::<Vec<_>>();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "Hugging Face install repository must be OWNER/REPO"
    );
    Ok(())
}

fn validate_gguf_relative_path(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.to_ascii_lowercase().ends_with(".gguf")
            && Path::new(value)
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "{label} must be a safe relative GGUF path"
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ModelInstallStore {
    root: PathBuf,
}

impl ModelInstallStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn from_data_dir(data_dir: impl AsRef<Path>) -> Self {
        Self::new(data_dir.as_ref().join("models/installed"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn record_path(&self, id: &ModelId) -> PathBuf {
        self.root.join(format!("{}.json", id.as_str()))
    }

    pub fn get(&self, id: &ModelId) -> Result<Option<ModelInstallRecord>> {
        let path = self.record_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read install record {}", path.display()))?;
        let record: ModelInstallRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse install record {}", path.display()))?;
        record
            .validate()
            .with_context(|| format!("invalid install record {}", path.display()))?;
        ensure!(
            &record.model_id == id,
            "install record filename does not match model id"
        );
        Ok(Some(record))
    }

    pub fn list(&self) -> Result<Vec<ModelInstallRecord>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = std::fs::read_dir(&self.root)
            .with_context(|| format!("failed to read install store {}", self.root.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        let mut records = Vec::new();
        for path in paths {
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let record: ModelInstallRecord = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse install record {}", path.display()))?;
            record.validate()?;
            ensure!(
                path == self.record_path(&record.model_id),
                "install record filename does not match model id: {}",
                path.display()
            );
            records.push(record);
        }
        Ok(records)
    }

    #[cfg(test)]
    pub(crate) fn write(&self, record: &ModelInstallRecord) -> Result<()> {
        record.validate()?;
        let bytes = serde_json::to_vec_pretty(record)?;
        atomic_write(&self.record_path(&record.model_id), &bytes)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBindingPatch {
    pub models: BTreeMap<ModelId, ModelBinding>,
}

impl ModelBindingPatch {
    pub fn insert(&mut self, id: ModelId, path: PathBuf) {
        self.models.insert(id, ModelBinding { path });
    }

    pub fn merge_into(&self, bindings: &mut ModelBindings, replace: bool) -> Result<()> {
        for (id, incoming) in &self.models {
            if let Some(existing) = bindings.models.get(id)
                && existing != incoming
                && !replace
            {
                bail!(
                    "model binding `{id}` already points to {}; pass --replace to update it",
                    existing.path.display()
                );
            }
        }
        bindings.models.extend(self.models.clone());
        bindings.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedModel {
    pub record: ModelInstallRecord,
    pub binding_patch: ModelBindingPatch,
}

pub fn import_local_model(
    path: impl AsRef<Path>,
    id: Option<ModelId>,
    role: ModelArtifactRole,
) -> Result<ImportedModel> {
    let path = std::fs::canonicalize(path.as_ref()).with_context(|| {
        format!(
            "failed to canonicalize local model {}",
            path.as_ref().display()
        )
    })?;
    let id = id.unwrap_or(derive_model_id_from_path(&path)?);
    let (byte_size, sha256) = validate_gguf(&path, None, None)?;
    let record = ModelInstallRecord {
        version: 1,
        model_id: id.clone(),
        package_id: None,
        role,
        source: InstallSource::Local {
            canonical_path: path.clone(),
        },
        path: path.clone(),
        byte_size,
        sha256,
        additional_files: Vec::new(),
        installed_at_unix_ms: now_unix_ms()?,
        state: InstallRecordState::Active,
    };
    let mut binding_patch = ModelBindingPatch::default();
    binding_patch.insert(id, path);
    Ok(ImportedModel {
        record,
        binding_patch,
    })
}

pub fn validate_gguf(
    path: impl AsRef<Path>,
    expected_size: Option<u64>,
    expected_sha256: Option<&str>,
) -> Result<(u64, String)> {
    let path = path.as_ref();
    ensure!(
        path.is_file(),
        "GGUF file does not exist: {}",
        path.display()
    );
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let size = metadata.len();
    if let Some(expected) = expected_size {
        ensure!(
            size == expected,
            "GGUF size mismatch for {}: expected {expected}, found {size}",
            path.display()
        );
    }
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("failed to read GGUF header from {}", path.display()))?;
    ensure!(magic == *b"GGUF", "file is not GGUF: {}", path.display());
    let mut hasher = Sha256::new();
    hasher.update(magic);
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let sha256 = hex(&hasher.finalize());
    if let Some(expected) = expected_sha256 {
        ensure!(
            sha256 == expected,
            "GGUF digest mismatch for {}: expected {expected}, found {sha256}",
            path.display()
        );
    }
    Ok((size, sha256))
}

pub(crate) fn now_unix_ms() -> Result<u64> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    u64::try_from(value).context("current timestamp does not fit u64")
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("atomic write path must be UTF-8")?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

pub fn derive_model_id_from_path(path: &Path) -> Result<ModelId> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("local model filename must be UTF-8")?;
    normalized_model_id(stem).context("could not derive a legal model id; pass --id")
}

pub fn derive_hf_model_id(repository: &str, filename: &str) -> Result<ModelId> {
    let repository_name = repository
        .split_once('/')
        .map(|(_, name)| name)
        .context("Hugging Face repository must be OWNER/REPO")?;
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("Hugging Face model filename must be UTF-8")?;
    normalized_model_id(&format!("{repository_name}-{stem}"))
        .context("could not derive a legal model id; pass --id")
}

fn normalized_model_id(input: &str) -> Result<ModelId> {
    let mut value = String::new();
    for character in input.chars() {
        if character.is_ascii_alphanumeric() {
            value.push(character.to_ascii_lowercase());
        } else if matches!(character, '-' | '_' | '.') {
            value.push(character);
        } else if !value.ends_with('-') {
            value.push('-');
        }
    }
    ModelId::new(value.trim_matches('-').to_string())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current directory")?
        .join(path))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-model-install-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn import_validates_without_copying() {
        let path = temp_file("My Model.gguf", b"GGUFpayload");
        let imported = import_local_model(&path, None, ModelArtifactRole::Main).unwrap();
        assert_eq!(imported.record.model_id.as_str(), "my-model");
        assert_eq!(imported.record.path, std::fs::canonicalize(&path).unwrap());
        assert_eq!(imported.record.byte_size, 11);
        assert_eq!(imported.binding_patch.models.len(), 1);
    }

    #[test]
    fn rejects_non_gguf_and_wrong_digest() {
        let invalid = temp_file("invalid.gguf", b"nope");
        assert!(validate_gguf(&invalid, None, None).is_err());
        let valid = temp_file("valid.gguf", b"GGUFpayload");
        assert!(validate_gguf(&valid, Some(11), Some(&"0".repeat(64))).is_err());
    }

    #[test]
    fn install_store_rejects_record_filename_identity_mismatch() {
        let path = temp_file("stored.gguf", b"GGUFpayload");
        let imported = import_local_model(
            &path,
            Some(ModelId::new("expected").unwrap()),
            ModelArtifactRole::Main,
        )
        .unwrap();
        let store = ModelInstallStore::new(path.parent().unwrap().join("records"));
        store.write(&imported.record).unwrap();
        std::fs::rename(
            store.record_path(&imported.record.model_id),
            store.root().join("wrong.json"),
        )
        .unwrap();
        assert!(store.list().is_err());
    }

    #[test]
    fn install_transaction_replaces_records_and_bindings_together() {
        let old_path = temp_file("old.gguf", b"GGUFold-model");
        let root = old_path.parent().unwrap();
        let new_path = root.join("new.gguf");
        std::fs::write(&new_path, b"GGUFnew-model").unwrap();
        let id = ModelId::new("replace-me").unwrap();
        let old = import_local_model(&old_path, Some(id.clone()), ModelArtifactRole::Main).unwrap();
        let new = import_local_model(&new_path, Some(id.clone()), ModelArtifactRole::Main).unwrap();
        let store = ModelInstallStore::new(root.join("records"));
        let bindings_path = root.join("models.toml");
        crate::ModelInstallTransaction::new(store.clone(), &bindings_path)
            .unwrap()
            .commit(crate::ModelInstallTransactionInput::new(
                vec![old.record.clone()],
                old.binding_patch.clone(),
                false,
            ))
            .unwrap();
        crate::ModelInstallTransaction::new(store.clone(), &bindings_path)
            .unwrap()
            .commit(crate::ModelInstallTransactionInput::new(
                vec![new.record.clone()],
                new.binding_patch,
                true,
            ))
            .unwrap();
        assert_eq!(store.get(&id).unwrap(), Some(new.record));
        assert_eq!(
            agl_config::load_model_bindings_or_empty(&bindings_path)
                .unwrap()
                .models[&id]
                .path,
            std::fs::canonicalize(&new_path).unwrap()
        );
    }

    #[test]
    fn install_transaction_publishes_new_state() {
        let path = temp_file("new-only.gguf", b"GGUFnew-only");
        let root = path.parent().unwrap();
        let imported = import_local_model(
            &path,
            Some(ModelId::new("new-only").unwrap()),
            ModelArtifactRole::Main,
        )
        .unwrap();
        let store = ModelInstallStore::new(root.join("records"));
        let bindings_path = root.join("models.toml");

        crate::ModelInstallTransaction::new(store.clone(), &bindings_path)
            .unwrap()
            .commit(crate::ModelInstallTransactionInput::new(
                vec![imported.record.clone()],
                imported.binding_patch,
                false,
            ))
            .unwrap();
        assert!(bindings_path.is_file());
        assert!(store.get(&imported.record.model_id).unwrap().is_some());
    }
}
