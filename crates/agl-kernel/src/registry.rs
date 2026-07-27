use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use crate::{
    AuthorityClass, DispatchDenial, DispatchDenialCode, EffectiveToolSet, KernelWorkflowEvent,
    MemoryToolEffectJournal, ToolEffectJournal, ToolEffectJournalError, ToolEffectJournalRecord,
    ToolEffectLifecycleState, ToolOutcome,
};
use agl_extension::{
    DeclarationError, EffectId, ExtensionDescriptor, ExtensionId, ExtensionRegistration,
    ExtensionTrust, HookDeclaration, HookId, ToolDeclaration, ToolDispatchContext,
    ToolDispatchControl, ToolHandler, ToolHandlerError, ToolId, ToolInvocation,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    providers: Vec<ExtensionDescriptor>,
    provider_index: BTreeMap<ExtensionId, usize>,
    hook_index: BTreeMap<HookId, usize>,
    capability_index: BTreeMap<ToolId, usize>,
    effect_authorities: BTreeMap<EffectId, AuthorityClass>,
    workflow_mappings: BTreeMap<(ToolId, String), KernelWorkflowEvent>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, declaration: ExtensionDescriptor) -> Result<(), ToolCatalogError> {
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
        for action in &declaration.tools {
            if self.capability_index.contains_key(&action.id) {
                return Err(ToolCatalogError::DuplicateCapability {
                    id: action.id.clone(),
                });
            }
        }
        let mut resolved_effects = Vec::with_capacity(declaration.effects.len());
        for effect in &declaration.effects {
            let authority = AuthorityClass::parse(&effect.authority_class).ok_or_else(|| {
                ToolCatalogError::UnknownAuthorityClass {
                    effect_id: effect.id.clone(),
                    authority_class: effect.authority_class.clone(),
                }
            })?;
            if let Some(existing) = self.effect_authorities.get(&effect.id)
                && *existing != authority
            {
                return Err(ToolCatalogError::ConflictingEffectAuthority {
                    effect_id: effect.id.clone(),
                    existing: *existing,
                    requested: authority,
                });
            }
            resolved_effects.push((effect.id.clone(), authority));
        }
        let mut resolved_workflow = Vec::new();
        if let Some(workflow) = &declaration.workflow {
            for mapping in &workflow.mappings {
                let event = KernelWorkflowEvent::parse(&mapping.event_id).ok_or_else(|| {
                    ToolCatalogError::UnknownWorkflowEvent {
                        event_id: mapping.event_id.clone(),
                    }
                })?;
                resolved_workflow.push((
                    (mapping.tool_id.clone(), mapping.outcome_code.clone()),
                    event,
                ));
            }
        }

        self.provider_index
            .insert(declaration.id.clone(), provider_index);
        for hook in &declaration.hooks {
            self.hook_index.insert(hook.id.clone(), provider_index);
        }
        for action in &declaration.tools {
            self.capability_index
                .insert(action.id.clone(), provider_index);
        }
        self.effect_authorities.extend(resolved_effects);
        self.workflow_mappings.extend(resolved_workflow);
        self.providers.push(declaration);
        Ok(())
    }

    pub fn providers(&self) -> &[ExtensionDescriptor] {
        &self.providers
    }

    pub fn provider(&self, id: &ExtensionId) -> Option<&ExtensionDescriptor> {
        self.providers.get(*self.provider_index.get(id)?)
    }

    pub fn hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        let provider = self.providers.get(*self.hook_index.get(id)?)?;
        provider.hooks.iter().find(|hook| &hook.id == id)
    }

    pub fn provider_for_hook(&self, id: &HookId) -> Option<&ExtensionDescriptor> {
        self.providers.get(*self.hook_index.get(id)?)
    }

    pub fn trusted_hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        self.provider_for_hook(id)?
            .permits_execution()
            .then(|| self.hook(id))
            .flatten()
    }

    pub fn tool(&self, id: &ToolId) -> Option<&ToolDeclaration> {
        let provider = self.providers.get(*self.capability_index.get(id)?)?;
        provider.tool(id)
    }

    pub fn extension_for_tool(&self, id: &ToolId) -> Option<&ExtensionDescriptor> {
        self.providers.get(*self.capability_index.get(id)?)
    }

    pub fn executable_tool(&self, id: &ToolId) -> Result<&ToolDeclaration, ToolDispatchError> {
        let action = self
            .tool(id)
            .ok_or_else(|| ToolDispatchError::UnknownCapability { id: id.clone() })?;
        let provider = self
            .extension_for_tool(id)
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

    pub fn capability_ids(&self) -> impl ExactSizeIterator<Item = &ToolId> {
        self.capability_index.keys()
    }

    pub fn authority_class(&self, effect_id: &EffectId) -> Option<AuthorityClass> {
        self.effect_authorities.get(effect_id).copied()
    }

    pub fn workflow_event(
        &self,
        tool_id: &ToolId,
        outcome_code: &str,
    ) -> Option<KernelWorkflowEvent> {
        self.workflow_mappings
            .get(&(tool_id.clone(), outcome_code.to_owned()))
            .copied()
    }
}

pub struct ToolRuntime {
    catalog: ToolCatalog,
    handlers: BTreeMap<ToolId, std::sync::Arc<dyn ToolHandler>>,
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

