use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use crate::{
    ArtifactAccess, ArtifactDeclaration, ArtifactId, ArtifactTargetSelector, DeclarationError,
    EffectId, ExtensionDescriptor, ExtensionId, ExtensionRegistration, ExtensionTrust,
    HookDeclaration, HookHandler, HookId, ResolvedArtifactTarget, ToolDeclaration,
    ToolDispatchContext, ToolDispatchControl, ToolHandler, ToolHandlerError, ToolId,
    ToolInvocation,
};
use crate::{
    AuthorityClass, DispatchDenial, DispatchDenialCode, EffectiveToolSet, KernelWorkflowEvent,
    MemoryToolEffectJournal, ToolEffectJournal, ToolEffectJournalError, ToolEffectLifecycleState,
    ToolEffectMachine, ToolOutcome,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    extensions: Vec<ExtensionDescriptor>,
    extension_index: BTreeMap<ExtensionId, usize>,
    hook_index: BTreeMap<HookId, usize>,
    tool_index: BTreeMap<ToolId, usize>,
    artifact_index: BTreeMap<ArtifactId, usize>,
    effect_authorities: BTreeMap<EffectId, AuthorityClass>,
    workflow_mappings: BTreeMap<(ToolId, String), KernelWorkflowEvent>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_extensions(
        declarations: impl IntoIterator<Item = ExtensionDescriptor>,
    ) -> Result<Self, ToolCatalogError> {
        let mut catalog = Self::new();
        for declaration in declarations {
            catalog.register(declaration)?;
        }
        Ok(catalog)
    }

    pub fn register(&mut self, declaration: ExtensionDescriptor) -> Result<(), ToolCatalogError> {
        declaration
            .validate()
            .map_err(ToolCatalogError::InvalidDeclaration)?;
        let extension_index = self.extensions.len();
        if self.extension_index.contains_key(&declaration.id) {
            return Err(ToolCatalogError::DuplicateExtension {
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
            if self.tool_index.contains_key(&action.id) {
                return Err(ToolCatalogError::DuplicateTool {
                    id: action.id.clone(),
                });
            }
        }
        for artifact in &declaration.artifacts {
            if self.artifact_index.contains_key(&artifact.id) {
                return Err(ToolCatalogError::DuplicateArtifact {
                    id: artifact.id.clone(),
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

        self.extension_index
            .insert(declaration.id.clone(), extension_index);
        for hook in &declaration.hooks {
            self.hook_index.insert(hook.id.clone(), extension_index);
        }
        for action in &declaration.tools {
            self.tool_index.insert(action.id.clone(), extension_index);
        }
        for artifact in &declaration.artifacts {
            self.artifact_index
                .insert(artifact.id.clone(), extension_index);
        }
        self.effect_authorities.extend(resolved_effects);
        self.workflow_mappings.extend(resolved_workflow);
        self.extensions.push(declaration);
        Ok(())
    }

    pub fn extensions(&self) -> &[ExtensionDescriptor] {
        &self.extensions
    }

    pub fn extension(&self, id: &ExtensionId) -> Option<&ExtensionDescriptor> {
        self.extensions.get(*self.extension_index.get(id)?)
    }

    pub fn hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        let extension = self.extensions.get(*self.hook_index.get(id)?)?;
        extension.hooks.iter().find(|hook| &hook.id == id)
    }

    pub fn extension_for_hook(&self, id: &HookId) -> Option<&ExtensionDescriptor> {
        self.extensions.get(*self.hook_index.get(id)?)
    }

    pub fn trusted_hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        self.extension_for_hook(id)?
            .permits_execution()
            .then(|| self.hook(id))
            .flatten()
    }

    pub fn tool(&self, id: &ToolId) -> Option<&ToolDeclaration> {
        let extension = self.extensions.get(*self.tool_index.get(id)?)?;
        extension.tool(id)
    }

    pub fn extension_for_tool(&self, id: &ToolId) -> Option<&ExtensionDescriptor> {
        self.extensions.get(*self.tool_index.get(id)?)
    }

    pub fn artifact(&self, id: &ArtifactId) -> Option<&ArtifactDeclaration> {
        let extension = self.extensions.get(*self.artifact_index.get(id)?)?;
        extension.artifact(id)
    }

    pub fn validate_artifact_links(&self) -> Result<(), ToolCatalogError> {
        for extension in &self.extensions {
            for tool in &extension.tools {
                for link in &tool.artifact_links {
                    let ArtifactTargetSelector::Fixed(artifact_id) = &link.selector else {
                        continue;
                    };
                    let declaration = self.artifact(artifact_id).ok_or_else(|| {
                        ToolCatalogError::UnknownArtifact {
                            tool_id: tool.id.clone(),
                            artifact_id: artifact_id.clone(),
                        }
                    })?;
                    if !declaration.permits(link.access) {
                        return Err(ToolCatalogError::ArtifactAccessMismatch {
                            tool_id: tool.id.clone(),
                            artifact_id: artifact_id.clone(),
                            requested: link.access,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub fn resolve_artifact_targets(
        &self,
        tool_id: &ToolId,
        arguments: &serde_json::Value,
    ) -> Result<Vec<ResolvedArtifactTarget>, DeclarationError> {
        let extension = self
            .extension_for_tool(tool_id)
            .expect("registered Tool has an owning Extension");
        let tool = extension
            .tool(tool_id)
            .expect("registered Tool index points to its declaration");
        let mut targets = Vec::new();
        for link in &tool.artifact_links {
            let artifact_id = link.resolve(arguments)?;
            let owner = artifact_id.owner();
            if owner != extension.id
                && !extension
                    .requirements
                    .iter()
                    .any(|requirement| requirement.extension_id == owner)
            {
                return Err(DeclarationError::ForeignArtifactOwner {
                    artifact_id,
                    extension_id: extension.id.clone(),
                });
            }
            let declaration =
                self.artifact(&artifact_id)
                    .ok_or_else(|| DeclarationError::UnknownArtifact {
                        tool_id: tool.id.clone(),
                        artifact_id: artifact_id.clone(),
                    })?;
            if !declaration.permits(link.access) {
                return Err(DeclarationError::ArtifactAccessMismatch {
                    tool_id: tool.id.clone(),
                    artifact_id,
                    requested: link.access,
                });
            }
            targets.push(ResolvedArtifactTarget {
                effect_id: link.effect_id.clone(),
                artifact_id,
                access: link.access,
            });
        }
        Ok(targets)
    }

    pub fn executable_tool(&self, id: &ToolId) -> Result<&ToolDeclaration, ToolDispatchError> {
        let action = self
            .tool(id)
            .ok_or_else(|| ToolDispatchError::UnknownTool { id: id.clone() })?;
        let extension = self
            .extension_for_tool(id)
            .expect("tool index must reference its extension");
        if extension.permits_execution() {
            Ok(action)
        } else {
            Err(ToolDispatchError::UntrustedExtension {
                tool_id: id.clone(),
                extension_id: extension.id.clone(),
                trust: extension.trust,
            })
        }
    }

    pub fn has_hook(&self, id: &HookId) -> bool {
        self.hook_index.contains_key(id)
    }

    pub fn tool_ids(&self) -> impl ExactSizeIterator<Item = &ToolId> {
        self.tool_index.keys()
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
    hook_handlers: BTreeMap<HookId, std::sync::Arc<dyn HookHandler>>,
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
            hook_handlers: BTreeMap::new(),
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

        let declared_hooks = descriptor
            .hooks
            .iter()
            .map(|hook| hook.id.clone())
            .collect::<BTreeSet<_>>();
        let mut bound_hooks = BTreeSet::new();
        for binding in registration.hook_bindings() {
            if !bound_hooks.insert(binding.hook_id().clone())
                || self.hook_handlers.contains_key(binding.hook_id())
            {
                return Err(ToolCatalogError::DuplicateHookHandler {
                    id: binding.hook_id().clone(),
                });
            }
            if !declared_hooks.contains(binding.hook_id()) {
                return Err(ToolCatalogError::UndeclaredHookHandler {
                    id: binding.hook_id().clone(),
                });
            }
        }
        if let Some(id) = declared_hooks.difference(&bound_hooks).next() {
            return Err(ToolCatalogError::MissingHookHandler { id: id.clone() });
        }

        let (descriptor, bindings, hook_bindings) = registration.into_parts();
        let mut next_catalog = self.catalog.clone();
        next_catalog.register(descriptor)?;
        let mut next_handlers = self.handlers.clone();
        for binding in bindings {
            let (tool_id, handler) = binding.into_parts();
            next_handlers.insert(tool_id, handler);
        }
        let mut next_hook_handlers = self.hook_handlers.clone();
        for binding in hook_bindings {
            let (hook_id, handler) = binding.into_parts();
            next_hook_handlers.insert(hook_id, handler);
        }
        self.catalog = next_catalog;
        self.handlers = next_handlers;
        self.hook_handlers = next_hook_handlers;
        Ok(())
    }

    pub fn handler_ids(&self) -> impl ExactSizeIterator<Item = &ToolId> {
        self.handlers.keys()
    }

    pub fn hook_handler_ids(&self) -> impl ExactSizeIterator<Item = &HookId> {
        self.hook_handlers.keys()
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
        let tool_id = invocation.tool_id.clone();
        let extension_id = invocation.extension_id.clone();
        let schema_digest = invocation.declaration_digest.clone();
        let declaration = effective
            .authorize(&invocation, self.catalog.extensions())
            .map_err(ToolDispatchError::Denied)?;
        self.catalog
            .resolve_artifact_targets(&invocation.tool_id, &invocation.arguments)
            .map_err(ToolDispatchError::ArtifactTarget)?;
        let handler = self.handlers.get(&invocation.tool_id).ok_or_else(|| {
            ToolDispatchError::MissingHandler {
                id: invocation.tool_id.clone(),
            }
        })?;
        let effective_tool = effective
            .tool(&invocation.tool_id)
            .expect("authorized invocation must have an effective tool")
            .clone();
        let requested_effects = handler
            .preflight(&invocation)
            .map_err(ToolDispatchError::Handler)?;
        if !requested_effects.is_subset(&effective_tool.declaration().conditional_state_effects) {
            return Err(ToolDispatchError::Denied(DispatchDenial {
                tool_id: invocation.tool_id.clone(),
                code: DispatchDenialCode::ConditionalEffectUndeclared,
            }));
        }
        if !requested_effects.is_subset(effective_tool.authorized_state_effects()) {
            return Err(ToolDispatchError::Denied(DispatchDenial {
                tool_id: invocation.tool_id.clone(),
                code: DispatchDenialCode::ConditionalEffectDenied,
            }));
        }
        let admitted_effects = declaration
            .state_effects
            .iter()
            .chain(&requested_effects)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut effect_journal = EffectJournalContext {
            machine: ToolEffectMachine::new(
                call_id.clone(),
                tool_id.clone(),
                extension_id.clone(),
                schema_digest.clone(),
                declaration.delivery,
                admitted_effects.clone(),
            ),
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
                effective_tool.grant_provenance().cloned(),
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
                    crate::ToolSchema::compile(&error_declaration.data_schema)
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
                if error_declaration.class == crate::ToolErrorClass::Recoverable {
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

struct EffectJournalContext {
    machine: ToolEffectMachine,
}

impl EffectJournalContext {
    fn append(
        &mut self,
        journal: &mut dyn ToolEffectJournal,
        state: ToolEffectLifecycleState,
        observed_effects: &[crate::ObservedEffect],
        outcome_code: Option<&str>,
    ) -> Result<String, ToolEffectJournalError> {
        let record = self
            .machine
            .apply(
                state,
                observed_effects.to_vec(),
                outcome_code.map(str::to_owned),
            )
            .map_err(|error| ToolEffectJournalError::new(error.to_string()))?;
        journal.append(&record)
    }

    fn append_terminal(
        &mut self,
        journal: &mut dyn ToolEffectJournal,
        state: ToolEffectLifecycleState,
        observed_effects: &[crate::ObservedEffect],
        outcome_code: Option<&str>,
    ) -> Result<Option<String>, ToolDispatchError> {
        if self.machine.admitted_effects().is_empty() {
            return Ok(None);
        }
        self.append(journal, state, observed_effects, outcome_code)
            .map(Some)
            .map_err(|error| ToolDispatchError::OutcomeUnknown {
                id: self.machine.tool_id().clone(),
                message: format!("effect completed but terminal journal append failed: {error}"),
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCatalogError {
    InvalidDeclaration(DeclarationError),
    DuplicateExtension {
        id: ExtensionId,
    },
    DuplicateHook {
        id: HookId,
    },
    DuplicateTool {
        id: ToolId,
    },
    DuplicateArtifact {
        id: ArtifactId,
    },
    UnknownArtifact {
        tool_id: ToolId,
        artifact_id: ArtifactId,
    },
    ArtifactAccessMismatch {
        tool_id: ToolId,
        artifact_id: ArtifactId,
        requested: ArtifactAccess,
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
    DuplicateHookHandler {
        id: HookId,
    },
    MissingHookHandler {
        id: HookId,
    },
    UndeclaredHookHandler {
        id: HookId,
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
        event_id: crate::WorkflowEventId,
    },
}

impl Display for ToolCatalogError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration(error) => Display::fmt(error, formatter),
            Self::DuplicateExtension { id } => write!(formatter, "duplicate extension ID `{id}`"),
            Self::DuplicateHook { id } => write!(formatter, "duplicate hook ID `{id}`"),
            Self::DuplicateTool { id } => {
                write!(formatter, "duplicate tool ID `{id}`")
            }
            Self::DuplicateArtifact { id } => write!(formatter, "duplicate Artifact ID `{id}`"),
            Self::UnknownArtifact {
                tool_id,
                artifact_id,
            } => write!(
                formatter,
                "Tool `{tool_id}` references unknown Artifact `{artifact_id}`"
            ),
            Self::ArtifactAccessMismatch {
                tool_id,
                artifact_id,
                requested,
            } => write!(
                formatter,
                "Tool `{tool_id}` requests {requested:?} for Artifact `{artifact_id}` outside its declaration"
            ),
            Self::DuplicateHandler { id } => {
                write!(formatter, "duplicate tool handler for `{id}`")
            }
            Self::MissingHandler { id } => {
                write!(formatter, "tool declaration `{id}` has no handler")
            }
            Self::UndeclaredHandler { id } => {
                write!(formatter, "tool handler `{id}` has no declaration")
            }
            Self::DuplicateHookHandler { id } => {
                write!(formatter, "duplicate hook handler for `{id}`")
            }
            Self::MissingHookHandler { id } => {
                write!(formatter, "hook declaration `{id}` has no handler")
            }
            Self::UndeclaredHookHandler { id } => {
                write!(formatter, "hook handler `{id}` has no declaration")
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
    UnknownTool {
        id: ToolId,
    },
    MissingHandler {
        id: ToolId,
    },
    UntrustedExtension {
        tool_id: ToolId,
        extension_id: ExtensionId,
        trust: ExtensionTrust,
    },
    Denied(DispatchDenial),
    ArtifactTarget(DeclarationError),
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
            Self::UnknownTool { id } => write!(formatter, "unknown tool `{id}`"),
            Self::MissingHandler { id } => write!(formatter, "tool `{id}` has no handler"),
            Self::UntrustedExtension {
                tool_id,
                extension_id,
                trust,
            } => write!(
                formatter,
                "tool `{tool_id}` extension `{extension_id}` is not trusted: {}",
                trust.as_str()
            ),
            Self::Denied(denial) => Display::fmt(denial, formatter),
            Self::ArtifactTarget(error) => Display::fmt(error, formatter),
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
            Self::ArtifactTarget(error) => Some(error),
            Self::Handler(error) => Some(error),
            Self::Journal(error) => Some(error),
            _ => None,
        }
    }
}

pub fn verify_handler_coverage(runtime: &ToolRuntime) -> Result<(), HandlerCoverageError> {
    let declared = runtime.catalog.tool_ids().cloned().collect::<BTreeSet<_>>();
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
