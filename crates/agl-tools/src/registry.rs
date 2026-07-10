use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, DeclarationError, DispatchDenial, EffectiveCapabilitySet, HookDeclaration,
    HookId, ProviderDeclaration, ProviderId, ProviderTrust,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    providers: Vec<ProviderDeclaration>,
    provider_index: BTreeMap<ProviderId, usize>,
    hook_index: BTreeMap<HookId, usize>,
    capability_index: BTreeMap<CapabilityId, usize>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, declaration: ProviderDeclaration) -> Result<(), ToolCatalogError> {
        declaration
            .validate()
            .map_err(ToolCatalogError::InvalidDeclaration)?;
        let provider_index = self.providers.len();
        if self.provider_index.contains_key(&declaration.id) {
            return Err(ToolCatalogError::DuplicateProvider {
                id: declaration.id.clone(),
            });
        }
        for hook in &declaration.hooks {
            if self.hook_index.contains_key(&hook.id) {
                return Err(ToolCatalogError::DuplicateHook {
                    id: hook.id.clone(),
                });
            }
        }
        for action in &declaration.actions {
            if self.capability_index.contains_key(&action.id) {
                return Err(ToolCatalogError::DuplicateCapability {
                    id: action.id.clone(),
                });
            }
        }

        self.provider_index
            .insert(declaration.id.clone(), provider_index);
        for hook in &declaration.hooks {
            self.hook_index.insert(hook.id.clone(), provider_index);
        }
        for action in &declaration.actions {
            self.capability_index
                .insert(action.id.clone(), provider_index);
        }
        self.providers.push(declaration);
        Ok(())
    }

    pub fn providers(&self) -> &[ProviderDeclaration] {
        &self.providers
    }

    pub fn provider(&self, id: &ProviderId) -> Option<&ProviderDeclaration> {
        self.providers.get(*self.provider_index.get(id)?)
    }

    pub fn hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        let provider = self.providers.get(*self.hook_index.get(id)?)?;
        provider.hooks.iter().find(|hook| &hook.id == id)
    }

    pub fn provider_for_hook(&self, id: &HookId) -> Option<&ProviderDeclaration> {
        self.providers.get(*self.hook_index.get(id)?)
    }

    pub fn trusted_hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        self.provider_for_hook(id)?
            .permits_execution()
            .then(|| self.hook(id))
            .flatten()
    }

    pub fn action(&self, id: &CapabilityId) -> Option<&ActionDeclaration> {
        let provider = self.providers.get(*self.capability_index.get(id)?)?;
        provider.action(id)
    }

    pub fn provider_for_action(&self, id: &CapabilityId) -> Option<&ProviderDeclaration> {
        self.providers.get(*self.capability_index.get(id)?)
    }

    pub fn executable_action(
        &self,
        id: &CapabilityId,
    ) -> Result<&ActionDeclaration, ToolDispatchError> {
        let action = self
            .action(id)
            .ok_or_else(|| ToolDispatchError::UnknownCapability { id: id.clone() })?;
        let provider = self
            .provider_for_action(id)
            .expect("capability index must reference its provider");
        if provider.permits_execution() {
            Ok(action)
        } else {
            Err(ToolDispatchError::UntrustedProvider {
                capability_id: id.clone(),
                provider_id: provider.id.clone(),
                trust: provider.trust,
            })
        }
    }

    pub fn has_hook(&self, id: &HookId) -> bool {
        self.hook_index.contains_key(id)
    }

    pub fn capability_ids(&self) -> impl ExactSizeIterator<Item = &CapabilityId> {
        self.capability_index.keys()
    }
}

