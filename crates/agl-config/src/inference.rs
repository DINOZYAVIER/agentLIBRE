use serde::{Deserialize, Serialize};

/// Minimum package-bound context size accepted for an agent Model profile.
pub const MIN_AUTO_CONTEXT_TOKENS: u32 = 32_768;

/// Exact llama.cpp KV-cache element type stored in a Model v3 profile.
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
