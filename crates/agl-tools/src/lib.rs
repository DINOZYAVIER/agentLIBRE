pub mod fs;
pub mod guards;
mod hook;
mod ids;
pub mod memory;
pub mod notes;
mod provider;
mod registry;
mod tool;

pub use fs::{CoreTools, FS_EDIT_TOOL_ID, FS_LIST_TOOL_ID, FS_READ_TOOL_ID, FS_SEARCH_TOOL_ID};
pub use hook::{
    HookBatchRequest, HookBatchResult, HookEvent, HookInput, HookMessage, HookResult, HookStatus,
};
pub use ids::{HookId, IdKind, SkillId, ToolId, ToolProviderId, ToolProviderIdError};
pub use memory::{MEMORY_SUGGEST_TOOL_ID, MemoryTools};
pub use notes::{
    NOTES_ADD_TOOL_ID, NOTES_LINK_TOOL_ID, NOTES_SEARCH_TOOL_ID, NOTES_SHOW_TOOL_ID,
    NOTES_UPDATE_TOOL_ID, NotesTools,
};
pub use provider::{
    HookDeclaration, ToolCapability, ToolDeclaration, ToolOperationKind, ToolProviderDeclaration,
    ToolProviderDeclarationError, ToolProviderSource, ToolProviderTrust, ToolStateEffect,
};
pub use registry::{ToolCatalog, ToolCatalogError, ToolDispatchError, ToolRuntime};
pub use tool::{ToolHandler, ToolInput, ToolOutput};

#[cfg(test)]
mod tests;