pub struct ToolRuntime {
    catalog: ToolCatalog,
    handlers: BTreeMap<CapabilityId, Box<dyn ActionHandler>>,
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
        }
    }

    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    pub fn register_provider(
        &mut self,
        declaration: ProviderDeclaration,
    ) -> Result<(), ToolCatalogError> {
        self.catalog.register(declaration)
    }

    pub fn register_handler(
        &mut self,
        capability_id: CapabilityId,
        handler: impl ActionHandler + 'static,
    ) -> Result<(), ToolCatalogError> {
        if self.handlers.contains_key(&capability_id) {
            return Err(ToolCatalogError::DuplicateHandler { id: capability_id });
        }
        self.handlers.insert(capability_id, Box::new(handler));
        Ok(())
    }

    pub fn handler_ids(&self) -> impl ExactSizeIterator<Item = &CapabilityId> {
        self.handlers.keys()
    }

    pub fn dispatch(
        &self,
        invocation: ActionInvocation,
        effective: &EffectiveCapabilitySet,
    ) -> Result<ActionResult, ToolDispatchError> {
        effective
            .authorize(&invocation, self.catalog.providers())
            .map_err(ToolDispatchError::Denied)?;
        let handler = self
            .handlers
            .get(&invocation.capability_id)
            .ok_or_else(|| ToolDispatchError::MissingHandler {
                id: invocation.capability_id.clone(),
            })?;
        handler
            .dispatch(invocation)
            .map_err(ToolDispatchError::Handler)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCatalogError {
    InvalidDeclaration(DeclarationError),
    DuplicateProvider { id: ProviderId },
    DuplicateHook { id: HookId },
    DuplicateCapability { id: CapabilityId },
    DuplicateHandler { id: CapabilityId },
}

impl Display for ToolCatalogError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration(error) => Display::fmt(error, formatter),
            Self::DuplicateProvider { id } => write!(formatter, "duplicate provider ID `{id}`"),
            Self::DuplicateHook { id } => write!(formatter, "duplicate hook ID `{id}`"),
            Self::DuplicateCapability { id } => {
                write!(formatter, "duplicate capability ID `{id}`")
            }
            Self::DuplicateHandler { id } => {
                write!(formatter, "duplicate action handler for `{id}`")
            }
        }
    }
}

impl std::error::Error for ToolCatalogError {}

#[derive(Debug)]
pub enum ToolDispatchError {
    UnknownCapability {
        id: CapabilityId,
    },
    MissingHandler {
        id: CapabilityId,
    },
    UntrustedProvider {
        capability_id: CapabilityId,
        provider_id: ProviderId,
        trust: ProviderTrust,
    },
    Denied(DispatchDenial),
    Handler(ActionHandlerError),
}

impl ToolDispatchError {
    pub fn denial(&self) -> Option<&DispatchDenial> {
        match self {
            Self::Denied(denial) => Some(denial),
            _ => None,
        }
    }
}

