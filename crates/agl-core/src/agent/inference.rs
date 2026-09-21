use serde::{Deserialize, Serialize};

use super::{ModelDefinitionRef, PackageDigest};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelArtifactKind {
    Gguf,
    LoraAdapter,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactRef {
    pub kind: ModelArtifactKind,
    pub url: String,
    pub digest: PackageDigest,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDialect {
    Generic,
    Qwen3,
    Gemma4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFormat {
    StructuredToolCalls,
    HermesJson,
    GemmaAgentCall,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationSettings {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReasoningSelection {
    Disabled,
    Enabled {
        max_tokens: u32,
        effort: Option<ReasoningEffort>,
        preserve: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    Xhigh,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhysicalDeviceSelector {
    Pci { address: String },
    Uuid { uuid: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    None,
    Layer,
    Row,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "count", rename_all = "snake_case")]
pub enum GpuLayerSelection {
    All,
    Count(u32),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpeculativeSelection {
    Mtp {
        max_draft_tokens: u32,
        kv_cache_type_k: KvCacheType,
        kv_cache_type_v: KvCacheType,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLoadSelection {
    pub context_tokens: u32,
    pub batch_size: u32,
    pub ubatch_size: u32,
    pub threads: u32,
    pub threads_batch: u32,
    pub gpu_layers: GpuLayerSelection,
    pub devices: Vec<PhysicalDeviceSelector>,
    pub split_mode: SplitMode,
    pub main_gpu: Option<usize>,
    pub tensor_split: Vec<f64>,
    pub mmap: bool,
    pub mlock: bool,
    pub flash_attention: bool,
    pub kv_cache_type_k: KvCacheType,
    pub kv_cache_type_v: KvCacheType,
    pub engine_build_digest: PackageDigest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdapterSelection {
    pub model: ModelDefinitionRef,
    pub artifact: ModelArtifactRef,
    pub scale: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelServiceSelection {
    pub key: PackageDigest,
    pub slots: u32,
    pub queue_capacity: u32,
    pub continuous_batching: bool,
    pub idle_timeout_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRuntimeSelection {
    pub artifact: ModelArtifactRef,
    pub dialect: ModelDialect,
    pub tool_call_format: ToolCallFormat,
    pub generation: GenerationSettings,
    pub reasoning: ReasoningSelection,
    pub load: ModelLoadSelection,
    pub speculative: Option<SpeculativeSelection>,
    pub adapters: Vec<ModelAdapterSelection>,
    pub service: ModelServiceSelection,
}

impl ModelRuntimeSelection {
    pub fn shares_service_with(&self, other: &Self) -> bool {
        same_artifact(&self.artifact, &other.artifact)
            && self.load == other.load
            && self.speculative == other.speculative
            && self.adapters.len() == other.adapters.len()
            && self
                .adapters
                .iter()
                .zip(&other.adapters)
                .all(|(left, right)| {
                    same_artifact(&left.artifact, &right.artifact) && left.scale == right.scale
                })
            && self.service.key == other.service.key
            && self.service.slots == other.service.slots
            && self.service.queue_capacity == other.service.queue_capacity
            && self.service.continuous_batching == other.service.continuous_batching
    }
}

fn same_artifact(left: &ModelArtifactRef, right: &ModelArtifactRef) -> bool {
    left.kind == right.kind && left.digest == right.digest && left.bytes == right.bytes
}
