pub mod cron;
pub mod fs;
pub mod guards;
mod hook;
mod ids;
pub mod matrix;
pub mod matrix_delivery;
pub mod memory;
pub mod notes;
pub mod permissions;
mod provider;
mod registry;
pub mod repo;
pub mod skills;
pub mod store;
mod tool;

pub use cron::{
    CRON_ADD_TOOL_ID, CRON_DELETE_TOOL_ID, CRON_DISABLE_TOOL_ID, CRON_ENABLE_TOOL_ID,
    CRON_HISTORY_TOOL_ID, CRON_LIST_TOOL_ID, CRON_PREFLIGHT_TOOL_ID, CRON_RUN_TOOL_ID,
    CRON_SHOW_TOOL_ID, CRON_TICK_TOOL_ID, CRON_UPDATE_TOOL_ID, CronTools,
};
pub use fs::{CoreTools, FS_EDIT_TOOL_ID, FS_LIST_TOOL_ID, FS_READ_TOOL_ID, FS_SEARCH_TOOL_ID};
pub use hook::{
    HookBatchRequest, HookBatchResult, HookEvent, HookInput, HookMessage, HookResult, HookStatus,
};
pub use ids::{HookId, IdKind, SkillId, ToolId, ToolProviderId, ToolProviderIdError};
pub use matrix::{MATRIX_OUTBOX_ENQUEUE_TOOL_ID, MATRIX_OUTBOX_STATUS_TOOL_ID, MatrixTools};
pub use matrix_delivery::MATRIX_OUTBOX_DELIVER_TOOL_ID;
pub use memory::{
    MEMORY_ADD_TOOL_ID, MEMORY_APPROVE_TOOL_ID, MEMORY_LIST_TOOL_ID, MEMORY_REJECT_TOOL_ID,
    MEMORY_SEARCH_TOOL_ID, MEMORY_SUGGEST_TOOL_ID, MemoryTools,
};
pub use notes::{
    NOTES_ADD_TOOL_ID, NOTES_DELETE_TOOL_ID, NOTES_LINK_TOOL_ID, NOTES_REMEMBER_TOOL_ID,
    NOTES_SEARCH_TOOL_ID, NOTES_SHOW_TOOL_ID, NOTES_UPDATE_TOOL_ID, NotesTools,
};
pub use permissions::{
    PERMISSIONS_GRANT_TOOL_ID, PERMISSIONS_REQUEST_TOOL_ID, PERMISSIONS_REVOKE_TOOL_ID,
    PERMISSIONS_STATUS_TOOL_ID, PermissionRuntimeStatus, PermissionTools,
};
pub use provider::{
    HookDeclaration, ToolCapability, ToolDeclaration, ToolOperationKind, ToolProviderDeclaration,
    ToolProviderDeclarationError, ToolProviderSource, ToolProviderTrust, ToolStateEffect,
};
pub use registry::{ToolCatalog, ToolCatalogError, ToolDispatchError, ToolRuntime};
pub use repo::{
    REPO_EXPORT_PROFILE_TOOL_ID, REPO_HOOKS_STATUS_TOOL_ID, REPO_IMPORT_PROFILE_TOOL_ID,
    REPO_INIT_TOOL_ID, REPO_INSTALL_HOOKS_TOOL_ID, REPO_STATUS_TOOL_ID, RepoTools,
};
pub use skills::{
    SKILL_INSPECT_TOOL_ID, SKILL_LIST_TOOL_ID, SKILL_LOCK_TOOL_ID, SKILL_REVOKE_TOOL_ID,
    SKILL_STATUS_TOOL_ID, SKILL_TRUST_TOOL_ID, SKILL_VERIFY_TOOL_ID,
};
pub use store::{STORE_EXPORT_TOOL_ID, STORE_MIGRATE_TOOL_ID, STORE_STATUS_TOOL_ID, StoreTools};
pub use tool::{ToolHandler, ToolInput, ToolOutput};

#[cfg(test)]
mod tests;
