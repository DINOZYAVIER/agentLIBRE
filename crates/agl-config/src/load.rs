use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::path::Path;

use crate::LocalInferenceConfig;

pub fn load_local_inference_config(path: impl AsRef<Path>) -> Result<LocalInferenceConfig> {
    let path = path.as_ref();
    let config: LocalInferenceConfig = load_toml_file(path)?;
    config
        .validate()
        .with_context(|| format!("invalid config file {}", path.display()))?;
    Ok(config)
}

pub fn load_local_inference_config_from_str(
    source_name: &str,
    text: &str,
) -> Result<LocalInferenceConfig> {
    let config: LocalInferenceConfig =
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
