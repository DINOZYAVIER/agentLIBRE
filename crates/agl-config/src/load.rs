use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::path::Path;

use crate::{LocalInferenceConfig, ModelConfig};

pub fn load_model_config(path: impl AsRef<Path>) -> Result<ModelConfig> {
    let config: ModelConfig = load_toml_file(path.as_ref())?;
    config.validate()?;
    Ok(config)
}

pub fn load_local_inference_config(path: impl AsRef<Path>) -> Result<LocalInferenceConfig> {
    let config: LocalInferenceConfig = load_toml_file(path.as_ref())?;
    config.validate()?;
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
