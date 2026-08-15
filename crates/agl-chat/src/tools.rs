use std::fmt::{self, Display, Formatter};
use std::path::Path;
use std::sync::Arc;

use agl_extension::package::ExtensionPackageBuilder;
use agl_extension::{
    Extension, ExtensionBindings, ExtensionDefinition, ExtensionHost, ExtensionHostBuilder,
    StaticExtensionFactory,
};
use agl_ids::RunId;
use agl_kernel::{HostBindingId, HostBindingRequirement, ToolBinding, ToolHandler, ToolId};
use agl_kernel::{ToolCatalog, ToolRuntime};
use agl_runtime::{ExtensionCompositionInput, StaticExtensionRegistry, compose_extension_catalog};
use anyhow::Result;

pub(crate) struct ChatToolRuntimeConfig<'a> {
    pub core_tools: &'a agl_core_tools::CoreTools,
    pub repositories: &'a agl_runtime::StoreRepositories,
    pub trust_store_path: &'a Path,
    pub workspace_root: &'a Path,
    pub runtime_paths: &'a agl_runtime::AgentLibrePaths,
    pub permission_status: agl_core_tools::PermissionRuntimeStatus,
    pub process_tools: Option<agl_core_tools::ProcessTools>,
    pub screen_admitted_run: Option<RunId>,
    pub delegation_handler: Option<crate::delegation::DelegationHandler>,
}