    pub fn register_extension(
        &mut self,
        registration: ExtensionRegistration,
    ) -> Result<(), ToolCatalogError> {
        let descriptor = registration.descriptor();
        descriptor
            .validate()
            .map_err(ToolCatalogError::InvalidDeclaration)?;
        let declared = descriptor
            .tools
            .iter()
            .map(|tool| tool.id.clone())
            .collect::<BTreeSet<_>>();
        let mut bound = BTreeSet::new();
        for binding in registration.bindings() {
            if !bound.insert(binding.tool_id().clone()) {
                return Err(ToolCatalogError::DuplicateHandler {
                    id: binding.tool_id().clone(),
                });
            }
            if !declared.contains(binding.tool_id()) {
                return Err(ToolCatalogError::UndeclaredHandler {
                    id: binding.tool_id().clone(),
                });
            }
            if self.handlers.contains_key(binding.tool_id()) {
                return Err(ToolCatalogError::DuplicateHandler {
                    id: binding.tool_id().clone(),
                });
            }
        }
        if let Some(id) = declared.difference(&bound).next() {
            return Err(ToolCatalogError::MissingHandler { id: id.clone() });
        }

        let (descriptor, bindings) = registration.into_parts();
        let mut next_catalog = self.catalog.clone();
        next_catalog.register(descriptor)?;
        let mut next_handlers = self.handlers.clone();
        for binding in bindings {
            let (tool_id, handler) = binding.into_parts();
            next_handlers.insert(tool_id, handler);
        }
        self.catalog = next_catalog;
        self.handlers = next_handlers;
        Ok(())
    }

    pub fn handler_ids(&self) -> impl ExactSizeIterator<Item = &ToolId> {
        self.handlers.keys()
    }

    pub fn dispatch(
        &self,
        invocation: ToolInvocation,
        effective: &EffectiveToolSet,
        control: ToolDispatchControl,
    ) -> Result<ToolOutcome, ToolDispatchError> {
        let mut journal = MemoryToolEffectJournal::default();
        self.dispatch_with_journal(invocation, effective, control, &mut journal)
    }

