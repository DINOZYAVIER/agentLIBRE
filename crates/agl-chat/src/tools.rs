use std::path::Path;

use agl_capabilities::{ActionHandler, CapabilityId, ProviderDeclaration};
use agl_tools::{ToolCatalog, ToolCatalogError, ToolRuntime};
use anyhow::{Context, Result};

pub(crate) struct ChatToolRuntimeConfig<'a> {
    pub core_tools: &'a agl_tools::CoreTools,
    pub store_root: &'a Path,
    pub trust_store_path: &'a Path,
    pub workspace_root: &'a Path,
    pub permission_status: agl_tools::PermissionRuntimeStatus,
}

pub(crate) fn chat_extension_catalog() -> Result<ToolCatalog> {
    let mut catalog = ToolCatalog::new();
    catalog
        .register(agl_tools::guards::declaration())
        .context("failed to register builtin core guard provider")?;
    register_chat_tool_providers(&mut catalog)?;
    Ok(catalog)
}

pub(crate) fn chat_tool_runtime(config: ChatToolRuntimeConfig<'_>) -> Result<ToolRuntime> {
    let mut runtime = ToolRuntime::new();
    runtime
        .register_provider(agl_tools::guards::declaration())
        .context("failed to register builtin core guard provider")?;
    for declaration in chat_tool_provider_declarations() {
        runtime.register_provider(declaration)?;
    }

    register_handlers(
        &mut runtime,
        FS_TOOL_IDS,
        config.core_tools.clone(),
        "core filesystem",
    )?;

    register_handlers(
        &mut runtime,
        CRON_TOOL_IDS,
        agl_tools::CronTools::new(config.store_root),
        "builtin cron",
    )?;
    register_handlers(
        &mut runtime,
        MATRIX_TOOL_IDS,
        agl_tools::MatrixTools::new(config.store_root),
        "builtin Matrix",
    )?;
    register_handlers(
        &mut runtime,
        MEMORY_TOOL_IDS,
        agl_tools::MemoryTools::new(config.store_root),
        "builtin memory",
    )?;
    register_handlers(
        &mut runtime,
        NOTES_TOOL_IDS,
        agl_tools::NotesTools::new(config.store_root),
        "builtin notes",
    )?;
    register_handlers(
        &mut runtime,
        PERMISSION_TOOL_IDS,
        agl_tools::PermissionTools::new(config.store_root)
            .with_runtime_status(config.permission_status),
        "builtin permission",
    )?;
    register_handlers(
        &mut runtime,
        REPO_TOOL_IDS,
        agl_tools::RepoTools::new(config.workspace_root),
        "builtin repo",
    )?;
    register_handlers(
        &mut runtime,
        STORE_TOOL_IDS,
        agl_tools::StoreTools::new(config.store_root),
        "builtin store",
    )?;
    register_handlers(
        &mut runtime,
        SKILL_TOOL_IDS,
        agl_host_tools::SkillTools::new(
            config.workspace_root,
            config.trust_store_path,
            env!("CARGO_PKG_VERSION"),
        ),
        "builtin skill",
    )?;

    Ok(runtime)
}

fn register_chat_tool_providers(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    for declaration in chat_tool_provider_declarations() {
        catalog.register(declaration)?;
    }
    Ok(())
}

pub(crate) fn chat_tool_provider_declarations() -> Vec<ProviderDeclaration> {
    vec![
        agl_tools::cron::declaration(),
        agl_tools::fs::declaration(),
        agl_tools::matrix::declaration(),
        agl_tools::memory::declaration(),
        agl_tools::notes::declaration(),
        agl_tools::permissions::declaration(),
        agl_tools::repo::declaration(),
        agl_tools::skills::declaration(),
        agl_tools::store::declaration(),
    ]
}

fn register_handlers<H>(
    runtime: &mut ToolRuntime,
    tool_ids: &[&str],
    handler: H,
    label: &str,
) -> Result<()>
where
    H: ActionHandler + Clone + 'static,
{
    for tool_id in tool_ids {
        runtime
            .register_handler(CapabilityId::new(*tool_id)?, handler.clone())
            .with_context(|| format!("failed to register {label} tool handler {tool_id}"))?;
    }
    Ok(())
}