pub(crate) fn chat_extension_catalog() -> Result<ToolCatalog> {
    let shared: Arc<dyn ToolHandler> = Arc::new(UnavailableToolHandler);
    let host = product_host_builder()
        .binding(
            host_binding_id(agl_core_tools::guards::EXTENSION_ID),
            1,
            agl_core_tools::guards::CoreGuards::new(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::cron::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::fs::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::matrix::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::memory::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::notes::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::permissions::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::process::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::repo::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::skills::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::store::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(agl_host_tools::screen::EXTENSION_ID),
            1,
            shared.clone(),
        )
        .shared_tool_handler(
            host_binding_id(crate::delegation_contract::AGENT_DELEGATE_EXTENSION_ID),
            1,
            shared,
        )
        .build();
    let composed = compose_chat_product(host)?;
    Ok(composed.runtime().catalog().clone())
}

pub(crate) fn chat_tool_runtime(config: ChatToolRuntimeConfig<'_>) -> Result<ToolRuntime> {
    let mut core_tools = config.core_tools.clone();
    let package_input = agl_repo::package_composition_input(config.workspace_root)?;
    let mut repo_tools = agl_core_tools::RepoTools::new(
        config.workspace_root,
        config.repositories.artifact_commits.clone(),
    );
    let mut host = product_host_builder()
        .binding(
            host_binding_id(agl_core_tools::guards::EXTENSION_ID),
            1,
            agl_core_tools::guards::CoreGuards::new(),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::fs::EXTENSION_ID),
            1,
            Arc::new(core_tools.clone()),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::cron::EXTENSION_ID),
            1,
            Arc::new(agl_core_tools::CronTools::new(
                config.repositories.cron.clone(),
                config.repositories.matrix_outbox.clone(),
            )),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::matrix::EXTENSION_ID),
            1,
            Arc::new(agl_core_tools::MatrixTools::new(
                config.repositories.matrix_outbox.clone(),
            )),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::memory::EXTENSION_ID),
            1,
            Arc::new(agl_core_tools::MemoryTools::new(
                config.repositories.memory.clone(),
            )),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::notes::EXTENSION_ID),
            1,
            Arc::new(agl_core_tools::NotesTools::new(
                config.repositories.notes.clone(),
            )),
        );
    let repo_descriptor = agl_core_tools::repo::declaration();
    if let Some(declaration) = repo_descriptor
        .artifacts
        .iter()
        .find(|artifact| artifact.id.as_str() == "core.repo:tasks")
        && let Ok(repository) = agl_repo::ArtifactGitRepository::open(config.workspace_root)
    {
        repository.recover_incomplete(config.repositories.artifact_commits.as_ref())?;
        if let Ok(binding) = repository.verify_binding(declaration) {
            let handle = agl_artifact::ArtifactHandle::bind(declaration.clone(), binding.clone())?;
            core_tools =
                core_tools.with_artifact_route(binding.submodule_path(), handle.clone())?;
            repo_tools = repo_tools.with_artifact(binding, handle.clone())?;
            host = host.artifact(handle).shared_tool_handler(
                host_binding_id(agl_core_tools::fs::EXTENSION_ID),
                1,
                Arc::new(core_tools.clone()),
            );
        }
    }
    let permission_tools =
        agl_core_tools::PermissionTools::new(config.repositories.permissions.clone())
            .with_runtime_status(config.permission_status);
    let permission_tools = config
        .process_tools
        .as_ref()
        .map(|process| {
            permission_tools
                .clone()
                .with_terminal_endpoint(process.terminal_endpoint())
        })
        .unwrap_or(permission_tools);
    host = host
        .shared_tool_handler(
            host_binding_id(agl_core_tools::permissions::EXTENSION_ID),
            1,
            Arc::new(permission_tools),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::process::EXTENSION_ID),
            1,
            config.process_tools.map_or_else(
                || Arc::new(UnavailableToolHandler) as Arc<dyn ToolHandler>,
                |tools| Arc::new(tools) as Arc<dyn ToolHandler>,
            ),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::repo::EXTENSION_ID),
            1,
            Arc::new(repo_tools),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::store::EXTENSION_ID),
            1,
            Arc::new(agl_core_tools::StoreTools::new(
                config.repositories.administration.clone(),
            )),
        )
        .shared_tool_handler(
            host_binding_id(agl_core_tools::skills::EXTENSION_ID),
            1,
            Arc::new(agl_host_tools::SkillTools::new(
                package_input,
                config.trust_store_path,
                config.runtime_paths.clone(),
            )),
        )
        .shared_tool_handler(
            host_binding_id(agl_host_tools::screen::EXTENSION_ID),
            1,
            Arc::new(agl_host_tools::ScreenTools::new(
                config.repositories.content.clone(),
                config.screen_admitted_run,
            )),
        )
        .shared_tool_handler(
            host_binding_id(crate::delegation_contract::AGENT_DELEGATE_EXTENSION_ID),
            1,
            Arc::new(
                config
                    .delegation_handler
                    .unwrap_or_else(crate::delegation::DelegationHandler::disabled),
            ),
        );

    Ok(compose_chat_product(host.build())?.into_runtime())
}

fn compose_chat_product(host: ExtensionHost) -> Result<agl_runtime::RuntimeExtensionCatalog> {
    let mut registry = StaticExtensionRegistry::new();
    let mut input = ExtensionCompositionInput::builder();
    for factory in chat_product_factories() {
        let definition = factory.definition();
        let selected = definition
            .descriptor()
            .artifacts
            .iter()
            .all(|artifact| host.artifact(&artifact.id).is_some());
        input = input
            .package(ExtensionPackageBuilder::build_to_memory(
                definition.clone(),
            )?)
            .selected(definition.id.clone(), selected);
        registry.register(factory)?;
    }
    Ok(compose_extension_catalog(
        input.host(host).registry(registry).build()?,
    )?)
}

pub(crate) fn chat_product_factories() -> Vec<StaticExtensionFactory> {
    vec![
        agl_core_tools::guards_extension_factory(),
        agl_core_tools::cron_extension_factory(),
        agl_core_tools::fs_extension_factory(),
        agl_core_tools::matrix_extension_factory(),
        agl_core_tools::memory_extension_factory(),
        agl_core_tools::notes_extension_factory(),
        agl_core_tools::permissions_extension_factory(),
        agl_core_tools::process_extension_factory(),
        agl_core_tools::repo_extension_factory(),
        agl_core_tools::skills_extension_factory(),
        agl_core_tools::store_extension_factory(),
        agl_host_tools::screen_extension_factory(),
        StaticExtensionFactory::for_extension::<DelegationExtension>(),
    ]
}

