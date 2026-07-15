use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::path::Path;

use crate::{InferencePreset, ResolvedInferenceConfig};

pub fn load_inference_preset(path: impl AsRef<Path>) -> Result<InferencePreset> {
    let path = path.as_ref();
    let preset: InferencePreset = load_toml_file(path)?;
    preset
        .validate()
        .with_context(|| format!("invalid inference preset {}", path.display()))?;
    Ok(preset)
}

pub fn load_inference_preset_from_str(source_name: &str, text: &str) -> Result<InferencePreset> {
    let preset: InferencePreset =
        toml::from_str(text).with_context(|| format!("failed to parse preset {source_name}"))?;
    preset
        .validate()
        .with_context(|| format!("invalid inference preset {source_name}"))?;
    Ok(preset)
}

pub fn load_local_inference_config(path: impl AsRef<Path>) -> Result<ResolvedInferenceConfig> {
    let path = path.as_ref();
    let config: ResolvedInferenceConfig = load_toml_file(path)?;
    config
        .validate()
        .with_context(|| format!("invalid config file {}", path.display()))?;
    Ok(config)
}

pub fn load_local_inference_config_from_str(
    source_name: &str,
    text: &str,
) -> Result<ResolvedInferenceConfig> {
    let config: ResolvedInferenceConfig =
        toml::from_str(text).with_context(|| format!("failed to parse config {source_name}"))?;
    config
        .validate()
        .with_context(|| format!("invalid config {source_name}"))?;
    Ok(config)
}

fn load_toml_file<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse config file {}", path.display()))
}
