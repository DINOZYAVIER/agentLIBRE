use std::fmt::{self, Display, Formatter};

use agl_extension::{
    Extension, ExtensionBindings, ExtensionDefinition, ExtensionHost, StaticExtensionFactory,
};
use agl_kernel::{HookBinding, HostBindingId, HostBindingRequirement, ToolBinding, ToolId};

const API_MAJOR: u32 = 1;

#[derive(Debug)]
pub struct FirstPartyBindError {
    extension_id: &'static str,
}

impl Display for FirstPartyBindError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "host binding for Extension `{}` is absent or has the wrong type",
            self.extension_id
        )
    }
}

impl std::error::Error for FirstPartyBindError {}

fn binding_id(extension_id: &'static str) -> HostBindingId {
    HostBindingId::new(extension_id).expect("first-party Extension ID is a valid host binding ID")
}

fn definition(
    descriptor: agl_kernel::ExtensionDescriptor,
    extension_id: &'static str,
) -> ExtensionDefinition {
    ExtensionDefinition::from_descriptor(
        API_MAJOR,
        descriptor.with_host_binding(HostBindingRequirement::new(
            binding_id(extension_id),
            API_MAJOR,
        )),
    )
    .expect("first-party Extension definition is valid")
}

fn tool_bindings(
    host: &ExtensionHost,
    extension_id: &'static str,
    tool_ids: &'static [&'static str],
) -> Result<ExtensionBindings, FirstPartyBindError> {
    let handler = host
        .shared_tool_handler(&binding_id(extension_id))
        .ok_or(FirstPartyBindError { extension_id })?;
    let tools = tool_ids.iter().map(|tool_id| {
        ToolBinding::from_shared(
            ToolId::new(*tool_id).expect("first-party Tool ID is valid"),
            handler.clone(),
        )
    });
    Ok(ExtensionBindings::new(tools, []))
}

macro_rules! tool_extension {
    ($type:ident, $factory:ident, $extension_id:path, $declaration:path, [$($tool_id:path),* $(,)?]) => {
        struct $type;

        impl Extension for $type {
            type BindError = FirstPartyBindError;

            fn definition() -> ExtensionDefinition {
                definition($declaration(), $extension_id)
            }

            fn bind(host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
                tool_bindings(host, $extension_id, &[$($tool_id),*])
            }
        }

        pub fn $factory() -> StaticExtensionFactory {
            StaticExtensionFactory::for_extension::<$type>()
        }
    };
}

struct GuardsExtension;

impl Extension for GuardsExtension {
    type BindError = FirstPartyBindError;

    fn definition() -> ExtensionDefinition {
        definition(super::guards::declaration(), super::guards::EXTENSION_ID)
    }

    fn bind(host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        let extension_id = super::guards::EXTENSION_ID;
        let guards = host
            .binding(&binding_id(extension_id))
            .and_then(|binding| binding.downcast_ref::<super::guards::CoreGuards>())
            .ok_or(FirstPartyBindError { extension_id })?;
        let hooks = Self::definition()
            .descriptor()
            .hooks
            .iter()
            .map(|hook| HookBinding::new(hook.id.clone(), guards.clone()))
            .collect::<Vec<_>>();
        Ok(ExtensionBindings::new([], hooks))
    }
}

pub fn guards_extension_factory() -> StaticExtensionFactory {
    StaticExtensionFactory::for_extension::<GuardsExtension>()
}