    pub fn dispatch_with_journal(
        &self,
        invocation: ToolInvocation,
        effective: &EffectiveToolSet,
        control: ToolDispatchControl,
        journal: &mut dyn ToolEffectJournal,
    ) -> Result<ToolOutcome, ToolDispatchError> {
        let call_id = invocation
            .scope
            .step_id()
            .map(|step_id| format!("{}:{step_id}", invocation.scope.run_id()))
            .unwrap_or_else(|| invocation.scope.run_id().to_string());
        let tool_id = invocation.capability_id.clone();
        let extension_id = invocation.provider_id.clone();
        let schema_digest = invocation.declaration_digest.clone();
        let declaration = effective
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
                code: DispatchDenialCode::ConditionalEffectUndeclared,
            }));
        }
        if !requested_effects.is_subset(effective_capability.authorized_state_effects()) {
            return Err(ToolDispatchError::Denied(DispatchDenial {
                capability_id: invocation.capability_id.clone(),
                code: DispatchDenialCode::ConditionalEffectDenied,
            }));
        }
        let admitted_effects = declaration
            .state_effects
            .iter()
            .chain(&requested_effects)
            .cloned()
            .collect::<BTreeSet<_>>();
        let effect_journal = EffectJournalContext {
            call_id: &call_id,
            tool_id: &tool_id,
            extension_id: &extension_id,
            schema_digest: &schema_digest,
            delivery: declaration.delivery,
            admitted_effects: &admitted_effects,
        };
        let mut admitted_receipt_refs = Vec::new();
        if !admitted_effects.is_empty() {
            admitted_receipt_refs.push(
                effect_journal
                    .append(journal, ToolEffectLifecycleState::Admitted, &[], None)
                    .map_err(ToolDispatchError::Journal)?,
            );
            effect_journal
                .append(journal, ToolEffectLifecycleState::Started, &[], None)
                .map_err(ToolDispatchError::Journal)?;
        }
        if control.is_cancelled() || control.is_expired() {
            if !admitted_effects.is_empty() {
                effect_journal
                    .append(
                        journal,
                        ToolEffectLifecycleState::Cancelled,
                        &[],
                        Some("cancelled"),
                    )
                    .map_err(ToolDispatchError::Journal)?;
            }
            return Err(ToolDispatchError::Cancelled { id: tool_id });
        }
        let handler_control = control.clone();
        let handler_result = block_on_handler(
            handler.dispatch(ToolDispatchContext::new(
                invocation,
                control,
                requested_effects,
                effective_capability.grant_provenance().cloned(),
            )),
            &handler_control,
        );
        let handler_result = match handler_result {
            HandlerAwait::Ready(result) => result,
            HandlerAwait::Interrupted if admitted_effects.is_empty() => {
                return Err(ToolDispatchError::Cancelled { id: tool_id });
            }
            HandlerAwait::Interrupted => {
                effect_journal.append_terminal(
                    journal,
                    ToolEffectLifecycleState::OutcomeUnknown,
                    &[],
                    Some("outcome_unknown"),
                )?;
                return Err(ToolDispatchError::OutcomeUnknown {
                    id: tool_id,
                    message: "cancellation or deadline interrupted a started effect".to_string(),
                });
            }
        };
        match handler_result {
            Ok(result) => {
                if declaration.declared_outcome(&result.outcome_code).is_none() {
                    effect_journal.append_terminal(
                        journal,
                        ToolEffectLifecycleState::Failed,
                        &[],
                        Some("undeclared_outcome"),
                    )?;
                    return Err(ToolDispatchError::UndeclaredOutcome {
                        id: declaration.id.clone(),
                        code: result.outcome_code,
                    });
                }
                if let Err(error) = declaration
                    .compile_output_schema()
                    .map_err(|error| ToolDispatchError::InvalidResult {
                        id: declaration.id.clone(),
                        message: error.to_string(),
                    })?
                    .validate(&result.data)
                    .map_err(|error| ToolDispatchError::InvalidResult {
                        id: declaration.id.clone(),
                        message: error.to_string(),
                    })
                {
                    effect_journal.append_terminal(
                        journal,
                        ToolEffectLifecycleState::Failed,
                        &[],
                        Some("invalid_result"),
                    )?;
                    return Err(error);
                }
                let invalid_observed_effect = if result.observed_effects.len() > 128 {
                    Some("more than 128 observed effect receipts were returned".to_owned())
                } else {
                    result.observed_effects.iter().find_map(|effect| {
                        (!admitted_effects.contains(&effect.effect_id)
                            || effect.scope.is_empty()
                            || effect.scope.len() > 32
                            || effect.scope.iter().any(|(key, value)| {
                                key.is_empty()
                                    || key.len() > 128
                                    || value.len() > 4096
                                    || value.contains('\0')
                            }))
                        .then(|| {
                            format!(
                                "observed effect `{}` is undeclared or has an invalid scope",
                                effect.effect_id
                            )
                        })
                    })
                };
                if let Some(message) = invalid_observed_effect {
                    let lifecycle = if admitted_effects.is_empty() {
                        ToolEffectLifecycleState::Failed
                    } else {
                        ToolEffectLifecycleState::OutcomeUnknown
                    };
                    effect_journal.append_terminal(
                        journal,
                        lifecycle,
                        &[],
                        Some("invalid_observed_effect"),
                    )?;
                    return Err(if admitted_effects.is_empty() {
                        ToolDispatchError::InvalidResult {
                            id: declaration.id.clone(),
                            message,
                        }
                    } else {
                        ToolDispatchError::OutcomeUnknown {
                            id: declaration.id.clone(),
                            message: format!(
                                "effect completed but its observed receipt was invalid: {message}"
                            ),
                        }
                    });
                }
                let terminal_receipt = effect_journal.append_terminal(
                    journal,
                    ToolEffectLifecycleState::Committed,
                    &result.observed_effects,
                    Some(&result.outcome_code),
                )?;
                let workflow_event = self
                    .catalog
                    .workflow_event(&tool_id, &result.outcome_code)
                    .map(KernelWorkflowEvent::id);
                Ok(
                    ToolOutcome::succeeded(call_id, tool_id, extension_id, schema_digest, result)
                        .with_workflow_event(workflow_event)
                        .with_effect_receipts(admitted_receipt_refs, terminal_receipt),
                )
            }
            Err(error) => {
                let Some(error_declaration) = declaration.declared_error(&error.code) else {
                    effect_journal.append_terminal(
                        journal,
                        ToolEffectLifecycleState::Failed,
                        &[],
                        Some("undeclared_handler_error"),
                    )?;
                    return Err(ToolDispatchError::UndeclaredHandlerError {
                        id: declaration.id.clone(),
                        code: error.code,
                    });
                };
                if let Err(schema_error) =
                    agl_extension::ToolSchema::compile(&error_declaration.data_schema)
                        .map_err(|schema_error| ToolDispatchError::InvalidHandlerError {
                            id: declaration.id.clone(),
                            code: error.code.clone(),
                            message: schema_error.to_string(),
                        })?
                        .validate(&error.data)
                        .map_err(|schema_error| ToolDispatchError::InvalidHandlerError {
                            id: declaration.id.clone(),
                            code: error.code.clone(),
                            message: schema_error.to_string(),
                        })
                {
                    effect_journal.append_terminal(
                        journal,
                        ToolEffectLifecycleState::Failed,
                        &[],
                        Some("invalid_handler_error"),
                    )?;
                    return Err(schema_error);
                }
                let lifecycle = if error.code == "outcome_unknown" {
                    ToolEffectLifecycleState::OutcomeUnknown
                } else {
                    ToolEffectLifecycleState::Failed
                };
                let terminal_receipt =
                    effect_journal.append_terminal(journal, lifecycle, &[], Some(&error.code))?;
                if error_declaration.class == agl_extension::ToolErrorClass::Recoverable {
                    let workflow_event = self
                        .catalog
                        .workflow_event(&tool_id, &error.code)
                        .map(KernelWorkflowEvent::id);
                    Ok(ToolOutcome::recoverable(
                        call_id,
                        tool_id,
                        extension_id,
                        schema_digest,
                        error_declaration,
                        error,
                    )
                    .with_workflow_event(workflow_event)
                    .with_effect_receipts(admitted_receipt_refs, terminal_receipt))
                } else {
                    Err(ToolDispatchError::Handler(error))
                }
            }
        }
    }
}

enum HandlerAwait<T> {
    Ready(T),
    Interrupted,
}

fn block_on_handler<T>(
    future: impl std::future::Future<Output = T>,
    control: &ToolDispatchControl,
) -> HandlerAwait<T> {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWake(std::thread::Thread);

    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return HandlerAwait::Ready(output),
            Poll::Pending if control.is_cancelled() || control.is_expired() => {
                return HandlerAwait::Interrupted;
            }
            Poll::Pending => match control.remaining() {
                Some(remaining) => {
                    std::thread::park_timeout(remaining.min(std::time::Duration::from_millis(10)))
                }
                None => std::thread::park_timeout(std::time::Duration::from_millis(10)),
            },
        }
    }
}

struct EffectJournalContext<'a> {
    call_id: &'a str,
    tool_id: &'a ToolId,
    extension_id: &'a ExtensionId,
    schema_digest: &'a agl_extension::DeclarationDigest,
    delivery: agl_extension::ToolDelivery,
    admitted_effects: &'a BTreeSet<agl_extension::EffectId>,
}

