use std::path::PathBuf;

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::ModelConfig;

pub const MAX_GPU_LAYERS: u32 = 4096;
pub const MAX_CONTEXT_TOKENS: u32 = 1_048_576;
pub const MAX_THREADS: u32 = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalInferenceConfig {
    pub backend: InferenceBackendConfig,
    pub runtime: InferenceRuntimeConfig,
    pub model: ModelConfig,
}

impl LocalInferenceConfig {
    pub fn validate(&self) -> Result<()> {
        self.backend.validate()?;
        self.runtime.validate()?;
        self.model.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceBackendConfig {
    pub kind: BackendKind,
    pub binary: PathBuf,
    pub model: PathBuf,
}

impl InferenceBackendConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.binary.as_os_str().is_empty(),
            "backend binary path cannot be empty"
        );
        ensure!(
            !self.model.as_os_str().is_empty(),
            "backend model path cannot be empty"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    LlamaCpp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRuntimeConfig {
    pub gpu_layers: u32,
    pub context_tokens: u32,
    pub threads: u32,
}

impl InferenceRuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        if self.gpu_layers > MAX_GPU_LAYERS {
            bail!(
                "gpu_layers {} exceeds maximum {}",
                self.gpu_layers,
                MAX_GPU_LAYERS
            );
        }
        if self.context_tokens == 0 || self.context_tokens > MAX_CONTEXT_TOKENS {
            bail!(
                "context_tokens {} must be between 1 and {}",
                self.context_tokens,
                MAX_CONTEXT_TOKENS
            );
        }
        if self.threads == 0 || self.threads > MAX_THREADS {
            bail!(
                "threads {} must be between 1 and {}",
                self.threads,
                MAX_THREADS
            );
        }
        Ok(())
    }
}
