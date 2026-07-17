use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use agl_capabilities::{
    ActionDeclaration, ActionDispatchContext, ActionDispatchControl, ActionHandler,
    ActionHandlerError, ActionInvocation, ActionResult, CapabilityId, DeclarationError,
    DispatchDenial, EffectiveCapabilitySet, HookDeclaration, HookId, ProviderDeclaration,
    ProviderId, ProviderTrust,
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
        control: ActionDispatchControl,
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
        let effective_capability = effective
            .capability(&invocation.capability_id)
            .expect("authorized invocation must have an effective capability")
            .clone();
        let requested_effects = handler
            .preflight(&invocation)
            .map_err(ToolDispatchError::Handler)?;
        if !requested_effects
            .is_subset(&effective_capability.declaration().conditional_state_effects)
        {
            return Err(ToolDispatchError::Denied(DispatchDenial {
                capability_id: invocation.capability_id.clone(),
                code: agl_capabilities::DispatchDenialCode::ConditionalEffectUndeclared,
            }));
        }
        if !requested_effects.is_subset(effective_capability.authorized_state_effects()) {
            return Err(ToolDispatchError::Denied(DispatchDenial {
                capability_id: invocation.capability_id.clone(),
                code: agl_capabilities::DispatchDenialCode::ConditionalEffectDenied,
            }));
        }
        handler
            .dispatch(ActionDispatchContext::new(
                invocation,
                effective_capability,
                control,
                requested_effects,
            ))
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
        ActionDeclaration, CapabilityGrant, CapabilityPolicyInput, DispatchDenialCode, HookEvent,
        OperationKind, ProviderSource, SensitiveInput, StateEffect, ToolAccessMode,
    };
    use agl_ids::{ExecutionScope, RunId};
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct CountingHandler(Arc<AtomicUsize>);

    impl ActionHandler for CountingHandler {
        fn dispatch(
            &self,
            context: ActionDispatchContext,
        ) -> Result<ActionResult, ActionHandlerError> {
            let invocation = context.into_invocation();
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ActionResult::new(json!({
                "echo": invocation.arguments["value"]
            })))
        }
    }

    #[derive(Clone)]
    struct ConditionalHandler {
        requested: StateEffect,
        dispatch_count: Arc<AtomicUsize>,
    }

    impl ActionHandler for ConditionalHandler {
        fn preflight(
            &self,
            _invocation: &ActionInvocation,
        ) -> Result<BTreeSet<StateEffect>, ActionHandlerError> {
            Ok([self.requested].into_iter().collect())
        }

        fn dispatch(
            &self,
            context: ActionDispatchContext,
        ) -> Result<ActionResult, ActionHandlerError> {
            assert!(
                context
                    .authorized_conditional_effects()
                    .contains(&self.requested)
            );
            self.dispatch_count.fetch_add(1, Ordering::SeqCst);
            Ok(ActionResult::new(json!({"ok": true})))
        }
    }

    #[test]
    fn provider_qualified_hooks_keep_local_names_isolated_from_model_tools() {
        let mut catalog = ToolCatalog::new();
        for provider in ["alpha", "beta"] {
            catalog
                .register(
                    ProviderDeclaration::new(
                        ProviderId::new(provider).unwrap(),
                        format!("{provider} provider"),
                        "1",
                        ProviderSource::TestFixture,
                        ProviderTrust::TrustedRegistered,
                    )
                    .unwrap()
                    .with_hook(HookDeclaration {
                        id: HookId::new(format!("{provider}:validate")).unwrap(),
                        event: HookEvent::ArtifactWrite,
                        required: true,
                    }),
                )
                .unwrap();
        }

        for provider in ["alpha", "beta"] {
            let hook_id = HookId::new(format!("{provider}:validate")).unwrap();
            assert_eq!(
                catalog.provider_for_hook(&hook_id).unwrap().id.as_str(),
                provider
            );
            assert!(catalog.trusted_hook(&hook_id).is_some());
        }
        assert_eq!(catalog.capability_ids().count(), 0);
    }

    #[test]
    fn catalog_rejects_invalid_hook_ownership_and_reserved_core_claims() {
        let duplicate = ProviderDeclaration {
            id: ProviderId::new("duplicate").unwrap(),
            name: "Duplicate".to_string(),
            version: "1".to_string(),
            source: ProviderSource::TestFixture,
            trust: ProviderTrust::TrustedRegistered,
            hooks: vec![
                HookDeclaration {
                    id: HookId::new("duplicate:validate").unwrap(),
                    event: HookEvent::ArtifactWrite,
                    required: true,
                },
                HookDeclaration {
                    id: HookId::new("duplicate:validate").unwrap(),
                    event: HookEvent::ModelResponse,
                    required: false,
                },
            ],
            actions: Vec::new(),
        };
        assert!(matches!(
            ToolCatalog::new().register(duplicate),
            Err(ToolCatalogError::InvalidDeclaration(
                DeclarationError::DuplicateId { kind: "hook", .. }
            ))
        ));

        let mismatched = ProviderDeclaration {
            id: ProviderId::new("alpha").unwrap(),
            name: "Alpha".to_string(),
            version: "1".to_string(),
            source: ProviderSource::TestFixture,
            trust: ProviderTrust::TrustedRegistered,
            hooks: vec![HookDeclaration {
                id: HookId::new("beta:validate").unwrap(),
                event: HookEvent::ArtifactWrite,
                required: true,
            }],
            actions: Vec::new(),
        };
        assert!(matches!(
            ToolCatalog::new().register(mismatched),
            Err(ToolCatalogError::InvalidDeclaration(
                DeclarationError::HookProviderMismatch { .. }
            ))
        ));

        let core_shadow = ProviderDeclaration {
            id: ProviderId::new("core").unwrap(),
            name: "Core shadow".to_string(),
            version: "1".to_string(),
            source: ProviderSource::ThirdPartyRegistered,
            trust: ProviderTrust::TrustedRegistered,
            hooks: vec![HookDeclaration {
                id: HookId::new("core:validate").unwrap(),
                event: HookEvent::ArtifactWrite,
                required: true,
            }],
            actions: Vec::new(),
        };
        assert!(matches!(
            ToolCatalog::new().register(core_shadow),
            Err(ToolCatalogError::InvalidDeclaration(
                DeclarationError::ReservedProviderNamespace { .. }
            ))
        ));
    }

    #[test]
    fn invalid_arguments_are_denied_before_handler_execution() {
        let provider = provider("Echo");
        let effective = policy(&provider, true);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(provider.clone(), count.clone());
        let invocation = invocation(&provider, &effective, json!({"value": 7, "extra": true}));

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

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

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

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

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

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

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

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

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::StaleDeclaration)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changed_operation_or_effect_invalidates_snapshot_before_handler_execution() {
        let original = provider("Echo");
        let effective = policy(&original, true);
        let operation_changed = {
            let mut provider = original.clone();
            provider.actions[0] = ActionDeclaration::new(
                capability_id(),
                "Echo",
                original.actions[0].input_schema.clone(),
                OperationKind::Request,
            )
            .unwrap();
            provider
        };
        let effect_changed = {
            let mut provider = original.clone();
            provider.actions[0] = provider.actions[0]
                .clone()
                .with_state_effects([StateEffect::HostScreenCapture])
                .with_sensitive_inputs([SensitiveInput::ScreenCapture]);
            provider
        };

        for current in [operation_changed, effect_changed] {
            let count = Arc::new(AtomicUsize::new(0));
            let runtime = runtime(current, count.clone());
            let invocation = invocation(&original, &effective, json!({"value": "hello"}));

            let error = runtime
                .dispatch(
                    invocation,
                    &effective,
                    ActionDispatchControl::uncancellable(),
                )
                .unwrap_err();

            assert_eq!(
                error.denial().map(|denial| denial.code),
                Some(DispatchDenialCode::StaleDeclaration)
            );
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
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

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

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

        let error = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::CatalogChanged)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn conditional_effect_requires_declaration_and_effective_grant_before_dispatch() {
        let provider = conditional_provider(StateEffect::HostProcessExecution);
        let denied = policy(&provider, true);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = conditional_runtime(
            provider.clone(),
            StateEffect::HostProcessExecution,
            count.clone(),
        );
        let error = runtime
            .dispatch(
                invocation(&provider, &denied, json!({"value": "host"})),
                &denied,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ConditionalEffectDenied)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);

        let admitted = CapabilityPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([CapabilityGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([StateEffect::HostProcessExecution])])
        .resolve()
        .unwrap();
        runtime
            .dispatch(
                invocation(&provider, &admitted, json!({"value": "host"})),
                &admitted,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handler_cannot_request_an_undeclared_conditional_effect() {
        let provider = conditional_provider(StateEffect::ShellLoginStartup);
        let effective = CapabilityPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([CapabilityGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([
                StateEffect::HostProcessExecution,
                StateEffect::ShellLoginStartup,
            ])])
        .resolve()
        .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = conditional_runtime(
            provider.clone(),
            StateEffect::HostProcessExecution,
            count.clone(),
        );

        let error = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "forged"})),
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ConditionalEffectUndeclared)
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

    fn conditional_provider(effect: StateEffect) -> ProviderDeclaration {
        let mut provider = provider("Conditional echo");
        provider.actions[0] = provider.actions[0]
            .clone()
            .with_conditional_state_effects([effect]);
        provider
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

    fn conditional_runtime(
        provider: ProviderDeclaration,
        requested: StateEffect,
        dispatch_count: Arc<AtomicUsize>,
    ) -> ToolRuntime {
        let mut runtime = ToolRuntime::new();
        runtime.register_provider(provider).unwrap();
        runtime
            .register_handler(
                capability_id(),
                ConditionalHandler {
                    requested,
                    dispatch_count,
                },
            )
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
