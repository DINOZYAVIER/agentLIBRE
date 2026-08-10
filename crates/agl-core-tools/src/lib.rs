pub mod cron;
mod extensions;
pub mod fs;
pub mod guards;
pub mod matrix;
pub mod matrix_delivery;
pub mod memory;
pub mod notes;
pub mod permissions;
pub mod process;
pub mod repo;
pub mod skills;
pub mod store;
#[cfg(test)]
mod test_support;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use agl_kernel::{
    ExtensionDescriptor, ExtensionWorkflowFragment, ToolErrorClass, ToolWorkflowMapping,
    WorkflowEventId,
};
use agl_kernel::{TOOL_OBSERVATION_APPEND_EVENT_ID, ToolCatalog, ToolCatalogError};
pub use cron::{
    CRON_ADD_TOOL_ID, CRON_DELETE_TOOL_ID, CRON_DISABLE_TOOL_ID, CRON_ENABLE_TOOL_ID,
    CRON_HISTORY_TOOL_ID, CRON_LIST_TOOL_ID, CRON_PREFLIGHT_TOOL_ID, CRON_RUN_TOOL_ID,
    CRON_SHOW_TOOL_ID, CRON_TICK_TOOL_ID, CRON_UPDATE_TOOL_ID, CronTools,
};
pub use extensions::{
    cron_extension_factory, fs_extension_factory, guards_extension_factory,
    matrix_delivery_extension_factory, matrix_extension_factory, memory_extension_factory,
    notes_extension_factory, permissions_extension_factory, process_extension_factory,
    repo_extension_factory, skills_extension_factory, store_extension_factory,
};
pub use fs::{
    CoreTools, FS_APPLY_PATCH_TOOL_ID, FS_LIST_TOOL_ID, FS_READ_TOOL_ID, FS_SEARCH_TOOL_ID,
};
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
pub use process::{
    PROCESS_CD_TOOL_ID, PROCESS_EXEC_TOOL_ID, PROCESS_KILL_TOOL_ID, PROCESS_PWD_TOOL_ID,
    PROCESS_READ_TOOL_ID, PROCESS_RESIZE_TOOL_ID, PROCESS_START_TOOL_ID, PROCESS_STATUS_TOOL_ID,
    PROCESS_TOOL_IDS, PROCESS_WRITE_TOOL_ID, ProcessExecutionAdmission, ProcessExecutionContext,
    ProcessToolRuntimeConfig, ProcessTools, SHELL_EXEC_TOOL_ID,
};
pub use repo::{ARTIFACT_COMMIT_TOOL_ID, RepoTools, TASKS_VERIFY_TOOL_ID};
pub use skills::{
    SKILL_INSPECT_TOOL_ID, SKILL_LIST_TOOL_ID, SKILL_REVOKE_TOOL_ID, SKILL_STATUS_TOOL_ID,
    SKILL_TRUST_TOOL_ID, SKILL_VERIFY_TOOL_ID,
};
pub use store::{STORE_EXPORT_TOOL_ID, STORE_MIGRATE_TOOL_ID, STORE_STATUS_TOOL_ID, StoreTools};
pub(crate) fn parse_tool_args<T>(tool: &str, arguments: Value) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

pub fn builtin_tool_catalog() -> Result<ToolCatalog, ToolCatalogError> {
    ToolCatalog::from_extensions([
        guards::declaration(),
        cron::declaration(),
        fs::declaration(),
        matrix::declaration(),
        matrix_delivery::declaration(),
        memory::declaration(),
        notes::declaration(),
        permissions::declaration(),
        process::declaration(),
        repo::declaration(),
        skills::declaration(),
        store::declaration(),
    ])
}

fn with_observation_workflow(descriptor: ExtensionDescriptor) -> ExtensionDescriptor {
    let event_id = WorkflowEventId::new(TOOL_OBSERVATION_APPEND_EVENT_ID)
        .expect("kernel Tool observation event ID is valid");
    let mappings = descriptor
        .tools
        .iter()
        .flat_map(|tool| {
            let successes = tool.outcomes.iter().map(|outcome| {
                ToolWorkflowMapping::new(tool.id.clone(), outcome.code.clone(), event_id.clone())
            });
            let recoverable_errors = tool
                .errors
                .iter()
                .filter(|error| error.class == ToolErrorClass::Recoverable)
                .map(|error| {
                    ToolWorkflowMapping::new(tool.id.clone(), error.code.clone(), event_id.clone())
                });
            successes.chain(recoverable_errors)
        })
        .collect::<Vec<_>>();
    descriptor.with_workflow(ExtensionWorkflowFragment::new(mappings))
}

#[cfg(test)]
mod tests;
