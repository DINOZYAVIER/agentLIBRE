//! Portable Function authoring contracts.

#![forbid(unsafe_code)]

mod manifest;

pub use manifest::{
    AcceleratorPreference, DeviceSelector, EntityRef, FUNCTION_FILE_NAME, FUNCTION_SCHEMA,
    FunctionAdapter, FunctionInference, FunctionLimits, FunctionLoad, FunctionManifest,
    FunctionReasoning, FunctionRequirements, FunctionService, FunctionSpeculative,
    FunctionSpeculativeMode, GenerationDefaults, GpuLayers, KvCacheType, SplitMode, TypedDuration,
    validate_git_source,
};
