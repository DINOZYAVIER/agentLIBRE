pub mod fs;
pub mod guards;
mod hook;
mod ids;
mod provider;
mod registry;
mod tool;

pub use fs::{CoreTools, FS_EDIT_TOOL_ID, FS_LIST_TOOL_ID, FS_READ_TOOL_ID, FS_SEARCH_TOOL_ID};
pub use hook::{
    HookBatchRequest, HookBatchResult, HookEvent, HookInput, HookMessage, HookResult, HookStatus,
};
pub use ids::{HookId, IdKind, SkillId, ToolId, ToolProviderId, ToolProviderIdError};
pub use provider::{
    HookDeclaration, ToolCapability, ToolDeclaration, ToolProviderDeclaration,
    ToolProviderDeclarationError, ToolProviderSource, ToolProviderTrust,
};
pub use registry::{ToolCatalog, ToolCatalogError, ToolDispatchError, ToolRuntime};
pub use tool::{ToolHandler, ToolInput, ToolOutput};

#[cfg(test)]
mod tests;
