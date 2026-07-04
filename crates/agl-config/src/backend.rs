use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{ModelConfig, PromptConfig};

pub const MAX_GPU_LAYERS: u32 = 4096;
pub const MAX_CONTEXT_TOKENS: u32 = 1_048_576;
pub const MAX_THREADS: u32 = 1024;
pub const MAX_BATCH_TOKENS: u32 = 1_048_576;
pub const MAX_MTP_DRAFT_TOKENS: u32 = 64;
const MTP_PROBABILITY_SCALE: u32 = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalInferenceConfig {
    pub backend: InferenceBackendConfig,
    pub runtime: InferenceRuntimeConfig,
    pub model: ModelConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
}

impl LocalInferenceConfig {
    pub fn validate(&self) -> Result<()> {
        self.backend.validate()?;
        self.runtime.validate()?;
        self.model.validate()?;
        self.prompt.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceBackendConfig {
    pub kind: BackendKind,
    pub model: PathBuf,
}

impl InferenceBackendConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !path_is_blank(&self.model),
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

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama_cpp",
        }
    }
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
    pub kv_unified: Option<bool>,
    #[serde(default)]
    pub mtp: MtpRuntimeConfig,
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
            ensure!(
                device.trim() == device,
                "runtime device cannot contain leading or trailing whitespace"
            );
        }
        validate_optional_token_limit("batch_size", self.batch_size)?;
        validate_optional_token_limit("ubatch_size", self.ubatch_size)?;
        self.mtp.validate()?;
        if let Some(ubatch_size) = self.ubatch_size {
            if let Some(batch_size) = self.batch_size {
                if ubatch_size > batch_size {
                    bail!("ubatch_size {ubatch_size} cannot exceed batch_size {batch_size}");
                }
            } else if ubatch_size > self.context_tokens {
                bail!(
                    "ubatch_size {ubatch_size} cannot exceed context_tokens {} when batch_size is not configured",
                    self.context_tokens
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MtpRuntimeConfig {
    pub enabled: bool,
    pub draft_model: Option<PathBuf>,
    pub draft_tokens: u32,
    pub p_min: MtpProbability,
    pub gpu_layers: Option<u32>,
    pub cache_type_k: Option<KvCacheType>,
    pub cache_type_v: Option<KvCacheType>,
}

impl MtpRuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(path) = &self.draft_model {
            ensure!(
                !path_is_blank(path),
                "runtime.mtp draft_model path cannot be empty"
            );
        }
        if self.enabled && self.draft_model.is_none() {
            bail!("runtime.mtp enabled requires draft_model");
        }
        if self.enabled && self.draft_tokens == 0 {
            bail!("runtime.mtp draft_tokens must be between 1 and {MAX_MTP_DRAFT_TOKENS}");
        }
        if self.draft_tokens > MAX_MTP_DRAFT_TOKENS {
            bail!(
                "runtime.mtp draft_tokens {} exceeds maximum {}",
                self.draft_tokens,
                MAX_MTP_DRAFT_TOKENS
            );
        }
        if let Some(gpu_layers) = self.gpu_layers
            && gpu_layers > MAX_GPU_LAYERS
        {
            bail!(
                "runtime.mtp gpu_layers {} exceeds maximum {}",
                gpu_layers,
                MAX_GPU_LAYERS
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MtpProbability(u32);

impl MtpProbability {
    pub fn from_f32(value: f32) -> Result<Self> {
        Self::from_f64(f64::from(value)).map_err(anyhow::Error::msg)
    }

    pub fn as_f32(self) -> f32 {
        self.0 as f32 / MTP_PROBABILITY_SCALE as f32
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    fn from_f64(value: f64) -> std::result::Result<Self, String> {
        if !value.is_finite() {
            return Err("runtime.mtp p_min must be finite".to_string());
        }
        if !(0.0..=1.0).contains(&value) {
            return Err("runtime.mtp p_min must be between 0.0 and 1.0".to_string());
        }
        Ok(Self(
            (value * f64::from(MTP_PROBABILITY_SCALE)).round() as u32
        ))
    }
}

impl Serialize for MtpProbability {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.as_f32())
    }
}

impl<'de> Deserialize<'de> for MtpProbability {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(MtpProbabilityVisitor)
    }
}

struct MtpProbabilityVisitor;

impl<'de> de::Visitor<'de> for MtpProbabilityVisitor {
    type Value = MtpProbability;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a probability between 0.0 and 1.0")
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        MtpProbability::from_f64(value).map_err(E::custom)
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_f64(value as f64)
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_f64(value as f64)
    }
}

fn path_is_blank(path: &Path) -> bool {
    path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty()
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