tool_extension!(
    CronExtension,
    cron_extension_factory,
    super::cron::EXTENSION_ID,
    super::cron::declaration,
    [
        super::CRON_LIST_TOOL_ID,
        super::CRON_SHOW_TOOL_ID,
        super::CRON_HISTORY_TOOL_ID,
        super::CRON_PREFLIGHT_TOOL_ID,
        super::CRON_ADD_TOOL_ID,
        super::CRON_UPDATE_TOOL_ID,
        super::CRON_DELETE_TOOL_ID,
        super::CRON_ENABLE_TOOL_ID,
        super::CRON_DISABLE_TOOL_ID,
        super::CRON_RUN_TOOL_ID,
        super::CRON_TICK_TOOL_ID,
    ]
);
tool_extension!(
    FsExtension,
    fs_extension_factory,
    super::fs::EXTENSION_ID,
    super::fs::declaration,
    [
        super::FS_READ_TOOL_ID,
        super::FS_LIST_TOOL_ID,
        super::FS_SEARCH_TOOL_ID,
        super::FS_APPLY_PATCH_TOOL_ID,
    ]
);
tool_extension!(
    MatrixExtension,
    matrix_extension_factory,
    super::matrix::EXTENSION_ID,
    super::matrix::declaration,
    [
        super::MATRIX_OUTBOX_STATUS_TOOL_ID,
        super::MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
    ]
);
tool_extension!(
    MatrixDeliveryExtension,
    matrix_delivery_extension_factory,
    super::matrix_delivery::EXTENSION_ID,
    super::matrix_delivery::declaration,
    [super::MATRIX_OUTBOX_DELIVER_TOOL_ID]
);
tool_extension!(
    MemoryExtension,
    memory_extension_factory,
    super::memory::EXTENSION_ID,
    super::memory::declaration,
    [
        super::MEMORY_SEARCH_TOOL_ID,
        super::MEMORY_LIST_TOOL_ID,
        super::MEMORY_SUGGEST_TOOL_ID,
        super::MEMORY_ADD_TOOL_ID,
        super::MEMORY_APPROVE_TOOL_ID,
        super::MEMORY_REJECT_TOOL_ID,
    ]
);
tool_extension!(
    NotesExtension,
    notes_extension_factory,
    super::notes::EXTENSION_ID,
    super::notes::declaration,
    [
        super::NOTES_ADD_TOOL_ID,
        super::NOTES_SEARCH_TOOL_ID,
        super::NOTES_SHOW_TOOL_ID,
        super::NOTES_UPDATE_TOOL_ID,
        super::NOTES_LINK_TOOL_ID,
        super::NOTES_DELETE_TOOL_ID,
        super::NOTES_REMEMBER_TOOL_ID,
    ]
);
tool_extension!(
    PermissionsExtension,
    permissions_extension_factory,
    super::permissions::EXTENSION_ID,
    super::permissions::declaration,
    [
        super::PERMISSIONS_STATUS_TOOL_ID,
        super::PERMISSIONS_REQUEST_TOOL_ID,
        super::PERMISSIONS_GRANT_TOOL_ID,
        super::PERMISSIONS_REVOKE_TOOL_ID,
    ]
);
tool_extension!(
    ProcessExtension,
    process_extension_factory,
    super::process::EXTENSION_ID,
    super::process::declaration,
    [
        super::PROCESS_PWD_TOOL_ID,
        super::PROCESS_CD_TOOL_ID,
        super::PROCESS_START_TOOL_ID,
        super::PROCESS_STATUS_TOOL_ID,
        super::PROCESS_READ_TOOL_ID,
        super::PROCESS_WRITE_TOOL_ID,
        super::PROCESS_RESIZE_TOOL_ID,
        super::PROCESS_KILL_TOOL_ID,
        super::PROCESS_EXEC_TOOL_ID,
        super::SHELL_EXEC_TOOL_ID,
    ]
);
tool_extension!(
    RepoExtension,
    repo_extension_factory,
    super::repo::EXTENSION_ID,
    super::repo::declaration,
    [super::ARTIFACT_COMMIT_TOOL_ID, super::TASKS_VERIFY_TOOL_ID,]
);
tool_extension!(
    SkillsExtension,
    skills_extension_factory,
    super::skills::EXTENSION_ID,
    super::skills::declaration,
    [
        super::SKILL_LIST_TOOL_ID,
        super::SKILL_INSPECT_TOOL_ID,
        super::SKILL_STATUS_TOOL_ID,
        super::SKILL_VERIFY_TOOL_ID,
        super::SKILL_TRUST_TOOL_ID,
        super::SKILL_REVOKE_TOOL_ID,
    ]
);
tool_extension!(
    StoreExtension,
    store_extension_factory,
    super::store::EXTENSION_ID,
    super::store::declaration,
    [
        super::STORE_STATUS_TOOL_ID,
        super::STORE_EXPORT_TOOL_ID,
        super::STORE_MIGRATE_TOOL_ID,
    ]
);
