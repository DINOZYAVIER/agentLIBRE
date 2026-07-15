use std::collections::{BTreeMap, BTreeSet};

use crate::{
    HookDeclaration, HookId, ToolDeclaration, ToolHandler, ToolId, ToolInput, ToolOutput,
    ToolProviderDeclaration, ToolProviderDeclarationError, ToolProviderId, ToolProviderTrust,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    providers: Vec<ToolProviderDeclaration>,
    provider_index: BTreeMap<ToolProviderId, usize>,
    hook_index: BTreeMap<HookId, usize>,
    tool_index: BTreeMap<ToolId, usize>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        declaration: ToolProviderDeclaration,
    ) -> Result<(), ToolCatalogError> {
        declaration
            .validate()
            .map_err(ToolCatalogError::InvalidDeclaration)?;
        let provider_index = self.providers.len();
        if self.provider_index.contains_key(&declaration.id) {
            return Err(ToolCatalogError::DuplicateProvider {
                id: declaration.id.as_str().to_string(),
            });
        }
        for hook in &declaration.hooks {
            if self.hook_index.contains_key(&hook.id) {
                return Err(ToolCatalogError::DuplicateHook {
                    id: hook.id.as_str().to_string(),
                });
            }
        }
        for tool in &declaration.tools {
            if self.tool_index.contains_key(&tool.id) {
                return Err(ToolCatalogError::DuplicateTool {
                    id: tool.id.as_str().to_string(),
                });
            }
        }
        self.provider_index
            .insert(declaration.id.clone(), provider_index);
        for hook in &declaration.hooks {
            self.hook_index.insert(hook.id.clone(), provider_index);
        }
        for tool in &declaration.tools {
            self.tool_index.insert(tool.id.clone(), provider_index);
        }
        self.providers.push(declaration);
        Ok(())
    }

    pub fn providers(&self) -> &[ToolProviderDeclaration] {
        &self.providers
    }

    pub fn provider(&self, id: &ToolProviderId) -> Option<&ToolProviderDeclaration> {
        let provider_index = *self.provider_index.get(id)?;
        self.providers.get(provider_index)
    }

    pub fn hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        let provider_index = *self.hook_index.get(id)?;
        self.providers[provider_index]
            .hooks
            .iter()
            .find(|hook| &hook.id == id)
    }

    pub fn tool(&self, id: &ToolId) -> Option<&ToolDeclaration> {
        let provider_index = *self.tool_index.get(id)?;
        self.providers[provider_index]
            .tools
            .iter()
            .find(|tool| &tool.id == id)
    }

    pub fn provider_for_tool(&self, id: &ToolId) -> Option<&ToolProviderDeclaration> {
        let provider_index = *self.tool_index.get(id)?;
        self.providers.get(provider_index)
    }

    pub fn executable_tool(&self, id: &ToolId) -> Result<&ToolDeclaration, ToolDispatchError> {
        let tool = self
            .tool(id)
            .ok_or_else(|| ToolDispatchError::UnknownTool {
                id: id.as_str().to_string(),
            })?;
        self.ensure_provider_trusted_for_tool(id)?;
        Ok(tool)
    }

    pub fn ensure_provider_trusted_for_tool(&self, id: &ToolId) -> Result<(), ToolDispatchError> {
        let provider =
            self.provider_for_tool(id)
                .ok_or_else(|| ToolDispatchError::UnknownTool {
                    id: id.as_str().to_string(),
                })?;
        if provider.permits_tool_execution() {
            Ok(())
        } else {
            Err(ToolDispatchError::UntrustedProvider {
                tool_id: id.as_str().to_string(),
                provider_id: provider.id.as_str().to_string(),
                trust: provider.trust,
            })
        }
    }

    pub fn has_hook(&self, id: &HookId) -> bool {
        self.hook_index.contains_key(id)
    }
}

