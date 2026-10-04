//! Package loading, Function activation, inference, and model-service reuse.

mod activation;
pub mod agent;
pub mod extension;
mod forge;
pub mod forge_verification;
pub mod function;
pub mod inference;
pub mod model;
pub mod package;
pub mod skill;
mod workspace_forge;

pub use activation::{
    ActivatedFunction, ActivationProgress, ModelServiceDisposition, RuntimeConfig, RuntimeHandle,
    RuntimeService, host_memory_budget,
};
pub use forge::{
    FunctionIdentity, function_identity, resolve_function_model, resolve_function_requirement,
};
pub use workspace_forge::{
    ForgeDeclaration, ResolvedForgeDeclaration, WORKSPACE_CONFIG_FILE_NAME, WorkspaceForge,
    backend_declarations as workspace_backend_declarations, read_workspace_forge_declarations,
    read_workspace_forge_declarations_at, read_workspace_output_directories,
    resolve_workspace_forge, serialize_workspace_forge_declarations,
};
