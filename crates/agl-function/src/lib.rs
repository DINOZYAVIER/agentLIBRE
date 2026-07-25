mod adapter;
mod loader;
mod locator;
mod manifest;
mod render;
mod runtime;
mod status;
mod subagent;
mod validation;

pub use adapter::{
    FunctionArtifactAdapter, builtin_source, directory_function_source, function_adapter_registry,
    parse_function_envelope,
};
pub use loader::{LoadedFunction, LoadedSubagent, MarkdownSection, load_function};
pub use locator::{
    FunctionListEntry, FunctionPackageLocation, FunctionPackageSource, ProfileResolution,
    default_local_profile_path, global_functions_root, global_profile_path, list_functions,
    resolve_function_package, resolve_profile, workspace_functions_root, workspace_profile_path,
};
pub use manifest::*;
pub use render::render_function_context;
pub use runtime::{
    RuntimeFunction, resolve_runtime_function, resolve_runtime_function_allow_missing_profile,
};
pub use status::{FunctionStatusReport, function_status, function_status_with_model_bindings};
pub use subagent::*;
pub use validation::{is_valid_function_id, validate_function_id};

#[cfg(test)]
mod tests;