pub struct ToolRuntime {
    catalog: ToolCatalog,
    handlers: BTreeMap<ToolId, Box<dyn ToolHandler>>,
    allowed_tools: Option<BTreeSet<ToolId>>,
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRuntime {
    pub fn new() -> Self {
        Self {
            catalog: ToolCatalog::new(),
            handlers: BTreeMap::new(),
            allowed_tools: None,
        }
    }

    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    pub fn register_provider(
        &mut self,
        declaration: ToolProviderDeclaration,
    ) -> Result<(), ToolCatalogError> {
        self.catalog.register(declaration)
    }

    pub fn register_handler(
        &mut self,
        tool_id: ToolId,
        handler: impl ToolHandler + 'static,
    ) -> Result<(), ToolCatalogError> {
        if self.handlers.contains_key(&tool_id) {
            return Err(ToolCatalogError::DuplicateHandler {
                id: tool_id.as_str().to_string(),
            });
        }
        self.handlers.insert(tool_id, Box::new(handler));
        Ok(())
    }

    pub fn handler_ids(&self) -> impl Iterator<Item = &ToolId> {
        self.handlers.keys()
    }

    pub fn set_allowed_tools(&mut self, allowed_tools: impl IntoIterator<Item = ToolId>) {
        self.allowed_tools = Some(allowed_tools.into_iter().collect());
    }

    pub fn clear_allowed_tools(&mut self) {
        self.allowed_tools = None;
    }

    pub fn dispatch(&self, input: ToolInput) -> Result<ToolOutput, ToolDispatchError> {
        self.catalog.executable_tool(&input.id)?;
        if let Some(allowed_tools) = &self.allowed_tools
            && !allowed_tools.contains(&input.id)
        {
            return Err(ToolDispatchError::ToolNotAllowed {
                id: input.id.as_str().to_string(),
            });
        }
        let handler =
            self.handlers
                .get(&input.id)
                .ok_or_else(|| ToolDispatchError::MissingHandler {
                    id: input.id.as_str().to_string(),
                })?;
        handler.dispatch(input).map_err(ToolDispatchError::Handler)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCatalogError {
    InvalidDeclaration(ToolProviderDeclarationError),
    DuplicateProvider { id: String },
    DuplicateHook { id: String },
    DuplicateTool { id: String },
    DuplicateHandler { id: String },
}

impl std::fmt::Display for ToolCatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDeclaration(err) => write!(f, "{err}"),
            Self::DuplicateProvider { id } => write!(f, "duplicate provider id `{id}`"),
            Self::DuplicateHook { id } => write!(f, "duplicate hook id `{id}`"),
            Self::DuplicateTool { id } => write!(f, "duplicate tool id `{id}`"),
            Self::DuplicateHandler { id } => write!(f, "duplicate tool handler `{id}`"),
        }
    }
}

impl std::error::Error for ToolCatalogError {}

#[derive(Debug)]
pub enum ToolDispatchError {
    UnknownTool {
        id: String,
    },
    MissingHandler {
        id: String,
    },
    UntrustedProvider {
        tool_id: String,
        provider_id: String,
        trust: ToolProviderTrust,
    },
    ToolNotAllowed {
        id: String,
    },
    Handler(anyhow::Error),
}

