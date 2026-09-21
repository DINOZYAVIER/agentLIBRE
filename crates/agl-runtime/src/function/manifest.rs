use std::collections::BTreeSet;
use std::path::{Component, Path};

use crate::package::{PackageId, PackageVersion};
use agl_core::agent::{
    AgentRunLimits, MAX_TOOL_RESULT_BYTES, PresentationColors, ReasoningEffort, RelativePath,
};
use agl_core::{AuthorityGrant, AuthorityGrantSet, CanonicalJson, ToolId};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

pub const FUNCTION_FILE_NAME: &str = "FUNCTION.toml";
pub const FUNCTION_SCHEMA: &str = "agentlibre.function/v2";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityRef {
    pub id: PackageId,
    pub version: PackageVersion,
    #[serde(default)]
    pub git: Option<String>,
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

impl EntityRef {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.git.is_some() == self.rev.is_some(),
            "entity git and rev must be specified together"
        );
        if let Some(git) = &self.git {
            validate_git_source(git)?;
        }
        if let Some(rev) = &self.rev {
            ensure!(
                !rev.trim().is_empty()
                    && !rev.starts_with('-')
                    && rev.len() <= 1024
                    && !rev.chars().any(char::is_control),
                "entity Git revision is invalid"
            );
        }
        if let Some(path) = &self.path {
            validate_relative(path, self.git.is_some()).context("invalid entity source path")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceleratorPreference {
    #[default]
    Auto,
    Cpu,
    PreferGpu,
    RequireGpu,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceSelector {
    Pci { address: String },
    Uuid { uuid: String },
}

impl DeviceSelector {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Pci { address } => {
                let bytes = address.as_bytes();
                ensure!(
                    bytes.len() == 12
                        && bytes[4] == b':'
                        && bytes[7] == b':'
                        && bytes[10] == b'.'
                        && bytes.iter().enumerate().all(|(index, byte)| matches!(
                            index,
                            4 | 7 | 10
                        ) || byte
                            .is_ascii_hexdigit()),
                    "PCI selector must use dddd:bb:ss.f"
                );
            }
            Self::Uuid { uuid } => {
                ensure!(
                    uuid.len() >= 8
                        && uuid.len() <= 128
                        && uuid.bytes().any(|byte| byte.is_ascii_hexdigit())
                        && uuid
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
                    "device UUID selector is invalid"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuLayers {
    All,
    Count(u32),
}

impl Serialize for GpuLayers {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::All => serializer.serialize_str("all"),
            Self::Count(value) => serializer.serialize_u32(*value),
        }
    }
}

impl<'de> Deserialize<'de> for GpuLayers {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Text(String),
            Count(u32),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Text(value) if value == "all" => Ok(Self::All),
            Wire::Text(_) => Err(serde::de::Error::custom(
                "gpu_layers string must be \"all\"",
            )),
            Wire::Count(value) => Ok(Self::Count(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    None,
    #[default]
    Layer,
    Row,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheType {
    F32,
    F16,
    Bf16,
    Q8_0,
    Q5_0,
    Q5_1,
    Q4_0,
    Q4_1,
    Iq4Nl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TypedDuration(u64);

impl TypedDuration {
    pub const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    pub fn parse(value: &str) -> Result<Self> {
        let (digits, multiplier) = if let Some(value) = value.strip_suffix("ms") {
            (value, 1_u64)
        } else if let Some(value) = value.strip_suffix('s') {
            (value, 1_000)
        } else if let Some(value) = value.strip_suffix('m') {
            (value, 60_000)
        } else if let Some(value) = value.strip_suffix('h') {
            (value, 3_600_000)
        } else {
            anyhow::bail!("duration requires ms, s, m or h suffix");
        };
        let amount: u64 = digits.parse().context("duration value is not an integer")?;
        let millis = amount
            .checked_mul(multiplier)
            .context("duration overflows milliseconds")?;
        ensure!(
            millis > 0 && millis <= i64::MAX as u64,
            "duration must be finite and positive"
        );
        Ok(Self(millis))
    }
}

impl Serialize for TypedDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}ms", self.0))
    }
}

impl<'de> Deserialize<'de> for TypedDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationDefaults {
    pub max_output_tokens: u64,
    pub seed: u64,
    pub temperature: f64,
    pub top_k: u32,
    pub top_p: f64,
    pub min_p: f64,
    pub typical_p: f64,
    pub repeat_last_n: u32,
    pub repeat_penalty: f64,
    pub presence_penalty: f64,
    pub frequency_penalty: f64,
    pub stop: Vec<String>,
}

impl Default for GenerationDefaults {
    fn default() -> Self {
        Self {
            max_output_tokens: 8_192,
            seed: 1,
            temperature: 0.0,
            top_k: 1,
            top_p: 1.0,
            min_p: 0.0,
            typical_p: 1.0,
            repeat_last_n: 64,
            repeat_penalty: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            stop: Vec::new(),
        }
    }
}

impl GenerationDefaults {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_output_tokens > 0,
            "max_output_tokens must be positive"
        );
        ensure!(self.top_k > 0, "top_k must be positive");
        for (name, value) in [
            ("temperature", self.temperature),
            ("top_p", self.top_p),
            ("min_p", self.min_p),
            ("typical_p", self.typical_p),
            ("repeat_penalty", self.repeat_penalty),
            ("presence_penalty", self.presence_penalty),
            ("frequency_penalty", self.frequency_penalty),
        ] {
            ensure!(value.is_finite(), "{name} must be finite");
        }
        ensure!(self.temperature >= 0.0, "temperature cannot be negative");
        ensure!((0.0..=1.0).contains(&self.top_p), "top_p must be in 0..=1");
        ensure!((0.0..=1.0).contains(&self.min_p), "min_p must be in 0..=1");
        ensure!(
            (0.0..=1.0).contains(&self.typical_p),
            "typical_p must be in 0..=1"
        );
        ensure!(self.repeat_penalty > 0.0, "repeat_penalty must be positive");
        ensure!(self.stop.len() <= 64, "stop contains too many strings");
        ensure!(
            self.stop
                .iter()
                .all(|value| !value.is_empty() && value.len() <= 1024),
            "stop strings must be non-empty and bounded"
        );
        let mut stop = self.stop.clone();
        stop.sort();
        ensure!(
            stop.windows(2).all(|pair| pair[0] != pair[1]),
            "stop contains duplicates"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionReasoning {
    pub enabled: bool,
    pub max_tokens: Option<u32>,
    pub default: Option<ReasoningEffort>,
    pub preserve: Option<bool>,
}

impl FunctionReasoning {
    fn validate(&self, generation_max_output_tokens: u64) -> Result<()> {
        if !self.enabled {
            ensure!(
                self.max_tokens.is_none() && self.default.is_none() && self.preserve.is_none(),
                "reasoning settings require enabled = true"
            );
            return Ok(());
        }
        let max_tokens = self
            .max_tokens
            .context("enabled reasoning requires max_tokens")?;
        ensure!(max_tokens > 0, "reasoning max_tokens must be positive");
        ensure!(
            max_tokens <= i32::MAX as u32,
            "reasoning max_tokens exceeds the backend budget"
        );
        ensure!(
            u64::from(max_tokens) <= generation_max_output_tokens,
            "reasoning max_tokens cannot exceed generation max_output_tokens"
        );
        ensure!(
            self.preserve.is_some(),
            "enabled reasoning requires an explicit preserve setting"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionLoad {
    pub min_context_tokens: Option<u64>,
    pub context_tokens: Option<u64>,
    pub batch_size: Option<u32>,
    pub ubatch_size: Option<u32>,
    pub threads: Option<u32>,
    pub threads_batch: Option<u32>,
    pub accelerator: AcceleratorPreference,
    pub gpu_layers: Option<GpuLayers>,
    pub devices: Vec<DeviceSelector>,
    pub split_mode: SplitMode,
    pub main_gpu: Option<usize>,
    pub tensor_split: Vec<f64>,
    pub mmap: Option<bool>,
    pub mlock: Option<bool>,
    pub flash_attention: Option<bool>,
    pub kv_cache_type_k: Option<KvCacheType>,
    pub kv_cache_type_v: Option<KvCacheType>,
    pub engine_build_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionSpeculative {
    pub mode: FunctionSpeculativeMode,
    pub max_draft_tokens: u32,
    pub kv_cache_type_k: KvCacheType,
    pub kv_cache_type_v: KvCacheType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionSpeculativeMode {
    Mtp,
}

impl FunctionSpeculative {
    fn validate(&self, load: &FunctionLoad) -> Result<()> {
        ensure!(
            self.max_draft_tokens > 0,
            "max_draft_tokens must be positive"
        );
        ensure!(
            matches!(self.mode, FunctionSpeculativeMode::Mtp),
            "unsupported speculative mode"
        );
        ensure!(
            load.accelerator != AcceleratorPreference::Cpu,
            "MTP requires GPU acceleration"
        );
        ensure!(
            matches!(load.gpu_layers, Some(GpuLayers::All)),
            "MTP requires gpu_layers = \"all\""
        );
        Ok(())
    }
}

impl FunctionLoad {
    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("min_context_tokens", self.min_context_tokens),
            ("context_tokens", self.context_tokens),
        ] {
            ensure!(
                value.is_none_or(|value| value > 0),
                "{name} must be positive"
            );
        }
        for (name, value) in [
            ("batch_size", self.batch_size),
            ("ubatch_size", self.ubatch_size),
            ("threads", self.threads),
            ("threads_batch", self.threads_batch),
        ] {
            ensure!(
                value.is_none_or(|value| value > 0),
                "{name} must be positive"
            );
        }
        if let (Some(minimum), Some(exact)) = (self.min_context_tokens, self.context_tokens) {
            ensure!(
                exact >= minimum,
                "context_tokens cannot be lower than min_context_tokens"
            );
        }
        if let (Some(batch), Some(ubatch)) = (self.batch_size, self.ubatch_size) {
            ensure!(ubatch <= batch, "ubatch_size cannot exceed batch_size");
        }
        for device in &self.devices {
            device.validate()?;
        }
        let devices = self
            .devices
            .iter()
            .map(|value| match value {
                DeviceSelector::Pci { address } => format!("pci:{}", address.to_ascii_lowercase()),
                DeviceSelector::Uuid { uuid } => format!("uuid:{}", uuid.to_ascii_lowercase()),
            })
            .collect::<BTreeSet<_>>();
        ensure!(
            devices.len() == self.devices.len(),
            "devices contain duplicates"
        );
        ensure!(
            self.main_gpu.is_none_or(|index| index < self.devices.len()),
            "main_gpu must index devices"
        );
        ensure!(
            self.tensor_split
                .iter()
                .all(|value| value.is_finite() && *value > 0.0),
            "tensor_split values must be positive and finite"
        );
        ensure!(
            self.tensor_split.is_empty() || self.tensor_split.len() == self.devices.len(),
            "tensor_split must match devices"
        );
        let explicitly_cpu = matches!(self.gpu_layers, Some(GpuLayers::Count(0)));
        let has_gpu_placement = !self.devices.is_empty()
            || self.main_gpu.is_some()
            || !self.tensor_split.is_empty()
            || self
                .gpu_layers
                .is_some_and(|layers| !matches!(layers, GpuLayers::Count(0)));
        ensure!(
            self.accelerator != AcceleratorPreference::Cpu || !has_gpu_placement,
            "CPU acceleration cannot contain GPU placement settings"
        );
        ensure!(
            !matches!(
                self.accelerator,
                AcceleratorPreference::PreferGpu | AcceleratorPreference::RequireGpu
            ) || !explicitly_cpu,
            "GPU acceleration cannot require zero GPU layers"
        );
        ensure!(
            self.split_mode != SplitMode::None || self.tensor_split.is_empty(),
            "tensor_split requires layer or row split_mode"
        );
        if let Some(digest) = &self.engine_build_digest {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "engine_build_digest must be 64 lowercase hexadecimal characters"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionAdapter {
    pub model: EntityRef,
    pub scale: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionService {
    pub slots: Option<u32>,
    pub queue_capacity: Option<u32>,
    pub continuous_batching: bool,
    pub idle_timeout: TypedDuration,
}

impl Default for FunctionService {
    fn default() -> Self {
        Self {
            slots: None,
            queue_capacity: None,
            continuous_batching: true,
            idle_timeout: TypedDuration::from_millis(15 * 60 * 1_000),
        }
    }
}

impl FunctionService {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.slots.is_none_or(|value| (1..=128).contains(&value)),
            "slots must be between 1 and 128"
        );
        ensure!(
            self.queue_capacity
                .is_none_or(|value| (1..=1024).contains(&value)),
            "queue_capacity must be between 1 and 1024"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionInference {
    pub generation: GenerationDefaults,
    pub reasoning: FunctionReasoning,
    pub load: FunctionLoad,
    pub speculative: Option<FunctionSpeculative>,
    pub adapters: Vec<FunctionAdapter>,
    pub service: FunctionService,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TypedDuration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_input_tokens: Option<u64>,
    pub model_output_tokens: u64,
    pub model_calls: u64,
    pub correction_input_tokens: u64,
    pub correction_output_tokens: u64,
    pub correction_calls: u64,
    pub tool_calls: u64,
    pub tool_result_bytes: u64,
}

impl Default for FunctionLimits {
    fn default() -> Self {
        Self {
            timeout: None,
            model_input_tokens: None,
            model_output_tokens: 100_000,
            model_calls: 100,
            correction_input_tokens: 100_000,
            correction_output_tokens: 10_000,
            correction_calls: 20,
            tool_calls: 500,
            tool_result_bytes: MAX_TOOL_RESULT_BYTES,
        }
    }
}

impl FunctionLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.model_input_tokens
                .is_none_or(|limit| limit > 0 && limit <= i64::MAX as u64)
                && self.model_output_tokens > 0
                && self.model_calls > 0
                && self.correction_input_tokens > 0
                && self.correction_output_tokens > 0
                && self.correction_calls > 0
                && self.tool_calls > 0
                && self.tool_result_bytes > 0
                && self.model_output_tokens <= i64::MAX as u64
                && self.model_calls <= i64::MAX as u64
                && self.correction_input_tokens <= i64::MAX as u64
                && self.correction_output_tokens <= i64::MAX as u64
                && self.correction_calls <= i64::MAX as u64
                && self.tool_calls <= i64::MAX as u64,
            "Function limits must be positive signed 64-bit values"
        );
        ensure!(
            self.tool_result_bytes <= MAX_TOOL_RESULT_BYTES,
            "tool_result_bytes cannot exceed the runtime maximum"
        );
        Ok(())
    }

    pub fn agent_run_limits(self) -> AgentRunLimits {
        AgentRunLimits {
            deadline_ms: self
                .timeout
                .map_or(i64::MAX, |timeout| timeout.as_millis() as i64),
            model_input_tokens: self.model_input_tokens,
            model_output_tokens: self.model_output_tokens,
            model_calls: self.model_calls,
            correction_input_tokens: self.correction_input_tokens,
            correction_output_tokens: self.correction_output_tokens,
            correction_calls: self.correction_calls,
            tool_calls: self.tool_calls,
            tool_result_bytes: self.tool_result_bytes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvalidModelOutputRecovery {
    pub model: EntityRef,
    #[serde(default)]
    pub inference: FunctionInference,
    pub max_attempts: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionRecovery {
    pub invalid_model_output: Option<InvalidModelOutputRecovery>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilePermission {
    #[default]
    None,
    Read,
    Write,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FunctionPermissions {
    pub files: FilePermission,
    pub commands: Vec<String>,
    pub terminal: bool,
    pub search: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionToolOutputPresentation {
    pub lines: u32,
    pub chars: u32,
}

impl Default for FunctionToolOutputPresentation {
    fn default() -> Self {
        Self {
            lines: 10,
            chars: 500,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionPresentation {
    pub tool_output: FunctionToolOutputPresentation,
    pub tool: FunctionToolPresentation,
    pub colors: PresentationColors,
    pub model_generation: FunctionModelGenerationPresentation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionToolPresentation {
    pub frame: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionModelGenerationPresentation {
    pub details: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionManifest {
    pub schema: String,
    pub id: PackageId,
    pub version: PackageVersion,
    #[serde(default)]
    pub description: Option<String>,
    pub agent: EntityRef,
    pub model: EntityRef,
    #[serde(default)]
    pub skills: Vec<EntityRef>,
    #[serde(default)]
    pub extensions: Vec<EntityRef>,
    #[serde(default = "default_working_directory")]
    pub working_directory: String,
    #[serde(default)]
    pub permissions: FunctionPermissions,
    #[serde(default)]
    pub inference: FunctionInference,
    #[serde(default)]
    pub recovery: FunctionRecovery,
    #[serde(default)]
    pub limits: FunctionLimits,
    pub presentation: FunctionPresentation,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionRequirements {
    pub function_id: PackageId,
    pub function_version: PackageVersion,
    pub agent: EntityRef,
    pub model: EntityRef,
    pub skills: Vec<EntityRef>,
    pub extensions: Vec<EntityRef>,
    pub tools: Vec<ToolId>,
    pub working_directory: RelativePath,
    pub authority: AuthorityGrantSet,
    pub inference: FunctionInference,
    pub recovery: FunctionRecovery,
    pub limits: FunctionLimits,
    pub presentation: FunctionPresentation,
}

impl FunctionManifest {
    pub fn parse(document: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(document).context("invalid FUNCTION.toml")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == FUNCTION_SCHEMA,
            "unsupported Function schema"
        );
        if let Some(description) = &self.description {
            ensure!(
                !description.trim().is_empty(),
                "Function description cannot be empty when present"
            );
        }
        self.agent.validate().context("invalid Function Agent")?;
        self.model.validate().context("invalid Function Model")?;
        if let Some(recovery) = &self.recovery.invalid_model_output {
            recovery
                .model
                .validate()
                .context("invalid recovery Model")?;
            ensure!(
                (1..=16).contains(&recovery.max_attempts),
                "invalid-model-output recovery max_attempts must be between 1 and 16"
            );
            recovery.inference.generation.validate()?;
            recovery
                .inference
                .reasoning
                .validate(recovery.inference.generation.max_output_tokens)?;
            recovery.inference.load.validate()?;
            recovery.inference.service.validate()?;
            ensure!(
                recovery.inference.adapters.is_empty(),
                "invalid-model-output recovery adapters are not supported"
            );
        }
        for skill in &self.skills {
            skill.validate().context("invalid Function Skill")?;
        }
        for extension in &self.extensions {
            extension.validate().context("invalid Function Extension")?;
        }
        self.validate_permissions()?;
        let _: RelativePath = self
            .working_directory
            .clone()
            .try_into()
            .map_err(anyhow::Error::msg)?;
        self.inference.generation.validate()?;
        self.inference
            .reasoning
            .validate(self.inference.generation.max_output_tokens)?;
        self.inference.load.validate()?;
        if let Some(speculative) = self.inference.speculative {
            speculative.validate(&self.inference.load)?;
        }
        self.inference.service.validate()?;
        for adapter in &self.inference.adapters {
            adapter.model.validate()?;
            ensure!(
                adapter.scale.is_finite() && adapter.scale > 0.0,
                "adapter scale must be positive and finite"
            );
        }
        let references = std::iter::once(("agent", &self.agent))
            .chain(std::iter::once(("model", &self.model)))
            .chain(
                self.recovery
                    .invalid_model_output
                    .iter()
                    .map(|value| ("model", &value.model)),
            )
            .chain(self.skills.iter().map(|value| ("skill", value)))
            .chain(self.extensions.iter().map(|value| ("extension", value)))
            .chain(
                self.inference
                    .adapters
                    .iter()
                    .map(|value| ("model", &value.model)),
            )
            .map(|(kind, reference)| (kind, reference.id.clone(), reference.version.clone()))
            .collect::<BTreeSet<_>>();
        let reference_count = 2
            + self.skills.len()
            + self.extensions.len()
            + self.inference.adapters.len()
            + usize::from(self.recovery.invalid_model_output.is_some());
        ensure!(
            references.len() == reference_count,
            "Function declares the same exact entity more than once"
        );
        self.limits.validate()?;
        ensure!(
            self.presentation.tool_output.lines > 0,
            "presentation.tool_output.lines must be greater than zero"
        );
        ensure!(
            self.presentation.tool_output.chars > 0,
            "presentation.tool_output.chars must be greater than zero"
        );
        self.presentation
            .colors
            .validate()
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    pub fn requirements(&self) -> Result<FunctionRequirements> {
        self.validate()?;
        let (tools, authority) = self.generated_permissions()?;
        Ok(FunctionRequirements {
            function_id: self.id.clone(),
            function_version: self.version.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            skills: self.skills.clone(),
            extensions: self.extensions.clone(),
            tools,
            working_directory: self
                .working_directory
                .clone()
                .try_into()
                .map_err(anyhow::Error::msg)?,
            authority,
            inference: self.inference.clone(),
            recovery: self.recovery.clone(),
            limits: self.limits,
            presentation: self.presentation.clone(),
        })
    }

    fn validate_permissions(&self) -> Result<()> {
        let permissions = &self.permissions;
        ensure!(
            permissions.commands.len() <= 256,
            "permissions.commands contains too many entries"
        );
        let mut commands = permissions.commands.clone();
        commands.sort();
        ensure!(
            commands.windows(2).all(|pair| pair[0] != pair[1]),
            "permissions.commands contains duplicates"
        );
        ensure!(
            commands.iter().all(|command| {
                !command.is_empty()
                    && command.len() <= 128
                    && command.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')
                    })
            }),
            "permissions.commands must contain logical executable names"
        );
        ensure!(
            !permissions.terminal || !commands.is_empty(),
            "permissions.terminal requires at least one permissions.commands entry"
        );
        self.generated_permissions().map(|_| ())
    }

    fn generated_permissions(&self) -> Result<(Vec<ToolId>, AuthorityGrantSet)> {
        let extension_ids = self
            .extensions
            .iter()
            .map(|extension| extension.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut tools = Vec::new();
        let mut authority = Vec::new();
        let workspace_scope = |executables: Vec<String>| {
            CanonicalJson::new(serde_json::json!({
                "root": "workspace",
                "executables": executables,
            }))
            .map_err(anyhow::Error::msg)
        };
        match self.permissions.files {
            FilePermission::None => {}
            FilePermission::Read => {
                ensure!(
                    extension_ids.contains("agentlibre.builtins"),
                    "permissions.files requires the agentlibre.builtins Extension"
                );
                tools.push(ToolId::new("agentlibre.builtins:fs_read")?);
            }
            FilePermission::Write => {
                ensure!(
                    extension_ids.contains("agentlibre.builtins"),
                    "permissions.files requires the agentlibre.builtins Extension"
                );
                tools.push(ToolId::new("agentlibre.builtins:fs_read")?);
                tools.push(ToolId::new("agentlibre.builtins:fs_apply_patch")?);
                authority.push(AuthorityGrant {
                    effect: agl_core::EffectId::new("agentlibre.builtins:filesystem_write")?,
                    scope: CanonicalJson::new(serde_json::json!({"root":"workspace"}))
                        .map_err(anyhow::Error::msg)?,
                });
            }
        }
        if !self.permissions.commands.is_empty() {
            ensure!(
                extension_ids.contains("agentlibre.execution"),
                "permissions.commands requires the agentlibre.execution Extension"
            );
            let mut commands = self.permissions.commands.clone();
            commands.sort();
            tools.push(ToolId::new("agentlibre.execution:command.exec")?);
            authority.push(AuthorityGrant {
                effect: agl_core::EffectId::new("agentlibre.execution:process.execute")?,
                scope: workspace_scope(commands.clone())?,
            });
            if self.permissions.terminal {
                tools.push(ToolId::new("agentlibre.execution:terminal.session")?);
                authority.push(AuthorityGrant {
                    effect: agl_core::EffectId::new("agentlibre.execution:terminal.control")?,
                    scope: workspace_scope(commands)?,
                });
            }
        }
        if self.permissions.search {
            ensure!(
                extension_ids.contains("agentlibre.searxng"),
                "permissions.search requires the agentlibre.searxng Extension"
            );
            tools.push(ToolId::new("agentlibre.searxng:search")?);
            authority.push(AuthorityGrant {
                effect: agl_core::EffectId::new("agentlibre.searxng:query")?,
                scope: CanonicalJson::new(serde_json::json!({
                    "service": "ayeque-search",
                    "sources": ["web", "wikipedia"],
                }))
                .map_err(anyhow::Error::msg)?,
            });
        }
        tools.sort();
        authority.sort_by(|left, right| {
            (
                left.effect.as_str(),
                serde_json::to_string(&left.scope).expect("CanonicalJson serializes"),
            )
                .cmp(&(
                    right.effect.as_str(),
                    serde_json::to_string(&right.scope).expect("CanonicalJson serializes"),
                ))
        });
        Ok((tools, AuthorityGrantSet(authority)))
    }
}

fn default_working_directory() -> String {
    ".".to_owned()
}

fn validate_relative(value: &str, allow_dot: bool) -> Result<()> {
    if allow_dot && value == "." {
        return Ok(());
    }
    let path = Path::new(value);
    ensure!(
        !path.as_os_str().is_empty()
            && !path.is_absolute()
            && !value.ends_with('/')
            && !value.contains('\\')
            && !value.contains(':')
            && !value.chars().any(char::is_control),
        "path must be non-empty and relative"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_)))
            && value
                .split('/')
                .all(|component| !component.is_empty() && !matches!(component, "." | "..")),
        "path must stay inside its root"
    );
    Ok(())
}

pub fn validate_git_source(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && !value.starts_with('-')
            && value.len() <= 4096
            && !value.chars().any(char::is_control),
        "Git source is empty, option-shaped, or oversized"
    );
    if let Ok(url) = Url::parse(value) {
        ensure!(
            matches!(url.scheme(), "https" | "ssh"),
            "Git source requires HTTPS or SSH"
        );
        ensure!(
            url.password().is_none(),
            "Git source cannot contain a password"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "Git source cannot contain query parameters or fragments"
        );
        if url.scheme() == "https" {
            ensure!(
                url.username().is_empty(),
                "HTTPS Git source cannot contain user information"
            );
        }
        ensure!(url.host_str().is_some(), "Git source requires a host");
        return Ok(());
    }
    let (authority, path) = value
        .split_once(':')
        .context("Git source is not an allowed URL or scp-like SSH source")?;
    let (user, host) = authority
        .split_once('@')
        .context("scp-like Git source requires user@host:path")?;
    ensure!(
        !user.is_empty()
            && !host.is_empty()
            && !path.is_empty()
            && !path.starts_with('/')
            && !path.contains(':')
            && !value.contains(['?', '#'])
            && user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && !host.starts_with('-'),
        "scp-like Git source is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> String {
        r##"schema = "agentlibre.function/v2"
id = "coder"
version = "1.0.0"
agent = { id = "coder-agent", version = "1.0.0" }
model = { id = "coder-model", version = "1.0.0" }
[presentation.tool_output]
lines = 10
chars = 500

[presentation.tool]
frame = true

[presentation.colors]
rule = "dim"
run = "bold #FF00FF"
run_id = "bold #FFFFFF"
status_success = "bold #7BD88F"
status_failure = "bold #FF0000"
status_pending = "bold #FFFF00"
operation = "bold #00FFFF"
tool = "bold #00FFFF"
ordinal = "dim"
field = "bold #8AA2D8"
muted = "dim"
json_key = "#00FFFF"
json_string = "#7BD88F"
json_number = "#FFFF00"
json_boolean = "#FF00FF"
json_null = "dim"
markdown_heading = "bold #00FFFF"
markdown_code = "dim"
markdown_inline_code = "#FFFF00"
markdown_strong = "bold"
markdown_emphasis = "italic"
markdown_link = "#8AA2D8"
markdown_quote = "dim"
markdown_bullet = "#00FFFF"
markdown_rule = "dim"
input_rule = "oklch(0.439 0 0)"
input_background = "oklch(0.269 0 0)"
input_prompt = "bold oklch(0.718 0.202 349.761)"
input_hint = "dim oklch(0.823 0.12 346.018)"
input_text = "oklch(0.936 0.032 17.717)"
input_activity = "oklch(0.823 0.12 346.018)"
input_selected = "bold oklch(0.518 0.253 323.949)"

[presentation.model_generation]
details = false
"##
        .to_owned()
    }

    #[test]
    fn minimal_function_materializes_selected_defaults() {
        let manifest = FunctionManifest::parse(&minimal()).unwrap();
        let requirements = manifest.requirements().unwrap();
        assert_eq!(requirements.inference.generation.max_output_tokens, 8192);
        assert_eq!(requirements.inference.generation.seed, 1);
        assert_eq!(
            requirements.inference.reasoning,
            FunctionReasoning::default()
        );
        assert_eq!(requirements.limits.timeout, None);
        assert_eq!(requirements.limits.tool_result_bytes, 65_536);
        assert_eq!(requirements.limits.model_input_tokens, None);
        assert_eq!(requirements.presentation.tool_output.lines, 10);
        assert_eq!(requirements.presentation.tool_output.chars, 500);
        assert!(requirements.presentation.tool.frame);
        assert_eq!(requirements.presentation.colors.tool, "bold #00FFFF");
        assert!(!requirements.presentation.model_generation.details);
        assert_eq!(
            requirements.inference.service.idle_timeout.as_millis(),
            15 * 60 * 1000
        );
    }

    #[test]
    fn tool_output_presentation_is_typed_and_positive() {
        let document = minimal().replace("lines = 10\nchars = 500", "lines = 20\nchars = 1200");
        let manifest = FunctionManifest::parse(&document).unwrap();
        assert_eq!(manifest.presentation.tool_output.lines, 20);
        assert_eq!(manifest.presentation.tool_output.chars, 1200);
        for invalid in [
            document.replace("lines = 20", "lines = 0"),
            document.replace("chars = 1200", "chars = 0"),
        ] {
            assert!(FunctionManifest::parse(&invalid).is_err());
        }
    }

    #[test]
    fn mtp_requires_gpu_and_has_a_strict_shape() {
        let document = minimal()
            + r#"
[inference.speculative]
mode = "mtp"
max_draft_tokens = 3
kv_cache_type_k = "q4_0"
kv_cache_type_v = "q4_0"

[inference.load]
accelerator = "require_gpu"
gpu_layers = "all"
"#;
        let manifest = FunctionManifest::parse(&document).unwrap();
        assert_eq!(manifest.inference.speculative.unwrap().max_draft_tokens, 3);
        for invalid in [
            document.replace("max_draft_tokens = 3", "max_draft_tokens = 0"),
            document.replace("gpu_layers = \"all\"", "gpu_layers = 1"),
            document.replace("accelerator = \"require_gpu\"", "accelerator = \"cpu\""),
            document.replace("mode = \"mtp\"", "mode = \"draft\""),
        ] {
            assert!(FunctionManifest::parse(&invalid).is_err());
        }
    }

    #[test]
    fn aggregate_input_limit_is_optional_but_explicit_values_are_bounded() {
        for value in [0, i64::MAX as u64 + 1] {
            assert!(
                FunctionManifest::parse(
                    &(minimal() + &format!("\n[limits]\nmodel_input_tokens = {value}\n"))
                )
                .is_err()
            );
        }
        let manifest =
            FunctionManifest::parse(&(minimal() + "\n[limits]\nmodel_input_tokens = 1234\n"))
                .unwrap();
        assert_eq!(
            manifest.requirements().unwrap().limits.model_input_tokens,
            Some(1234)
        );
        assert!(
            !toml::to_string(&FunctionLimits::default())
                .unwrap()
                .contains("model_input_tokens")
        );
    }

    #[test]
    fn invalid_model_output_recovery_has_an_explicit_cpu_model_profile() {
        let document = minimal()
            + r#"
[recovery.invalid_model_output]
model = { id = "small-corrector", version = "1.0.0" }
max_attempts = 2

[recovery.invalid_model_output.inference.generation]
max_output_tokens = 512
temperature = 0.0

[recovery.invalid_model_output.inference.load]
accelerator = "cpu"
gpu_layers = 0
threads = 8
"#;
        let manifest = FunctionManifest::parse(&document).unwrap();
        let recovery = manifest.recovery.invalid_model_output.unwrap();
        assert_eq!(recovery.model.id.as_str(), "small-corrector");
        assert_eq!(recovery.max_attempts, 2);
        assert_eq!(
            recovery.inference.load.accelerator,
            AcceleratorPreference::Cpu
        );
        assert_eq!(
            recovery.inference.load.gpu_layers,
            Some(GpuLayers::Count(0))
        );
        assert_eq!(recovery.inference.generation.max_output_tokens, 512);
    }

    #[test]
    fn reasoning_requires_one_strict_bounded_shape() {
        let enabled = minimal()
            + "\n[inference.reasoning]\nenabled = true\nmax_tokens = 4096\ndefault = \"xhigh\"\npreserve = true\n";
        let manifest = FunctionManifest::parse(&enabled).unwrap();
        assert_eq!(manifest.inference.reasoning.max_tokens, Some(4096));
        assert_eq!(
            manifest.inference.reasoning.default,
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(manifest.inference.reasoning.preserve, Some(true));

        for invalid in [
            "\n[inference.reasoning]\nenabled = true\npreserve = true\n",
            "\n[inference.reasoning]\nenabled = true\nmax_tokens = 0\npreserve = true\n",
            "\n[inference.reasoning]\nenabled = false\nmax_tokens = 1\n",
            "\n[inference.reasoning]\nenabled = true\nmax_tokens = 8193\npreserve = true\n",
            "\n[inference.reasoning]\nenabled = true\nmax_tokens = 2147483648\npreserve = true\n",
            "\n[inference.reasoning]\nenabled = true\nmax_tokens = 1\npreserve = true\nraw = true\n",
            "\n[inference.reasoning]\nenabled = true\nmax_tokens = 1\n",
            "\n[inference.reasoning]\nenabled = true\nmax_tokens = 1\neffort = \"low\"\npreserve = true\n",
        ] {
            assert!(FunctionManifest::parse(&(minimal() + invalid)).is_err());
        }
    }

    #[test]
    fn raw_backend_and_inexact_source_fields_are_rejected() {
        assert!(FunctionManifest::parse(&(minimal() + "backend_args = [\"--unsafe\"]\n")).is_err());
        assert!(FunctionManifest::parse(
            &minimal().replace(
                "agent = { id = \"coder-agent\", version = \"1.0.0\" }",
                "agent = { id = \"coder-agent\", version = \"1.0.0\", git = \"https://example.invalid/repo.git\" }"
            )
        )
        .is_err());
        assert!(validate_git_source("file:///tmp/repo").is_err());
        assert!(validate_git_source("https://token@example.invalid/repo").is_err());
        assert!(validate_git_source("git@example.invalid:owner/repo.git").is_ok());
    }

    #[test]
    fn git_transport_allowlist_rejects_local_credentials_and_option_shapes() {
        for allowed in [
            "https://example.invalid/owner/repo.git",
            "ssh://git@example.invalid/owner/repo.git",
            "git@example.invalid:owner/repo.git",
        ] {
            assert!(validate_git_source(allowed).is_ok(), "{allowed}");
        }
        for rejected in [
            "http://example.invalid/repo.git",
            "git://example.invalid/repo.git",
            "file:///tmp/repo",
            "/tmp/repo",
            "../repo",
            "ext::helper repo",
            "-c",
            "https://example.invalid/repo.git?token=secret",
            "ssh://git@example.invalid/repo.git#branch",
            "git@-oProxyCommand:owner/repo.git",
            "git@example.invalid:owner/repo.git?token=secret",
            "https://token@example.invalid/repo.git",
            "https://token:secret@example.invalid/repo.git",
        ] {
            assert!(validate_git_source(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn duplicate_entities_and_contradictory_physical_settings_are_rejected() {
        assert!(
            FunctionManifest::parse(
                &(minimal()
                    + "\n[[inference.adapters]]\nmodel = { id = \"coder-model\", version = \"1.0.0\" }\nscale = 1.0\n")
            )
            .is_err()
        );
        assert!(
            FunctionManifest::parse(
                &(minimal() + "\n[inference.load]\naccelerator = \"cpu\"\ngpu_layers = \"all\"\n")
            )
            .is_err()
        );
        assert!(
            FunctionManifest::parse(
                &(minimal()
                    + "\n[inference.load]\ndevices = [{ kind = \"pci\", address = \"0000:01:00.0\" }, { kind = \"pci\", address = \"0000:01:00.0\" }]\n")
            )
            .is_err()
        );
        assert!(
            FunctionManifest::parse(
                &(minimal()
                    + "\n[inference.load]\ndevices = [{ kind = \"pci\", address = \"0000:01:00.0\" }]\ntensor_split = [1.0]\nsplit_mode = \"none\"\n")
            )
            .is_err()
        );
    }

    #[test]
    fn permissions_generate_tools_and_workspace_authority() {
        let document = minimal().replace(
            "[presentation.tool_output]",
            r#"extensions = [
    { id = "agentlibre.builtins", version = "1.0.0" },
    { id = "agentlibre.execution", version = "1.0.0" },
]

[presentation.tool_output]"#,
        ) + r#"
[permissions]
files = "write"
commands = ["rg", "find"]
terminal = true
"#;
        let requirements = FunctionManifest::parse(&document)
            .unwrap()
            .requirements()
            .unwrap();
        assert_eq!(
            requirements
                .tools
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![
                "agentlibre.builtins:fs_apply_patch",
                "agentlibre.builtins:fs_read",
                "agentlibre.execution:command.exec",
                "agentlibre.execution:terminal.session",
            ]
        );
        assert_eq!(requirements.authority.0.len(), 3);
        assert!(
            FunctionManifest::parse(&(minimal() + "\n[permissions]\nterminal = true\n")).is_err()
        );
        assert!(
            FunctionManifest::parse(
                &(minimal() + "\n[permissions]\ncommands = [\"rg\", \"rg\"]\n")
            )
            .is_err()
        );
        assert!(
            FunctionManifest::parse(&(minimal() + "\ntools = [\"agentlibre.builtins:fs_read\"]\n"))
                .is_err()
        );
    }
}