fn product_host_builder() -> ExtensionHostBuilder {
    ExtensionHost::builder()
}

fn host_binding_id(extension_id: &str) -> HostBindingId {
    HostBindingId::new(extension_id).expect("Extension ID is a valid host binding ID")
}

#[derive(Clone, Copy)]
struct UnavailableToolHandler;

impl ToolHandler for UnavailableToolHandler {
    fn dispatch(
        &self,
        _context: agl_kernel::ToolDispatchContext,
    ) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(std::future::ready(Err(anyhow::anyhow!(
            "process runtime is unavailable"
        )
        .into())))
    }
}

struct DelegationExtension;

#[derive(Debug)]
struct DelegationBindError;

impl Display for DelegationBindError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("delegation host binding is absent or has the wrong type")
    }
}

impl std::error::Error for DelegationBindError {}

impl Extension for DelegationExtension {
    type BindError = DelegationBindError;

    fn definition() -> ExtensionDefinition {
        ExtensionDefinition::from_descriptor(
            1,
            crate::delegation_contract::delegation_extension().with_host_binding(
                HostBindingRequirement::new(
                    host_binding_id(crate::delegation_contract::AGENT_DELEGATE_EXTENSION_ID),
                    1,
                ),
            ),
        )
        .expect("delegation Extension definition is valid")
    }

