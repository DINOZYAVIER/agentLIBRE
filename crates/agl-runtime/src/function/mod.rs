//! Portable Function authoring contracts.

#![forbid(unsafe_code)]

mod family;
mod manifest;
mod requirement;

pub use family::{
    FUNCTION_FAMILY_FILE_NAME, FUNCTION_FAMILY_KIND, FUNCTION_FAMILY_SCHEMA, FamilyProfile,
    FunctionFamily, discover_function_families,
};
pub use requirement::{FunctionRequirement, parse_function_requirement};

pub use manifest::{
    AcceleratorPreference, DeviceSelector, EntityRef, FUNCTION_FILE_NAME, FUNCTION_SCHEMA,
    FunctionAdapter, FunctionInference, FunctionLimits, FunctionLoad, FunctionManifest,
    FunctionPermissions, FunctionReasoning, FunctionRequirements, FunctionSchedule,
    FunctionService, FunctionSpeculative, FunctionSpeculativeMode, GenerationDefaults, GpuLayers,
    KvCacheType, SplitMode, TypedDuration, validate_git_source,
};
