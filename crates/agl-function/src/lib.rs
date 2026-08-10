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
    FunctionPackageAdapter, parse_function_envelope, validate_resolved_function_model_contract,
};
#[cfg(test)]
pub(crate) use loader::load_function;
pub use loader::{LoadedFunction, LoadedSubagent, MarkdownSection, load_function_candidate};
#[cfg(test)]
pub(crate) use locator::resolve_function_package;
pub use locator::{
    FunctionListEntry, FunctionPackageLocation, FunctionPackageSource, ProfileResolution,
    default_local_profile_path, global_functions_root, global_profile_path, list_functions,
    resolve_profile, workspace_functions_root, workspace_profile_path,
};
pub use manifest::*;
pub use render::render_function_context;
pub use runtime::{
    RuntimeFunction, runtime_function_from_candidate, runtime_function_from_resolved_graph,
};
#[cfg(test)]
pub(crate) use runtime::{
    resolve_runtime_function, resolve_runtime_function_allow_missing_profile,
};
#[cfg(test)]
pub(crate) use status::function_status;
pub use status::{FunctionStatusReport, function_status_from_loaded};
pub use subagent::*;
pub use validation::{is_valid_function_id, validate_function_id};

#[cfg(test)]
mod tests;