const FS_TOOL_IDS: &[&str] = &[
    agl_tools::FS_READ_TOOL_ID,
    agl_tools::FS_LIST_TOOL_ID,
    agl_tools::FS_SEARCH_TOOL_ID,
    agl_tools::FS_EDIT_TOOL_ID,
];

const CRON_TOOL_IDS: &[&str] = &[
    agl_tools::CRON_LIST_TOOL_ID,
    agl_tools::CRON_SHOW_TOOL_ID,
    agl_tools::CRON_HISTORY_TOOL_ID,
    agl_tools::CRON_PREFLIGHT_TOOL_ID,
    agl_tools::CRON_ADD_TOOL_ID,
    agl_tools::CRON_UPDATE_TOOL_ID,
    agl_tools::CRON_DELETE_TOOL_ID,
    agl_tools::CRON_ENABLE_TOOL_ID,
    agl_tools::CRON_DISABLE_TOOL_ID,
    agl_tools::CRON_RUN_TOOL_ID,
    agl_tools::CRON_TICK_TOOL_ID,
];

const MATRIX_TOOL_IDS: &[&str] = &[
    agl_tools::MATRIX_OUTBOX_STATUS_TOOL_ID,
    agl_tools::MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
];

const MEMORY_TOOL_IDS: &[&str] = &[
    agl_tools::MEMORY_SEARCH_TOOL_ID,
    agl_tools::MEMORY_LIST_TOOL_ID,
    agl_tools::MEMORY_SUGGEST_TOOL_ID,
    agl_tools::MEMORY_ADD_TOOL_ID,
    agl_tools::MEMORY_APPROVE_TOOL_ID,
    agl_tools::MEMORY_REJECT_TOOL_ID,
];

const NOTES_TOOL_IDS: &[&str] = &[
    agl_tools::NOTES_ADD_TOOL_ID,
    agl_tools::NOTES_SEARCH_TOOL_ID,
    agl_tools::NOTES_SHOW_TOOL_ID,
    agl_tools::NOTES_UPDATE_TOOL_ID,
    agl_tools::NOTES_LINK_TOOL_ID,
    agl_tools::NOTES_DELETE_TOOL_ID,
    agl_tools::NOTES_REMEMBER_TOOL_ID,
];

const PERMISSION_TOOL_IDS: &[&str] = &[
    agl_tools::PERMISSIONS_STATUS_TOOL_ID,
    agl_tools::PERMISSIONS_REQUEST_TOOL_ID,
    agl_tools::PERMISSIONS_GRANT_TOOL_ID,
    agl_tools::PERMISSIONS_REVOKE_TOOL_ID,
];

const REPO_TOOL_IDS: &[&str] = &[
    agl_tools::REPO_STATUS_TOOL_ID,
    agl_tools::REPO_EXPORT_PROFILE_TOOL_ID,
    agl_tools::REPO_HOOKS_STATUS_TOOL_ID,
    agl_tools::REPO_INIT_TOOL_ID,
    agl_tools::REPO_IMPORT_PROFILE_TOOL_ID,
    agl_tools::REPO_INSTALL_HOOKS_TOOL_ID,
];

const STORE_TOOL_IDS: &[&str] = &[
    agl_tools::STORE_STATUS_TOOL_ID,
    agl_tools::STORE_EXPORT_TOOL_ID,
    agl_tools::STORE_MIGRATE_TOOL_ID,
];

