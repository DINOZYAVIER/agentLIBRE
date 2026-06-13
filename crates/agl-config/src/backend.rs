use std::path::PathBuf;

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::ModelConfig;

pub const MAX_GPU_LAYERS: u32 = 4096;
pub const MAX_CONTEXT_TOKENS: u32 = 1_048_576;
pub const MAX_THREADS: u32 = 1024;
pub const MAX_BATCH_TOKENS: u32 = 1_048_576;

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
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub batch_size: Option<u32>,
    #[serde(default)]
    pub ubatch_size: Option<u32>,
    #[serde(default)]
    pub flash_attention: Option<RuntimeSwitch>,
    #[serde(default)]
    pub cache_type_k: Option<KvCacheType>,
    #[serde(default)]
    pub cache_type_v: Option<KvCacheType>,
    #[serde(default)]
    pub mmap: Option<bool>,
    #[serde(default)]
    pub jinja: Option<bool>,
    #[serde(default)]
    pub conversation: Option<bool>,
    #[serde(default)]
    pub simple_io: bool,
    #[serde(default)]
    pub display_prompt: Option<bool>,
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
        if let Some(device) = &self.device {
            ensure!(
                !device.trim().is_empty(),
                "runtime device cannot be empty when configured"
            );
        }
        validate_optional_token_limit("batch_size", self.batch_size)?;
        validate_optional_token_limit("ubatch_size", self.ubatch_size)?;
        if let (Some(batch_size), Some(ubatch_size)) = (self.batch_size, self.ubatch_size)
            && ubatch_size > batch_size
        {
            bail!("ubatch_size {ubatch_size} cannot exceed batch_size {batch_size}");
        }
        Ok(())
    }
}

fn validate_optional_token_limit(name: &str, value: Option<u32>) -> Result<()> {
    if let Some(value) = value
        && (value == 0 || value > MAX_BATCH_TOKENS)
    {
        bail!("{name} {value} must be between 1 and {MAX_BATCH_TOKENS}");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSwitch {
    On,
    Off,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheType {
    F32,
    F16,
    Bf16,
    Q8_0,
    Q4_0,
    Q4_1,
    Iq4Nl,
    Q5_0,
    Q5_1,
}

impl KvCacheType {
    pub fn as_llama_arg(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F16 => "f16",
            Self::Bf16 => "bf16",
            Self::Q8_0 => "q8_0",
            Self::Q4_0 => "q4_0",
            Self::Q4_1 => "q4_1",
            Self::Iq4Nl => "iq4_nl",
            Self::Q5_0 => "q5_0",
            Self::Q5_1 => "q5_1",
        }
    }
}