impl EffectJournalContext<'_> {
    fn append(
        &self,
        journal: &mut dyn ToolEffectJournal,
        state: ToolEffectLifecycleState,
        observed_effects: &[agl_extension::ObservedEffect],
        outcome_code: Option<&str>,
    ) -> Result<String, ToolEffectJournalError> {
        journal.append(&ToolEffectJournalRecord {
            call_id: self.call_id.to_owned(),
            tool_id: self.tool_id.clone(),
            extension_id: self.extension_id.clone(),
            schema_digest: self.schema_digest.clone(),
            delivery: self.delivery,
            state,
            admitted_effects: self.admitted_effects.clone(),
            observed_effects: observed_effects.to_vec(),
            outcome_code: outcome_code.map(str::to_owned),
        })
    }

    fn append_terminal(
        &self,
        journal: &mut dyn ToolEffectJournal,
        state: ToolEffectLifecycleState,
        observed_effects: &[agl_extension::ObservedEffect],
        outcome_code: Option<&str>,
    ) -> Result<Option<String>, ToolDispatchError> {
        if self.admitted_effects.is_empty() {
            return Ok(None);
        }
        self.append(journal, state, observed_effects, outcome_code)
            .map(Some)
            .map_err(|error| ToolDispatchError::OutcomeUnknown {
                id: self.tool_id.clone(),
                message: format!("effect completed but terminal journal append failed: {error}"),
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCatalogError {
    InvalidDeclaration(DeclarationError),
    DuplicateProvider {
        id: ExtensionId,
    },
    DuplicateHook {
        id: HookId,
    },
    DuplicateCapability {
        id: ToolId,
    },
    DuplicateHandler {
        id: ToolId,
    },
    MissingHandler {
        id: ToolId,
    },
    UndeclaredHandler {
        id: ToolId,
    },
    UnknownAuthorityClass {
        effect_id: EffectId,
        authority_class: String,
    },
    ConflictingEffectAuthority {
        effect_id: EffectId,
        existing: AuthorityClass,
        requested: AuthorityClass,
    },
    UnknownWorkflowEvent {
        event_id: agl_extension::WorkflowEventId,
    },
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
                write!(formatter, "duplicate tool handler for `{id}`")
            }
            Self::MissingHandler { id } => {
                write!(formatter, "tool declaration `{id}` has no handler")
            }
            Self::UndeclaredHandler { id } => {
                write!(formatter, "tool handler `{id}` has no declaration")
            }
            Self::UnknownAuthorityClass {
                effect_id,
                authority_class,
            } => write!(
                formatter,
                "effect `{effect_id}` requests unknown kernel authority class `{authority_class}`"
            ),
            Self::ConflictingEffectAuthority {
                effect_id,
                existing,
                requested,
            } => write!(
                formatter,
                "effect `{effect_id}` maps to conflicting authority classes `{}` and `{}`",
                existing.as_str(),
                requested.as_str()
            ),
            Self::UnknownWorkflowEvent { event_id } => {
                write!(formatter, "unknown kernel workflow event `{event_id}`")
            }
        }
    }
}

impl std::error::Error for ToolCatalogError {}

#[derive(Debug)]
pub enum ToolDispatchError {
    UnknownCapability {
        id: ToolId,
    },
    MissingHandler {
        id: ToolId,
    },
    UntrustedProvider {
        capability_id: ToolId,
        provider_id: ExtensionId,
        trust: ExtensionTrust,
    },
    Denied(DispatchDenial),
    Handler(ToolHandlerError),
    InvalidResult {
        id: ToolId,
        message: String,
    },
    UndeclaredHandlerError {
        id: ToolId,
        code: String,
    },
    UndeclaredOutcome {
        id: ToolId,
        code: String,
    },
    InvalidHandlerError {
        id: ToolId,
        code: String,
        message: String,
    },
    Journal(ToolEffectJournalError),
    Cancelled {
        id: ToolId,
    },
    OutcomeUnknown {
        id: ToolId,
        message: String,
    },
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
            Self::InvalidResult { id, message } => {
                write!(
                    formatter,
                    "tool `{id}` returned an invalid result: {message}"
                )
            }
            Self::UndeclaredHandlerError { id, code } => {
                write!(formatter, "tool `{id}` returned undeclared error `{code}`")
            }
            Self::UndeclaredOutcome { id, code } => {
                write!(
                    formatter,
                    "tool `{id}` returned undeclared outcome `{code}`"
                )
            }
            Self::InvalidHandlerError { id, code, message } => {
                write!(
                    formatter,
                    "tool `{id}` returned invalid `{code}` error data: {message}"
                )
            }
            Self::Journal(error) => write!(formatter, "effect journal append failed: {error}"),
            Self::Cancelled { id } => {
                write!(formatter, "tool `{id}` was cancelled before dispatch")
            }
            Self::OutcomeUnknown { id, message } => {
                write!(formatter, "tool `{id}` outcome is unknown: {message}")
            }
        }
    }
}