impl std::fmt::Display for ToolDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool { id } => write!(f, "unknown tool `{id}`"),
            Self::MissingHandler { id } => write!(f, "tool `{id}` has no registered handler"),
            Self::UntrustedProvider {
                tool_id,
                provider_id,
                trust,
            } => write!(
                f,
                "tool `{tool_id}` provider `{provider_id}` is not trusted for execution: {}",
                trust.block_reason()
            ),
            Self::ToolNotAllowed { id } => {
                write!(f, "tool `{id}` is not allowed in the current session")
            }
            Self::Handler(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for ToolDispatchError {}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use crate::{
        HookDeclaration, HookEvent, HookId, ToolCapability, ToolDeclaration, ToolHandler, ToolId,
        ToolInput, ToolOutput, ToolProviderDeclaration, ToolProviderDeclarationError,
        ToolProviderId, ToolProviderTrust, ToolStateEffect,
    };

    use super::*;

    #[test]
    fn catalog_registers_hooks_and_tools() {
        let hook_id = HookId::new("json.validate").unwrap();
        let tool_id = ToolId::new("file_read").unwrap();
        let declaration = ToolProviderDeclaration::new(
            ToolProviderId::new("core-tools").unwrap(),
            "Core Tools",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: hook_id.clone(),
            event: HookEvent::ModelResponse,
            required: true,
        })
        .with_tool(ToolDeclaration::new(
            tool_id.clone(),
            "Read a file",
            ToolCapability::Read,
            ["path"],
        ));
        let mut catalog = ToolCatalog::new();

        catalog.register(declaration).unwrap();

        assert!(catalog.has_hook(&hook_id));
        assert_eq!(
            catalog.hook(&hook_id).unwrap().event,
            HookEvent::ModelResponse
        );
        assert_eq!(catalog.tool(&tool_id).unwrap().description, "Read a file");
        assert_eq!(
            catalog.tool(&tool_id).unwrap().required_arguments,
            vec!["path"]
        );
    }

    #[test]
    fn catalog_rejects_duplicate_hooks_across_providers() {
        let first = ToolProviderDeclaration::new(
            ToolProviderId::new("core-guards").unwrap(),
            "Core Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("json.validate").unwrap(),
            event: HookEvent::ModelResponse,
            required: true,
        });
        let second = ToolProviderDeclaration::new(
            ToolProviderId::new("other-guards").unwrap(),
            "Other Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("json.validate").unwrap(),
            event: HookEvent::ArtifactWrite,
            required: true,
        });
        let mut catalog = ToolCatalog::new();
        catalog.register(first).unwrap();

        assert_eq!(
            catalog.register(second).unwrap_err(),
            ToolCatalogError::DuplicateHook {
                id: "json.validate".to_string(),
            }
        );
    }

    #[test]
    fn runtime_dispatches_registered_third_party_tool() {
        let tool_id = ToolId::new("example.echo").unwrap();
        let declaration = ToolProviderDeclaration::test_fixture(
            ToolProviderId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ToolProviderTrust::TrustedRegistered,
        )
        .unwrap()
        .with_tool(ToolDeclaration::new(
            tool_id.clone(),
            "Echo text",
            ToolCapability::Read,
            ["text"],
        ));
        let mut runtime = ToolRuntime::new();
        runtime.register_provider(declaration).unwrap();
        runtime
            .register_handler(tool_id.clone(), EchoHandler)
            .unwrap();

        let output = runtime
            .dispatch(ToolInput {
                id: tool_id,
                arguments: serde_json::json!({ "text": "hello" }),
            })
            .unwrap();

        assert_eq!(output.observation, "hello");
    }

    #[test]
    fn runtime_rejects_untrusted_provider_before_dispatch() {
        let tool_id = ToolId::new("example.echo").unwrap();
        let declaration = ToolProviderDeclaration::test_fixture(
            ToolProviderId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ToolProviderTrust::Unsupported,
        )
        .unwrap()
        .with_tool(ToolDeclaration::new(
            tool_id.clone(),
            "Echo text",
            ToolCapability::Read,
            ["text"],
        ));
        let mut runtime = ToolRuntime::new();
        runtime.register_provider(declaration).unwrap();
        runtime
            .register_handler(tool_id.clone(), EchoHandler)
            .unwrap();

        let err = runtime
            .dispatch(ToolInput {
                id: tool_id,
                arguments: serde_json::json!({ "text": "hello" }),
            })
            .unwrap_err();

        assert!(err.to_string().contains("not trusted"));
    }

    #[test]
    fn runtime_rejects_tool_outside_session_allowlist() {
        let tool_id = ToolId::new("example.echo").unwrap();
        let declaration = ToolProviderDeclaration::test_fixture(
            ToolProviderId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ToolProviderTrust::TrustedRegistered,
        )
        .unwrap()
        .with_tool(ToolDeclaration::new(
            tool_id.clone(),
            "Echo text",
            ToolCapability::Read,
            ["text"],
        ));
        let mut runtime = ToolRuntime::new();
        runtime.register_provider(declaration).unwrap();
        runtime
            .register_handler(tool_id.clone(), EchoHandler)
            .unwrap();
        runtime.set_allowed_tools([ToolId::new("example.other").unwrap()]);

        let err = runtime
            .dispatch(ToolInput {
                id: tool_id,
                arguments: serde_json::json!({ "text": "hello" }),
            })
            .unwrap_err();

        assert!(matches!(err, ToolDispatchError::ToolNotAllowed { .. }));
    }

    #[test]
    fn catalog_rejects_mutating_tool_without_state_effects() {
        let tool_id = ToolId::new("example.write").unwrap();
        let declaration = ToolProviderDeclaration::test_fixture(
            ToolProviderId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ToolProviderTrust::TrustedRegistered,
        )
        .unwrap()
        .with_tool(ToolDeclaration::new(
            tool_id,
            "Write state",
            ToolCapability::Write,
            ["value"],
        ));
        let mut catalog = ToolCatalog::new();

        assert_eq!(
            catalog.register(declaration).unwrap_err(),
            ToolCatalogError::InvalidDeclaration(
                ToolProviderDeclarationError::InvalidToolOperation {
                    id: "example.write".to_string(),
                    message: "state-mutating operations must declare state effects".to_string(),
                },
            )
        );
    }

    #[test]
    fn catalog_rejects_read_tool_with_state_effects() {
        let tool_id = ToolId::new("example.read").unwrap();
        let declaration = ToolProviderDeclaration::test_fixture(
            ToolProviderId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ToolProviderTrust::TrustedRegistered,
        )
        .unwrap()
        .with_tool(
            ToolDeclaration::new(tool_id, "Read state", ToolCapability::Read, ["value"])
                .with_state_effects([ToolStateEffect::RepoFiles]),
        );
        let mut catalog = ToolCatalog::new();

        assert_eq!(
            catalog.register(declaration).unwrap_err(),
            ToolCatalogError::InvalidDeclaration(
                ToolProviderDeclarationError::InvalidToolOperation {
                    id: "example.read".to_string(),
                    message: "read operations must not declare state effects".to_string(),
                },
            )
        );
    }

    struct EchoHandler;

    impl ToolHandler for EchoHandler {
        fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
            let text = input
                .arguments
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(ToolOutput { observation: text })
        }
    }
}