const SKILL_TOOL_IDS: &[&str] = &[
    agl_tools::SKILL_LIST_TOOL_ID,
    agl_tools::SKILL_INSPECT_TOOL_ID,
    agl_tools::SKILL_STATUS_TOOL_ID,
    agl_tools::SKILL_VERIFY_TOOL_ID,
    agl_tools::SKILL_LOCK_TOOL_ID,
    agl_tools::SKILL_TRUST_TOOL_ID,
    agl_tools::SKILL_REVOKE_TOOL_ID,
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_capabilities::{
        ActionInvocation, CapabilityPolicyInput, DeclarationDigest, DispatchDenialCode,
        ToolAccessMode,
    };
    use agl_ids::{ExecutionScope, RunId};
    use serde_json::json;

    use super::*;

    #[test]
    fn chat_tool_runtime_handlers_match_catalog_tools() {
        let root = temp_root("tool-parity");
        let core_tools = agl_tools::CoreTools::new(&root).unwrap();
        let catalog = chat_extension_catalog().unwrap();
        let runtime = chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools: &core_tools,
            store_root: &root.join("store"),
            trust_store_path: &root.join("skill-trust.toml"),
            workspace_root: &root,
            permission_status: agl_tools::PermissionRuntimeStatus::default(),
        })
        .unwrap();

        let catalog_tools = tool_ids(&catalog);
        let runtime_catalog_tools = tool_ids(runtime.catalog());
        let handler_tools = runtime.handler_ids().cloned().collect::<BTreeSet<_>>();
        let catalog_providers = provider_digests(&catalog);
        let runtime_providers = provider_digests(runtime.catalog());

        assert_eq!(runtime_catalog_tools, catalog_tools);
        assert_eq!(handler_tools, catalog_tools);
        assert_eq!(runtime_providers, catalog_providers);
        assert!(
            !catalog_tools
                .contains(&CapabilityId::new(agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap()),
            "Matrix delivery is bridge-owned and must stay out of chat runtime"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    fn provider_digests(
        catalog: &ToolCatalog,
    ) -> std::collections::BTreeMap<String, DeclarationDigest> {
        catalog
            .providers()
            .iter()
            .map(|provider| (provider.id.as_str().to_owned(), provider.digest()))
            .collect()
    }

    #[test]
    fn forged_hidden_capability_is_denied_before_its_handler_runs() {
        let root = temp_root("hidden-dispatch");
        let path = root.join("README.MD");
        std::fs::write(&path, "old\n").unwrap();
        let core_tools = agl_tools::CoreTools::new(&root).unwrap();
        let runtime = test_runtime(&root, &core_tools);
        let effective = CapabilityPolicyInput::new(
            runtime.catalog().providers().iter().cloned(),
            [CapabilityId::new(agl_tools::FS_READ_TOOL_ID).unwrap()],
            ToolAccessMode::Admin,
        )
        .resolve()
        .unwrap();
        let capability_id = CapabilityId::new(agl_tools::FS_EDIT_TOOL_ID).unwrap();
        let provider = runtime
            .catalog()
            .provider_for_action(&capability_id)
            .unwrap();
        let declaration = provider.action(&capability_id).unwrap();
        let invocation = ActionInvocation::new(
            ExecutionScope::builder(RunId::generate()).build().unwrap(),
            capability_id,
            provider.id.clone(),
            declaration.digest(),
            effective.policy_hash().clone(),
            json!({"path": "README.MD", "old_text": "old", "new_text": "new"}),
        );

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().unwrap().code,
            DispatchDenialCode::CapabilityNotEffective
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_declaration_snapshot_is_rejected_again_at_chat_dispatch() {
        let root = temp_root("stale-dispatch");
        std::fs::write(root.join("README.MD"), "content\n").unwrap();
        let core_tools = agl_tools::CoreTools::new(&root).unwrap();
        let runtime = test_runtime(&root, &core_tools);
        let capability_id = CapabilityId::new(agl_tools::FS_READ_TOOL_ID).unwrap();
        let effective = CapabilityPolicyInput::new(
            runtime.catalog().providers().iter().cloned(),
            [capability_id.clone()],
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap();
        let provider = runtime
            .catalog()
            .provider_for_action(&capability_id)
            .unwrap();
        let invocation = ActionInvocation::new(
            ExecutionScope::builder(RunId::generate()).build().unwrap(),
            capability_id,
            provider.id.clone(),
            DeclarationDigest::parse(&format!("sha256:{}", "0".repeat(64))).unwrap(),
            effective.policy_hash().clone(),
            json!({"path": "README.MD"}),
        );

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().unwrap().code,
            DispatchDenialCode::StaleDeclaration
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn test_runtime(root: &Path, core_tools: &agl_tools::CoreTools) -> ToolRuntime {
        chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools,
            store_root: &root.join("store"),
            trust_store_path: &root.join("skill-trust.toml"),
            workspace_root: root,
            permission_status: agl_tools::PermissionRuntimeStatus::default(),
        })
        .unwrap()
    }

    fn tool_ids(catalog: &ToolCatalog) -> BTreeSet<CapabilityId> {
        catalog
            .providers()
            .iter()
            .flat_map(|provider| provider.actions.iter().map(|action| action.id.clone()))
            .collect()
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("agl-chat-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
}