impl std::error::Error for ToolDispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Denied(error) => Some(error),
            Self::Handler(error) => Some(error),
            Self::Journal(error) => Some(error),
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
    pub missing: BTreeSet<ToolId>,
    pub undeclared: BTreeSet<ToolId>,
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

    use agl_extension::{
        EffectDeclaration, EffectId, ExtensionSource, ExtensionWorkflowFragment, HookEvent,
        OperationKind, SensitiveInput, ToolBinding, ToolDeclaration, ToolErrorDeclaration,
        ToolHandlerFuture, ToolResult, ToolWorkflowMapping, WorkflowEventId,
    };
    use agl_ids::{ExecutionScope, RunId};
    use serde_json::json;

    use super::*;
    use crate::{ToolAccessMode, ToolGrant, ToolPolicyInput};

    #[derive(Clone)]
    struct CountingHandler(Arc<AtomicUsize>);

    impl ToolHandler for CountingHandler {
        fn dispatch(&self, context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
            Box::pin(async move {
                let invocation = context.into_invocation();
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult::new(json!({
                    "echo": invocation.arguments["value"]
                })))
            })
        }
    }

    #[derive(Clone)]
    struct ConditionalHandler {
        requested: EffectId,
        dispatch_count: Arc<AtomicUsize>,
    }

    impl ToolHandler for ConditionalHandler {
        fn preflight(
            &self,
            _invocation: &ToolInvocation,
        ) -> Result<BTreeSet<EffectId>, ToolHandlerError> {
            Ok([self.requested.clone()].into_iter().collect())
        }

        fn dispatch(&self, context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
            Box::pin(async move {
                assert!(
                    context
                        .authorized_conditional_effects()
                        .contains(&self.requested)
                );
                self.dispatch_count.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult::new(json!({"ok": true})).with_observed_effects([
                    agl_extension::ObservedEffect::new(
                        self.requested.clone(),
                        [("profile".to_owned(), "host".to_owned())],
                    ),
                ]))
            })
        }
    }

    #[derive(Clone)]
    struct ObservedReceiptHandler {
        requested: EffectId,
        observed: Vec<agl_extension::ObservedEffect>,
    }

    impl ToolHandler for ObservedReceiptHandler {
        fn preflight(
            &self,
            _invocation: &ToolInvocation,
        ) -> Result<BTreeSet<EffectId>, ToolHandlerError> {
            Ok([self.requested.clone()].into_iter().collect())
        }

        fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
            Box::pin(std::future::ready(Ok(
                ToolResult::new(json!({"ok": true})).with_observed_effects(self.observed.clone())
            )))
        }
    }

    #[derive(Clone)]
    struct ReturningHandler(Result<ToolResult, ToolHandlerError>);

    impl ToolHandler for ReturningHandler {
        fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
            Box::pin(std::future::ready(self.0.clone()))
        }
    }

    #[derive(Clone)]
    struct PendingHandler {
        requested: EffectId,
    }

    struct NotCancelled;

    impl agl_extension::CancellationSignal for NotCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    impl ToolHandler for PendingHandler {
        fn preflight(
            &self,
            _invocation: &ToolInvocation,
        ) -> Result<BTreeSet<EffectId>, ToolHandlerError> {
            Ok([self.requested.clone()].into_iter().collect())
        }

        fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn provider_qualified_hooks_keep_local_names_isolated_from_model_tools() {
        let mut catalog = ToolCatalog::new();
        for provider in ["alpha", "beta"] {
            catalog
                .register(
                    ExtensionDescriptor::new(
                        ExtensionId::new(provider).unwrap(),
                        format!("{provider} provider"),
                        "1",
                        ExtensionSource::TestFixture,
                        ExtensionTrust::TrustedRegistered,
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
        let duplicate = ExtensionDescriptor {
            id: ExtensionId::new("duplicate").unwrap(),
            name: "Duplicate".to_string(),
            version: "1".to_string(),
            source: ExtensionSource::TestFixture,
            trust: ExtensionTrust::TrustedRegistered,
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
            effects: Vec::new(),
            tools: Vec::new(),
            workflow: None,
        };
        assert!(matches!(
            ToolCatalog::new().register(duplicate),
            Err(ToolCatalogError::InvalidDeclaration(
                DeclarationError::DuplicateId { kind: "hook", .. }
            ))
        ));

        let mismatched = ExtensionDescriptor {
            id: ExtensionId::new("alpha").unwrap(),
            name: "Alpha".to_string(),
            version: "1".to_string(),
            source: ExtensionSource::TestFixture,
            trust: ExtensionTrust::TrustedRegistered,
            hooks: vec![HookDeclaration {
                id: HookId::new("beta:validate").unwrap(),
                event: HookEvent::ArtifactWrite,
                required: true,
            }],
            effects: Vec::new(),
            tools: Vec::new(),
            workflow: None,
        };
        assert!(matches!(
            ToolCatalog::new().register(mismatched),
            Err(ToolCatalogError::InvalidDeclaration(
                DeclarationError::HookProviderMismatch { .. }
            ))
        ));

        let core_shadow = ExtensionDescriptor {
            id: ExtensionId::new("core").unwrap(),
            name: "Core shadow".to_string(),
            version: "1".to_string(),
            source: ExtensionSource::ThirdPartyRegistered,
            trust: ExtensionTrust::TrustedRegistered,
            hooks: vec![HookDeclaration {
                id: HookId::new("core:validate").unwrap(),
                event: HookEvent::ArtifactWrite,
                required: true,
            }],
            effects: Vec::new(),
            tools: Vec::new(),
            workflow: None,
        };
        assert!(matches!(
            ToolCatalog::new().register(core_shadow),
            Err(ToolCatalogError::InvalidDeclaration(
                DeclarationError::ReservedProviderNamespace { .. }
            ))
        ));
    }

    #[test]
    fn failed_extension_registration_leaves_catalog_and_handlers_unchanged() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut runtime = runtime(provider("Echo"), count);
        let providers_before = runtime.catalog().providers().to_vec();
        let handlers_before = runtime.handler_ids().cloned().collect::<Vec<_>>();
        let declared_id = ToolId::new("second:declared").unwrap();
        let undeclared_id = ToolId::new("second:undeclared").unwrap();
        let second = ExtensionDescriptor::new(
            ExtensionId::new("second").unwrap(),
            "Second",
            "1",
            ExtensionSource::TestFixture,
            ExtensionTrust::TrustedRegistered,
        )
        .unwrap()
        .with_tool(
            ToolDeclaration::new(
                declared_id.clone(),
                "Declared",
                json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false
                }),
                OperationKind::Read,
            )
            .unwrap(),
        );

        let error = runtime
            .register_extension(ExtensionRegistration::new(second.clone(), []))
            .unwrap_err();
        assert_eq!(
            error,
            ToolCatalogError::MissingHandler {
                id: declared_id.clone()
            }
        );
        assert_eq!(runtime.catalog().providers(), providers_before);
        assert_eq!(
            runtime.handler_ids().cloned().collect::<Vec<_>>(),
            handlers_before
        );

        let error = runtime
            .register_extension(ExtensionRegistration::new(
                second,
                [ToolBinding::new(
                    undeclared_id.clone(),
                    ReturningHandler(Ok(ToolResult::new(json!({})))),
                )],
            ))
            .unwrap_err();
        assert_eq!(
            error,
            ToolCatalogError::UndeclaredHandler { id: undeclared_id }
        );
        assert_eq!(runtime.catalog().providers(), providers_before);
        assert_eq!(
            runtime.handler_ids().cloned().collect::<Vec<_>>(),
            handlers_before
        );
    }

    #[test]
    fn dispatch_rejects_result_outside_declared_output_schema() {
        let mut provider = provider("Strict output");
        provider.tools[0].output_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
            "additionalProperties": false
        });
        provider.validate().unwrap();
        let effective = policy(&provider, true);
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ReturningHandler(Ok(ToolResult::new(json!({"unexpected": true})))),
                )],
            ))
            .unwrap();

        let error = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "hello"})),
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert!(matches!(error, ToolDispatchError::InvalidResult { .. }));
    }

    #[test]
    fn dispatch_rejects_undeclared_success_outcome() {
        let provider = provider("Echo");
        let effective = policy(&provider, true);
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ReturningHandler(Ok(
                        ToolResult::new(json!({"echo": "hello"})).with_outcome_code("partial")
                    )),
                )],
            ))
            .unwrap();

        let error = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "hello"})),
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ToolDispatchError::UndeclaredOutcome { code, .. } if code == "partial"
        ));
    }

    #[test]
    fn kernel_validates_and_selects_extension_workflow_mapping() {
        let mut provider = provider("Echo");
        provider =
            provider.with_workflow(ExtensionWorkflowFragment::new([ToolWorkflowMapping::new(
                capability_id(),
                "success",
                WorkflowEventId::new(crate::TOOL_OBSERVATION_APPEND_EVENT_ID).unwrap(),
            )]));
        let effective = policy(&provider, true);
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ReturningHandler(Ok(ToolResult::new(json!({"echo": "hello"})))),
                )],
            ))
            .unwrap();

        let outcome = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "hello"})),
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap();
        assert_eq!(
            outcome.workflow_event.as_ref().map(WorkflowEventId::as_str),
            Some(crate::TOOL_OBSERVATION_APPEND_EVENT_ID)
        );
    }

    #[test]
    fn kernel_rejects_unknown_extension_workflow_event() {
        let provider = provider("Echo").with_workflow(ExtensionWorkflowFragment::new([
            ToolWorkflowMapping::new(
                capability_id(),
                "success",
                WorkflowEventId::new("third-party:take_over_fsm").unwrap(),
            ),
        ]));
        let error = ToolCatalog::new().register(provider).unwrap_err();
        assert!(matches!(
            error,
            ToolCatalogError::UnknownWorkflowEvent { event_id }
                if event_id.as_str() == "third-party:take_over_fsm"
        ));
    }

    #[test]
    fn undeclared_handler_error_is_terminal_kernel_failure() {
        let provider = provider("Echo");
        let effective = policy(&provider, true);
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ReturningHandler(Err(ToolHandlerError::new(
                        "conflict",
                        "stale value",
                        json!({}),
                    ))),
                )],
            ))
            .unwrap();

        let error = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "hello"})),
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ToolDispatchError::UndeclaredHandlerError { code, .. } if code == "conflict"
        ));
    }

    #[test]
    fn declared_recoverable_error_becomes_typed_outcome() {
        let mut provider = provider("Echo");
        provider.tools[0] = provider.tools[0]
            .clone()
            .with_errors([
                ToolErrorDeclaration::recoverable("conflict"),
                ToolErrorDeclaration::terminal("execution_failed"),
            ])
            .unwrap();
        let effective = policy(&provider, true);
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ReturningHandler(Err(ToolHandlerError::new(
                        "conflict",
                        "stale value",
                        json!({"expected": "one", "actual": "two"}),
                    ))),
                )],
            ))
            .unwrap();

        let outcome = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "hello"})),
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap();
        assert_eq!(outcome.status, crate::ToolOutcomeStatus::RecoverableError);
        assert_eq!(outcome.outcome_code, "conflict");
        assert_eq!(outcome.error.unwrap().code, "conflict");
    }

    #[test]
    fn invalid_arguments_are_denied_before_handler_execution() {
        let provider = provider("Echo");
        let effective = policy(&provider, true);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(provider.clone(), count.clone());
        let invocation = invocation(&provider, &effective, json!({"value": 7, "extra": true}));

        let error = runtime
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
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
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
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
        let changed = trusted.clone().with_trust(ExtensionTrust::Changed);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(changed, count.clone());
        let invocation = invocation(&trusted, &effective, json!({"value": "hello"}));

        let error = runtime
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
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
        let changed = trusted.clone().with_trust(ExtensionTrust::TrustedByBinary);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(changed, count.clone());
        let invocation = invocation(&trusted, &effective, json!({"value": "hello"}));

        let error = runtime
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
            .unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ExtensionTrustChanged)
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
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
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
            provider.tools[0] = ToolDeclaration::new(
                capability_id(),
                "Echo",
                original.tools[0].input_schema.clone(),
                OperationKind::Request,
            )
            .unwrap();
            provider
        };
        let effect_changed = {
            let mut provider = original.clone();
            provider.tools[0] = provider.tools[0]
                .clone()
                .with_state_effects([EffectId::host_screen_capture()])
                .with_sensitive_inputs([SensitiveInput::ScreenCapture]);
            provider.effects.push(EffectDeclaration::new(
                EffectId::host_screen_capture(),
                AuthorityClass::HostObservation.as_str(),
            ));
            provider
        };

        for current in [operation_changed, effect_changed] {
            let count = Arc::new(AtomicUsize::new(0));
            let runtime = runtime(current, count.clone());
            let invocation = invocation(&original, &effective, json!({"value": "hello"}));

            let error = runtime
                .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
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
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
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
        let secondary = ExtensionDescriptor::new(
            ExtensionId::new("secondary-provider").unwrap(),
            "Secondary Provider",
            "1",
            ExtensionSource::TestFixture,
            ExtensionTrust::TrustedRegistered,
        )
        .unwrap();
        let effective = ToolPolicyInput::new(
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
        runtime
            .register_extension(ExtensionRegistration::new(changed_secondary, []))
            .unwrap();
        let invocation = invocation(&primary, &effective, json!({"value": "hello"}));

        let error = runtime
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
            .unwrap_err();

        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::CatalogChanged)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn conditional_effect_requires_declaration_and_effective_grant_before_dispatch() {
        let provider = conditional_provider(EffectId::host_process_execution());
        let denied = policy(&provider, true);
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = conditional_runtime(
            provider.clone(),
            EffectId::host_process_execution(),
            count.clone(),
        );
        let error = runtime
            .dispatch(
                invocation(&provider, &denied, json!({"value": "host"})),
                &denied,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ConditionalEffectDenied)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);

        let admitted = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([ToolGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([EffectId::host_process_execution()])])
        .resolve()
        .unwrap();
        runtime
            .dispatch(
                invocation(&provider, &admitted, json!({"value": "host"})),
                &admitted,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mutating_dispatch_journals_admitted_started_and_committed_in_order() {
        let provider = conditional_provider(EffectId::host_process_execution());
        let effective = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([ToolGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([EffectId::host_process_execution()])])
        .resolve()
        .unwrap();
        let runtime = conditional_runtime(
            provider.clone(),
            EffectId::host_process_execution(),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut journal = crate::MemoryToolEffectJournal::default();

        let outcome = runtime
            .dispatch_with_journal(
                invocation(&provider, &effective, json!({"value": "journal"})),
                &effective,
                ToolDispatchControl::uncancellable(),
                &mut journal,
            )
            .unwrap();

        assert_eq!(
            journal
                .records()
                .iter()
                .map(|record| record.state)
                .collect::<Vec<_>>(),
            vec![
                ToolEffectLifecycleState::Admitted,
                ToolEffectLifecycleState::Started,
                ToolEffectLifecycleState::Committed,
            ]
        );
        assert_eq!(outcome.admitted_effect_receipt_refs.len(), 1);
        assert_eq!(outcome.observed_effect_receipt_refs.len(), 1);
        assert!(journal.records()[0].observed_effects.is_empty());
        assert_eq!(
            journal.records()[2].observed_effects[0].scope["profile"],
            "host"
        );
    }

    #[test]
    fn terminal_journal_loss_after_handler_is_outcome_unknown() {
        struct FailTerminalJournal {
            appends: usize,
        }

        impl ToolEffectJournal for FailTerminalJournal {
            fn append(
                &mut self,
                _record: &ToolEffectJournalRecord,
            ) -> Result<String, ToolEffectJournalError> {
                self.appends += 1;
                if self.appends == 3 {
                    Err(ToolEffectJournalError::new("injected terminal loss"))
                } else {
                    Ok(format!("effect:test:{}", self.appends))
                }
            }
        }

        let provider = conditional_provider(EffectId::host_process_execution());
        let effective = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([ToolGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([EffectId::host_process_execution()])])
        .resolve()
        .unwrap();
        let runtime = conditional_runtime(
            provider.clone(),
            EffectId::host_process_execution(),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut journal = FailTerminalJournal { appends: 0 };

        let error = runtime
            .dispatch_with_journal(
                invocation(&provider, &effective, json!({"value": "journal"})),
                &effective,
                ToolDispatchControl::uncancellable(),
                &mut journal,
            )
            .unwrap_err();
        assert!(matches!(error, ToolDispatchError::OutcomeUnknown { .. }));
    }

    #[test]
    fn invalid_receipt_after_started_effect_is_outcome_unknown() {
        let effect = EffectId::host_process_execution();
        let provider = conditional_provider(effect.clone());
        let effective = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([ToolGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([effect.clone()])])
        .resolve()
        .unwrap();
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ObservedReceiptHandler {
                        requested: effect.clone(),
                        observed: vec![agl_extension::ObservedEffect::new(effect, [])],
                    },
                )],
            ))
            .unwrap();
        let mut journal = MemoryToolEffectJournal::default();

        let error = runtime
            .dispatch_with_journal(
                invocation(&provider, &effective, json!({"value": "invalid"})),
                &effective,
                ToolDispatchControl::uncancellable(),
                &mut journal,
            )
            .unwrap_err();

        assert!(matches!(error, ToolDispatchError::OutcomeUnknown { .. }));
        assert_eq!(
            journal.records().last().unwrap().state,
            ToolEffectLifecycleState::OutcomeUnknown
        );
    }

    #[test]
    fn repeated_observed_effect_id_can_report_distinct_scopes() {
        let effect = EffectId::host_process_execution();
        let provider = conditional_provider(effect.clone());
        let effective = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([ToolGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([effect.clone()])])
        .resolve()
        .unwrap();
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    ObservedReceiptHandler {
                        requested: effect.clone(),
                        observed: ["first", "second"]
                            .into_iter()
                            .map(|path| {
                                agl_extension::ObservedEffect::new(
                                    effect.clone(),
                                    [("path".to_owned(), path.to_owned())],
                                )
                            })
                            .collect(),
                    },
                )],
            ))
            .unwrap();
        let mut journal = MemoryToolEffectJournal::default();

        runtime
            .dispatch_with_journal(
                invocation(&provider, &effective, json!({"value": "scoped"})),
                &effective,
                ToolDispatchControl::uncancellable(),
                &mut journal,
            )
            .unwrap();

        assert_eq!(journal.records().last().unwrap().observed_effects.len(), 2);
    }

    #[test]
    fn deadline_during_started_async_effect_is_outcome_unknown() {
        let effect = EffectId::host_process_execution();
        let provider = conditional_provider(effect.clone());
        let effective = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([ToolGrant::new(capability_id(), OperationKind::Read)
            .with_state_effects([effect.clone()])])
        .resolve()
        .unwrap();
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider.clone(),
                [ToolBinding::new(
                    capability_id(),
                    PendingHandler { requested: effect },
                )],
            ))
            .unwrap();
        let mut journal = MemoryToolEffectJournal::default();

        let error = runtime
            .dispatch_with_journal(
                invocation(&provider, &effective, json!({"value": "pending"})),
                &effective,
                ToolDispatchControl::new(
                    Arc::new(NotCancelled),
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(20)),
                ),
                &mut journal,
            )
            .unwrap_err();

        assert!(matches!(error, ToolDispatchError::OutcomeUnknown { .. }));
        assert_eq!(
            journal
                .records()
                .iter()
                .map(|record| record.state)
                .collect::<Vec<_>>(),
            [
                ToolEffectLifecycleState::Admitted,
                ToolEffectLifecycleState::Started,
                ToolEffectLifecycleState::OutcomeUnknown,
            ]
        );
    }

    #[test]
    fn handler_cannot_request_an_undeclared_conditional_effect() {
        let provider = conditional_provider(EffectId::shell_login_startup());
        let effective = ToolPolicyInput::new(
            [provider.clone()],
            [capability_id()],
            ToolAccessMode::ReadOnly,
        )
        .with_grants([
            ToolGrant::new(capability_id(), OperationKind::Read).with_state_effects([
                EffectId::host_process_execution(),
                EffectId::shell_login_startup(),
            ]),
        ])
        .resolve()
        .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = conditional_runtime(
            provider.clone(),
            EffectId::host_process_execution(),
            count.clone(),
        );

        let error = runtime
            .dispatch(
                invocation(&provider, &effective, json!({"value": "forged"})),
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert_eq!(
            error.denial().map(|denial| denial.code),
            Some(DispatchDenialCode::ConditionalEffectUndeclared)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    fn capability_id() -> ToolId {
        ToolId::new("example.echo").unwrap()
    }

    fn provider(description: &str) -> ExtensionDescriptor {
        ExtensionDescriptor::new(
            ExtensionId::new("example-provider").unwrap(),
            "Example Provider",
            "1",
            ExtensionSource::TestFixture,
            ExtensionTrust::TrustedRegistered,
        )
        .unwrap()
        .with_tool(
            ToolDeclaration::new(
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

    fn conditional_provider(effect: EffectId) -> ExtensionDescriptor {
        let mut provider = provider("Conditional echo");
        provider.tools[0] = provider.tools[0]
            .clone()
            .with_conditional_state_effects([effect.clone()]);
        let authority = match effect.as_str() {
            "agl:process.host_execution" => AuthorityClass::HostExecution,
            "agl:process.shell_login_startup" => AuthorityClass::ShellStartup,
            other => panic!("test effect `{other}` has no authority mapping"),
        };
        provider
            .effects
            .push(EffectDeclaration::new(effect, authority.as_str()));
        provider
    }

    fn policy(provider: &ExtensionDescriptor, routed: bool) -> EffectiveToolSet {
        ToolPolicyInput::new(
            [provider.clone()],
            routed.then(capability_id),
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap()
    }

    fn runtime(provider: ExtensionDescriptor, count: Arc<AtomicUsize>) -> ToolRuntime {
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider,
                [ToolBinding::new(capability_id(), CountingHandler(count))],
            ))
            .unwrap();
        runtime
    }

    fn conditional_runtime(
        provider: ExtensionDescriptor,
        requested: EffectId,
        dispatch_count: Arc<AtomicUsize>,
    ) -> ToolRuntime {
        let mut runtime = ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(
                provider,
                [ToolBinding::new(
                    capability_id(),
                    ConditionalHandler {
                        requested,
                        dispatch_count,
                    },
                )],
            ))
            .unwrap();
        runtime
    }

    fn invocation(
        provider: &ExtensionDescriptor,
        effective: &EffectiveToolSet,
        arguments: serde_json::Value,
    ) -> ToolInvocation {
        ToolInvocation::new(
            ExecutionScope::builder(RunId::generate()).build().unwrap(),
            capability_id(),
            provider.id.clone(),
            provider.tool(&capability_id()).unwrap().digest(),
            effective.policy_hash().clone(),
            arguments,
        )
    }
}
