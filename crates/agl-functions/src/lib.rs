mod loader;
mod locator;
mod manifest;
mod render;
mod runtime;
mod status;
mod subagent;
mod validation;

pub use loader::{LoadedFunction, LoadedSubagent, MarkdownSection, load_function};
pub use locator::{
    FunctionListEntry, FunctionLocator, FunctionSource, ProfileResolution,
    default_local_profile_path, global_functions_root, global_profile_path, list_functions,
    resolve_function_reference, resolve_profile, workspace_functions_root, workspace_profile_path,
};
pub use manifest::*;
pub use render::render_function_context;
pub use runtime::{
    RuntimeFunction, resolve_runtime_function, resolve_runtime_function_allow_missing_profile,
};
pub use status::{FunctionStatusReport, function_status};
pub use subagent::*;
pub use validation::{is_valid_function_id, validate_function_id};

#[cfg(test)]
mod tests;
