//! Package loading, Function activation, inference, and model-service reuse.

mod activation;
pub mod agent;
pub mod extension;
mod forge;
pub mod function;
pub mod inference;
pub mod model;
pub mod package;
pub mod planner;
pub mod skill;
mod workspace_artifacts;

pub use activation::{
    ActivatedFunction, ActivationProgress, ModelServiceDisposition, RuntimeConfig, RuntimeHandle,
    RuntimeService, host_memory_budget,
};
pub use forge::{FunctionIdentity, function_identity, resolve_function_requirement};
pub use planner::PlannerWorkspaceBoundary;
pub use workspace_artifacts::{
    ResolvedWorkspaceArtifact, WORKSPACE_CONFIG_FILE_NAME, WorkspaceArtifacts,
    resolve_workspace_artifacts,
};
