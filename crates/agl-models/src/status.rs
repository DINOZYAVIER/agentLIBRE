use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use agl_config::{ModelId, load_model_bindings_or_empty};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{InstallRecordState, ModelInstallRecord, ModelInstallStore, validate_gguf};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFileObservation {
    pub path: PathBuf,
    pub exists: bool,
    pub is_file: bool,
    pub byte_size: Option<u64>,
    pub gguf_header: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelStatusReport {
    pub model_id: ModelId,
    pub binding_path: Option<PathBuf>,
    pub install_record: Option<ModelInstallRecord>,
    pub file: Option<ModelFileObservation>,
    pub additional_files: Vec<ModelFileObservation>,
    pub healthy: bool,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelVerificationReport {
    pub status: ModelStatusReport,
    pub verified: bool,
    pub byte_size: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ModelInspector {
    store: ModelInstallStore,
    bindings_path: PathBuf,
}

impl ModelInspector {
    pub fn new(store: ModelInstallStore, bindings_path: impl Into<PathBuf>) -> Self {
        Self {
            store,
            bindings_path: bindings_path.into(),
        }
    }

    pub fn list(&self) -> Result<Vec<ModelStatusReport>> {
        let bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        let records = self.store.list()?;
        let mut ids = bindings.models.keys().cloned().collect::<BTreeSet<_>>();
        ids.extend(records.into_iter().map(|record| record.model_id));
        ids.into_iter().map(|id| self.status(&id)).collect()
    }

    pub fn status(&self, id: &ModelId) -> Result<ModelStatusReport> {
        let bindings = load_model_bindings_or_empty(&self.bindings_path)?;
        let binding_path = bindings.models.get(id).map(|binding| binding.path.clone());
        let install_record = self.store.get(id)?;
        let observed_path = binding_path
            .as_deref()
            .or_else(|| install_record.as_ref().map(|record| record.path.as_path()));
        let file = observed_path.map(observe_file).transpose()?;
        let additional_files = install_record
            .as_ref()
            .map(|record| {
                record
                    .additional_files
                    .iter()
                    .map(|file| observe_file(&file.path))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let mut problems = Vec::new();
        if binding_path.is_none() && install_record.is_none() {
            problems
                .push("model has neither an explicit binding nor an install record".to_string());
        }
        if let Some(record) = &install_record {
            if record.state == InstallRecordState::Removed {
                problems.push("install record is marked removed".to_string());
            }
            if record.state == InstallRecordState::Removed && binding_path.is_some() {
                problems.push("removed install record is still bound".to_string());
            }
            if let Some(binding) = &binding_path
                && binding != &record.path
            {
                problems.push("binding path differs from the install record".to_string());
            }
        }
        if let Some(file) = &file {
            if !file.exists {
                problems.push("model file does not exist".to_string());
            } else if !file.is_file {
                problems.push("model path is not a regular file".to_string());
            } else if file.gguf_header == Some(false) {
                problems.push("model file does not have a GGUF header".to_string());
            }
            if let Some(record) = &install_record
                && file.byte_size.is_some_and(|size| size != record.byte_size)
            {
                problems.push("model file size differs from the install record".to_string());
            }
        }
        for (index, file) in additional_files.iter().enumerate() {
            let label = install_record
                .as_ref()
                .and_then(|record| record.additional_files.get(index))
                .map(|file| file.filename.as_str())
                .unwrap_or("additional GGUF shard");
            if !file.exists {
                problems.push(format!("{label} does not exist"));
            } else if !file.is_file {
                problems.push(format!("{label} is not a regular file"));
            } else if file.gguf_header == Some(false) {
                problems.push(format!("{label} does not have a GGUF header"));
            }
            if let Some(recorded) = install_record
                .as_ref()
                .and_then(|record| record.additional_files.get(index))
                && file
                    .byte_size
                    .is_some_and(|size| size != recorded.byte_size)
            {
                problems.push(format!("{label} size differs from the install record"));
            }
        }
        Ok(ModelStatusReport {
            model_id: id.clone(),
            binding_path,
            install_record,
            file,
            additional_files,
            healthy: problems.is_empty(),
            problems,
        })
    }

    pub fn verify(&self, id: &ModelId) -> Result<ModelVerificationReport> {
        let status = self.status(id)?;
        let path = status
            .binding_path
            .as_deref()
            .or_else(|| {
                status
                    .install_record
                    .as_ref()
                    .map(|record| record.path.as_path())
            })
            .with_context(|| format!("model `{id}` has no file to verify"))?;
        let expected_size = status
            .install_record
            .as_ref()
            .map(|record| record.byte_size);
        let expected_sha256 = status
            .install_record
            .as_ref()
            .map(|record| record.sha256.as_str());
        let (byte_size, sha256) = validate_gguf(path, expected_size, expected_sha256)?;
        if let Some(record) = &status.install_record {
            for file in &record.additional_files {
                validate_gguf(&file.path, Some(file.byte_size), Some(&file.sha256))?;
            }
        }
        Ok(ModelVerificationReport {
            verified: status.healthy,
            status,
            byte_size: Some(byte_size),
            sha256: Some(sha256),
        })
    }
}

fn observe_file(path: &Path) -> Result<ModelFileObservation> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let (exists, is_file, byte_size, gguf_header) = if let Some(metadata) = metadata {
        let is_file = metadata.is_file();
        let header = if is_file {
            let mut bytes = [0_u8; 4];
            File::open(path)
                .and_then(|mut file| file.read_exact(&mut bytes))
                .map(|()| bytes == *b"GGUF")
                .ok()
        } else {
            None
        };
        (true, is_file, is_file.then_some(metadata.len()), header)
    } else {
        (false, false, None, None)
    };
    Ok(ModelFileObservation {
        path: path.to_path_buf(),
        exists,
        is_file,
        byte_size,
        gguf_header,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::{InstallSource, ModelArtifactRole};

    use super::*;

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn removed_install_record_is_not_reported_ready() {
        let root = std::env::temp_dir().join(format!(
            "agl-model-status-removed-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let model = root.join("model.gguf");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&model, b"GGUFmodel").unwrap();
        let id = ModelId::new("removed").unwrap();
        let store = ModelInstallStore::new(root.join("records"));
        store
            .write(&ModelInstallRecord {
                version: 1,
                model_id: id.clone(),
                package_id: None,
                role: ModelArtifactRole::Main,
                source: InstallSource::Local {
                    canonical_path: model.clone(),
                },
                path: model,
                byte_size: 9,
                sha256: "a".repeat(64),
                additional_files: Vec::new(),
                installed_at_unix_ms: 1,
                state: InstallRecordState::Removed,
            })
            .unwrap();
        let report = ModelInspector::new(store, root.join("models.toml"))
            .status(&id)
            .unwrap();
        assert!(!report.healthy);
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("removed"))
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
