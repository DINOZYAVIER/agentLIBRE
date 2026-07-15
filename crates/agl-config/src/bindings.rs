use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{InferenceBackendConfig, InferencePreset, ResolvedInferenceConfig};

pub const MODEL_BINDINGS_FILE_NAME: &str = "models.toml";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure!(!value.is_empty(), "model id cannot be empty");
        ensure!(
            value.trim() == value,
            "model id cannot contain leading or trailing whitespace"
        );
        ensure!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "model id `{value}` contains unsupported characters"
        );
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ModelId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ModelId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBindings {
    pub version: u32,
    pub models: BTreeMap<ModelId, ModelBinding>,
}

impl ModelBindings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported model bindings version {}",
            self.version
        );
        for (id, binding) in &self.models {
            validate_model_path_value(id, &binding.path)?;
        }
        Ok(())
    }

    fn resolve(&self, id: &ModelId, source: &Path) -> Result<PathBuf> {
        let binding = self
            .models
            .get(id)
            .with_context(|| format!("model `{id}` is not configured in {}", source.display()))?;
        validate_model_file(id, &binding.path)
            .with_context(|| format!("invalid model `{id}` binding in {}", source.display()))?;
        Ok(binding.path.clone())
    }
}

pub fn model_bindings_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join(MODEL_BINDINGS_FILE_NAME)
}

pub fn load_model_bindings(path: impl AsRef<Path>) -> Result<ModelBindings> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read model bindings {}", path.display()))?;
    let bindings: ModelBindings = toml::from_str(&text)
        .with_context(|| format!("failed to parse model bindings {}", path.display()))?;
    bindings
        .validate()
        .with_context(|| format!("invalid model bindings {}", path.display()))?;
    Ok(bindings)
}

pub fn resolve_inference_preset(
    preset: InferencePreset,
    bindings_path: impl AsRef<Path>,
) -> Result<ResolvedInferenceConfig> {
    let bindings_path = bindings_path.as_ref();
    let bindings = load_model_bindings(bindings_path)?;
    resolve_inference_preset_with_bindings(preset, &bindings, bindings_path)
}

pub fn resolve_inference_preset_with_bindings(
    preset: InferencePreset,
    bindings: &ModelBindings,
    bindings_path: &Path,
) -> Result<ResolvedInferenceConfig> {
    preset.validate()?;
    let model = bindings.resolve(&preset.backend.model_id, bindings_path)?;
    let multimodal_projector = preset
        .backend
        .multimodal_projector_id
        .as_ref()
        .map(|id| bindings.resolve(id, bindings_path))
        .transpose()?;
    let draft_model = preset
        .runtime
        .mtp
        .draft_model_id
        .as_ref()
        .map(|id| bindings.resolve(id, bindings_path))
        .transpose()?;
    let config = ResolvedInferenceConfig {
        backend: InferenceBackendConfig {
            kind: preset.backend.kind,
            model,
            multimodal_projector,
        },
        runtime: preset.runtime.into_resolved(draft_model),
        model: preset.model,
        prompt: preset.prompt,
    };
    config.validate()?;
    Ok(config)
}

fn validate_model_path_value(id: &ModelId, path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty() && !path.as_os_str().to_string_lossy().trim().is_empty(),
        "model `{id}` path cannot be blank"
    );
    Ok(())
}

fn validate_model_file(id: &ModelId, path: &Path) -> Result<()> {
    validate_model_path_value(id, path)?;
    if !path.exists() {
        bail!("model `{id}` file does not exist: {}", path.display());
    }
    ensure!(
        path.is_file(),
        "model `{id}` path is not a file: {}",
        path.display()
    );
    Ok(())
}