    fn bind(host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        let handler = host
            .shared_tool_handler(&host_binding_id(
                crate::delegation_contract::AGENT_DELEGATE_EXTENSION_ID,
            ))
            .ok_or(DelegationBindError)?;
        Ok(ExtensionBindings::new(
            [ToolBinding::from_shared(
                ToolId::new(crate::delegation_contract::AGENT_DELEGATE_TOOL_ID)
                    .expect("delegation Tool ID is valid"),
                handler.clone(),
            )],
            [],
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_ids::{ExecutionScope, RunId};
    use agl_kernel::{DeclarationDigest, ToolInvocation};
    use agl_kernel::{DispatchDenialCode, ToolAccessMode, ToolPolicyInput};
    use serde_json::json;

    use super::*;

    #[test]
    fn chat_tool_runtime_handlers_match_catalog_tools() {
        let root = temp_root("tool-parity");
        let core_tools = agl_core_tools::CoreTools::new(&root).unwrap();
        let catalog = chat_extension_catalog().unwrap();
        let runtime_paths = agl_runtime::AgentLibrePaths::from_agl_home(&root);
        let store_runtime = agl_runtime::StoreRuntime::open(&runtime_paths).unwrap();
        let runtime = chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools: &core_tools,
            repositories: store_runtime.repositories(),
            trust_store_path: &root.join("skill-trust.toml"),
            workspace_root: &root,
            runtime_paths: &runtime_paths,
            permission_status: agl_core_tools::PermissionRuntimeStatus::default(),
            process_tools: None,
            screen_admitted_run: None,
            delegation_handler: None,
        })
        .unwrap();

        let catalog_tools = tool_ids(&catalog);
        let runtime_catalog_tools = tool_ids(runtime.catalog());
        let handler_tools = runtime.handler_ids().cloned().collect::<BTreeSet<_>>();
        let catalog_extensions = extension_digests(&catalog);
        let runtime_extensions = extension_digests(runtime.catalog());

        assert_eq!(runtime_catalog_tools, catalog_tools);
        assert_eq!(handler_tools, catalog_tools);
        assert_eq!(runtime_extensions, catalog_extensions);
        assert!(
            !catalog_tools
                .contains(&ToolId::new(agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap()),
            "Matrix delivery is bridge-owned and must stay out of chat runtime"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    fn extension_digests(
        catalog: &ToolCatalog,
    ) -> std::collections::BTreeMap<String, DeclarationDigest> {
        catalog
            .extensions()
            .iter()
            .map(|extension| (extension.id.as_str().to_owned(), extension.digest()))
            .collect()
    }

    #[test]
    fn forged_hidden_tool_is_denied_before_its_handler_runs() {
        let root = temp_root("hidden-dispatch");
        let path = root.join("README.MD");
        std::fs::write(&path, "old\n").unwrap();
        let core_tools = agl_core_tools::CoreTools::new(&root).unwrap();
        let runtime = test_runtime(&root, &core_tools);
        let effective = ToolPolicyInput::new(
            runtime.catalog().extensions().iter().cloned(),
            [ToolId::new(agl_core_tools::FS_READ_TOOL_ID).unwrap()],
            ToolAccessMode::Admin,
        )
        .resolve()
        .unwrap();
        let tool_id = ToolId::new(agl_core_tools::FS_APPLY_PATCH_TOOL_ID).unwrap();
        let extension = runtime.catalog().extension_for_tool(&tool_id).unwrap();
        let declaration = extension.tool(&tool_id).unwrap();
        let invocation = ToolInvocation::new(
            ExecutionScope::builder(RunId::generate()).build().unwrap(),
            tool_id,
            extension.id.clone(),
            declaration.digest(),
            effective.policy_hash().clone(),
            json!({"path": "README.MD", "old_text": "old", "new_text": "new"}),
        );

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                agl_kernel::ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();

        assert_eq!(
            error.denial().unwrap().code,
            DispatchDenialCode::ToolNotEffective
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_declaration_snapshot_is_rejected_again_at_chat_dispatch() {
        let root = temp_root("stale-dispatch");
        std::fs::write(root.join("README.MD"), "content\n").unwrap();
        let core_tools = agl_core_tools::CoreTools::new(&root).unwrap();
        let runtime = test_runtime(&root, &core_tools);
        let tool_id = ToolId::new(agl_core_tools::FS_READ_TOOL_ID).unwrap();
        let effective = ToolPolicyInput::new(
            runtime.catalog().extensions().iter().cloned(),
            [tool_id.clone()],
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap();
        let extension = runtime.catalog().extension_for_tool(&tool_id).unwrap();
        let invocation = ToolInvocation::new(
            ExecutionScope::builder(RunId::generate()).build().unwrap(),
            tool_id,
            extension.id.clone(),
            DeclarationDigest::parse(&format!("sha256:{}", "0".repeat(64))).unwrap(),
            effective.policy_hash().clone(),
            json!({"path": "README.MD"}),
        );

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                agl_kernel::ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();

        assert_eq!(
            error.denial().unwrap().code,
            DispatchDenialCode::StaleDeclaration
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn test_runtime(root: &Path, core_tools: &agl_core_tools::CoreTools) -> ToolRuntime {
        let runtime_paths = agl_runtime::AgentLibrePaths::from_agl_home(root);
        let store_runtime = agl_runtime::StoreRuntime::open(&runtime_paths).unwrap();
        chat_tool_runtime(ChatToolRuntimeConfig {
            core_tools,
            repositories: store_runtime.repositories(),
            trust_store_path: &root.join("skill-trust.toml"),
            workspace_root: root,
            runtime_paths: &runtime_paths,
            permission_status: agl_core_tools::PermissionRuntimeStatus::default(),
            process_tools: None,
            screen_admitted_run: None,
            delegation_handler: None,
        })
        .unwrap()
    }

    fn tool_ids(catalog: &ToolCatalog) -> BTreeSet<ToolId> {
        catalog
            .extensions()
            .iter()
            .flat_map(|extension| extension.tools.iter().map(|action| action.id.clone()))
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