impl Display for ToolDispatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCapability { id } => write!(formatter, "unknown capability `{id}`"),
            Self::MissingHandler { id } => write!(formatter, "capability `{id}` has no handler"),
            Self::UntrustedProvider {
                capability_id,
                provider_id,
                trust,
            } => write!(
                formatter,
                "capability `{capability_id}` provider `{provider_id}` is not trusted: {}",
                trust.as_str()
            ),
            Self::Denied(denial) => Display::fmt(denial, formatter),
            Self::Handler(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ToolDispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Denied(error) => Some(error),
            Self::Handler(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

pub fn verify_handler_coverage(runtime: &ToolRuntime) -> Result<(), HandlerCoverageError> {
    let declared = runtime
        .catalog
        .capability_ids()
        .cloned()
        .collect::<BTreeSet<_>>();
    let registered = runtime.handler_ids().cloned().collect::<BTreeSet<_>>();
    if declared == registered {
        Ok(())
    } else {
        Err(HandlerCoverageError {
            missing: declared.difference(&registered).cloned().collect(),
            undeclared: registered.difference(&declared).cloned().collect(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandlerCoverageError {
    pub missing: BTreeSet<CapabilityId>,
    pub undeclared: BTreeSet<CapabilityId>,
}

impl Display for HandlerCoverageError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "handler coverage mismatch: {} missing, {} undeclared",
            self.missing.len(),
            self.undeclared.len()
        )
    }
}

impl std::error::Error for HandlerCoverageError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agl_capabilities::{
        ActionDeclaration, CapabilityPolicyInput, DispatchDenialCode, OperationKind,
        ProviderSource, ToolAccessMode,
    };
    use agl_ids::{ExecutionScope, RunId};
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct CountingHandler(Arc<AtomicUsize>);

    impl ActionHandler for CountingHandler {
        fn dispatch(
            &self,
            invocation: ActionInvocation,
        ) -> Result<ActionResult, ActionHandlerError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ActionResult::new(json!({
                "echo": invocation.arguments["value"]
            })))
        }
    }

    #[test]
    fn invalid_arguments_are_denied_before_handler_execution() {
        let provider = provider("Echo");
        let effective = policy(&provider, true);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(provider.clone(), count.clone());
        let invocation = invocation(&provider, &effective, json!({"value": 7, "extra": true}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::InvalidArguments)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn hidden_capability_is_denied_before_handler_execution() {
        let provider = provider("Echo");
        let effective = policy(&provider, false);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(provider.clone(), count.clone());
        let invocation = invocation(&provider, &effective, json!({"value": "hello"}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::CapabilityNotEffective)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changed_provider_trust_invalidates_snapshot_before_handler_execution() {
        let trusted = provider("Echo");
        let effective = policy(&trusted, true);
        let changed = trusted.clone().with_trust(ProviderTrust::Changed);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(changed, count.clone());
        let invocation = invocation(&trusted, &effective, json!({"value": "hello"}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ProviderUntrusted)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn executable_trust_change_also_invalidates_snapshot() {
        let trusted = provider("Echo");
        let effective = policy(&trusted, true);
        let changed = trusted.clone().with_trust(ProviderTrust::TrustedByBinary);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(changed, count.clone());
        let invocation = invocation(&trusted, &effective, json!({"value": "hello"}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ProviderTrustChanged)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changed_declaration_invalidates_snapshot_before_handler_execution() {
        let original = provider("Echo");
        let effective = policy(&original, true);
        let current = provider("Changed description");
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(current, count.clone());
        let invocation = invocation(&original, &effective, json!({"value": "hello"}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::StaleDeclaration)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changed_provider_declaration_invalidates_snapshot() {
        let original = provider("Echo");
        let effective = policy(&original, true);
        let mut current = original.clone();
        current.version = "2".to_owned();
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(current, count.clone());
        let invocation = invocation(&original, &effective, json!({"value": "hello"}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ProviderChanged)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unrelated_catalog_change_invalidates_snapshot() {
        let primary = provider("Echo");
        let secondary = ProviderDeclaration::new(
            ProviderId::new("secondary-provider").unwrap(),
            "Secondary Provider",
            "1",
            ProviderSource::TestFixture,
            ProviderTrust::TrustedRegistered,
        )
        .unwrap();
        let effective = CapabilityPolicyInput::new(
            [primary.clone(), secondary.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap();
        let mut changed_secondary = secondary;
        changed_secondary.version = "2".to_owned();
        let count = Arc::new(AtomicUsize::new(0));
        let mut runtime = runtime(primary.clone(), count.clone());
        runtime.register_provider(changed_secondary).unwrap();
        let invocation = invocation(&primary, &effective, json!({"value": "hello"}));

        let error = runtime.dispatch(invocation, &effective).unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::CatalogChanged)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    fn capability_id() -> CapabilityId {
        CapabilityId::new("example.echo").unwrap()
    }

    fn provider(description: &str) -> ProviderDeclaration {
        ProviderDeclaration::new(
            ProviderId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ProviderSource::TestFixture,
            ProviderTrust::TrustedRegistered,
        )
        .unwrap()
        .with_action(
            ActionDeclaration::new(
                capability_id(),
                description,
                json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"value": {}},
                    "required": ["value"],
                    "additionalProperties": false
                }),
                OperationKind::Read,
            )
            .unwrap(),
        )
    }

    fn policy(provider: &ProviderDeclaration, routed: bool) -> EffectiveCapabilitySet {
        CapabilityPolicyInput::new(
            [provider.clone()],
            routed.then(capability_id),
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap()
    }

    fn runtime(provider: ProviderDeclaration, count: Arc<AtomicUsize>) -> ToolRuntime {
        let mut runtime = ToolRuntime::new();
        runtime.register_provider(provider).unwrap();
        runtime
            .register_handler(capability_id(), CountingHandler(count))
            .unwrap();
        runtime
    }

    fn invocation(
        provider: &ProviderDeclaration,
        effective: &EffectiveCapabilitySet,
        arguments: serde_json::Value,
    ) -> ActionInvocation {
        ActionInvocation::new(
            ExecutionScope::builder(RunId::generate()).build().unwrap(),
            capability_id(),
            provider.id.clone(),
            provider.action(&capability_id()).unwrap().digest(),
            effective.policy_hash().clone(),
            arguments,
        )
    }
}
