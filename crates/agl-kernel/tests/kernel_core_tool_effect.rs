#[path = "core/support/mod.rs"]
mod support;
#[path = "core/support/tool_effect.rs"]
mod tool_effect_support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_ids::{ExecutionScope, RunId};
use agl_kernel::{
    AuthorityClass, MemoryToolEffectJournal, ToolAccessMode, ToolDispatchError, ToolEffectJournal,
    ToolEffectJournalError, ToolEffectJournalRecord, ToolEffectLifecycleState, ToolGrant,
    ToolPolicyInput, ToolRuntime,
};
use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionRegistration, ExtensionSource,
    ExtensionTrust, ObservedEffect, OperationKind, ToolBinding, ToolDelivery, ToolDispatchContext,
    ToolDispatchControl, ToolHandler, ToolHandlerError, ToolHandlerFuture, ToolInvocation,
    ToolResult,
};
use serde_json::json;
use support::{extension_id, tool_declaration, tool_id};
use tool_effect_support::{EffectTransition, ProductionToolEffectMachine};

#[derive(Clone)]
struct EffectHandler {
    effect: EffectId,
}

impl ToolHandler for EffectHandler {
    fn preflight(
        &self,
        _invocation: &ToolInvocation,
    ) -> Result<BTreeSet<EffectId>, agl_kernel::ToolHandlerError> {
        Ok(BTreeSet::from([self.effect.clone()]))
    }

    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        Box::pin(std::future::ready(Ok(ToolResult::new(json!({
            "ok": true
        }))
        .with_observed_effects([ObservedEffect::new(
            self.effect.clone(),
            [("path".to_string(), "README.md".to_string())],
        )]))))
    }
}

fn effect_fixture() -> (
    ToolRuntime,
    ExtensionDescriptor,
    agl_kernel::EffectiveToolSet,
    ToolInvocation,
) {
    effect_fixture_with_handler(EffectHandler {
        effect: EffectId::repo_files(),
    })
}

fn effect_fixture_with_handler(
    handler: impl ToolHandler + 'static,
) -> (
    ToolRuntime,
    ExtensionDescriptor,
    agl_kernel::EffectiveToolSet,
    ToolInvocation,
) {
    let effect = EffectId::repo_files();
    let mut tool = tool_declaration("example.workspace:write");
    tool.conditional_state_effects = BTreeSet::from([effect.clone()]);
    let descriptor = ExtensionDescriptor::new(
        extension_id("example.workspace"),
        "Workspace",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(EffectDeclaration::new(
        effect.clone(),
        AuthorityClass::RepositoryMutation.as_str(),
    ))
    .with_tool(tool);
    descriptor.validate().unwrap();
    let effective = ToolPolicyInput::new(
        [descriptor.clone()],
        [tool_id("example.workspace:write")],
        ToolAccessMode::ReadOnly,
    )
    .with_grants([
        ToolGrant::new(tool_id("example.workspace:write"), OperationKind::Read)
            .with_state_effects([effect.clone()]),
    ])
    .resolve()
    .unwrap();
    let invocation = ToolInvocation::new(
        ExecutionScope::builder(RunId::generate()).build().unwrap(),
        tool_id("example.workspace:write"),
        extension_id("example.workspace"),
        descriptor.tools[0].digest(),
        effective.policy_hash().clone(),
        json!({}),
    );
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(ExtensionRegistration::new(
            descriptor.clone(),
            [ToolBinding::new(
                tool_id("example.workspace:write"),
                handler,
            )],
        ))
        .unwrap();
    (runtime, descriptor, effective, invocation)
}

fn effect_machine(call_id: &str) -> ProductionToolEffectMachine {
    ProductionToolEffectMachine::new(
        call_id,
        tool_id("example.workspace:write"),
        extension_id("example.workspace"),
        agl_kernel::DeclarationDigest::from_json(&json!({"fixture": "effect-machine"})),
        ToolDelivery::AtMostOnce,
        BTreeSet::from([EffectId::repo_files()]),
    )
}

#[derive(Clone)]
struct ReturningEffectHandler {
    effect: EffectId,
    result: Result<ToolResult, ToolHandlerError>,
}

impl ToolHandler for ReturningEffectHandler {
    fn preflight(
        &self,
        _invocation: &ToolInvocation,
    ) -> Result<BTreeSet<EffectId>, ToolHandlerError> {
        Ok(BTreeSet::from([self.effect.clone()]))
    }

    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        Box::pin(std::future::ready(self.result.clone()))
    }
}

#[derive(Clone)]
struct PendingEffectHandler {
    effect: EffectId,
}

impl ToolHandler for PendingEffectHandler {
    fn preflight(
        &self,
        _invocation: &ToolInvocation,
    ) -> Result<BTreeSet<EffectId>, ToolHandlerError> {
        Ok(BTreeSet::from([self.effect.clone()]))
    }

    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        Box::pin(std::future::pending())
    }
}

struct CancelOnSecondCheck(AtomicUsize);

impl agl_kernel::CancellationSignal for CancelOnSecondCheck {
    fn is_cancelled(&self) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst) >= 1
    }
}

struct NeverCancelled;

impl agl_kernel::CancellationSignal for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

// KCT-EFFECT-001 and KCT-EFFECT-003. Mutation: skip Started or change fixed identity.
#[test]
fn accepted_transitions_produce_the_complete_legal_success_path() {
    let effects = BTreeSet::from([EffectId::repo_files()]);
    let mut machine = effect_machine("call-1");
    let expected = [
        (EffectTransition::Admit, ToolEffectLifecycleState::Admitted),
        (EffectTransition::Start, ToolEffectLifecycleState::Started),
        (
            EffectTransition::Commit,
            ToolEffectLifecycleState::Committed,
        ),
    ];

    for (transition, state) in expected {
        let record = machine
            .apply(transition)
            .unwrap_or_else(|error| panic!("transition {transition:?}: {error}"));
        assert_eq!(record.state, state);
        assert_eq!(record.call_id, "call-1");
        assert_eq!(record.tool_id, tool_id("example.workspace:write"));
        assert_eq!(record.extension_id, extension_id("example.workspace"));
        assert_eq!(
            record.schema_digest,
            agl_kernel::DeclarationDigest::from_json(&json!({
                "fixture": "effect-machine"
            }))
        );
        assert_eq!(record.delivery, ToolDelivery::AtMostOnce);
        assert_eq!(record.admitted_effects, effects);
    }
    assert_eq!(machine.state(), Some(ToolEffectLifecycleState::Committed));
}

// KCT-EFFECT-001. Mutation: remove one selected terminal edge.
#[test]
fn every_selected_terminal_branch_is_reachable_only_after_start() {
    for (transition, state) in [
        (
            EffectTransition::Commit,
            ToolEffectLifecycleState::Committed,
        ),
        (EffectTransition::Fail, ToolEffectLifecycleState::Failed),
        (
            EffectTransition::Cancel,
            ToolEffectLifecycleState::Cancelled,
        ),
        (
            EffectTransition::MarkOutcomeUnknown,
            ToolEffectLifecycleState::OutcomeUnknown,
        ),
    ] {
        let mut machine = effect_machine("call-terminal");
        machine.apply(EffectTransition::Admit).unwrap();
        machine.apply(EffectTransition::Start).unwrap();
        assert_eq!(machine.apply(transition).unwrap().state, state);
    }
}

// KCT-EFFECT-002. Mutation: accept a skipped, repeated or post-terminal transition.
#[test]
fn illegal_effect_transitions_leave_state_unchanged() {
    let illegal_prefixes = [
        vec![EffectTransition::Commit],
        vec![EffectTransition::Start],
        vec![EffectTransition::Admit, EffectTransition::Commit],
    ];
    for prefix in illegal_prefixes {
        let mut machine = effect_machine("call-invalid");
        for transition in prefix.iter().take(prefix.len().saturating_sub(1)) {
            machine.apply(*transition).unwrap();
        }
        let before = machine.state();
        assert!(machine.apply(*prefix.last().unwrap()).is_err());
        assert_eq!(machine.state(), before);
    }

    let mut terminal = effect_machine("call-terminal");
    terminal.apply(EffectTransition::Admit).unwrap();
    terminal.apply(EffectTransition::Start).unwrap();
    terminal.apply(EffectTransition::Commit).unwrap();
    let before = terminal.state();
    for transition in [
        EffectTransition::Admit,
        EffectTransition::Start,
        EffectTransition::Commit,
        EffectTransition::Fail,
        EffectTransition::Cancel,
        EffectTransition::MarkOutcomeUnknown,
    ] {
        assert!(
            terminal.apply(transition).is_err(),
            "accepted {transition:?}"
        );
        assert_eq!(terminal.state(), before);
    }
}

// KCT-EFFECT-004 and KCT-EFFECT-006. Existing dispatch evidence remains exact.
#[test]
fn effectful_dispatch_records_admitted_started_and_committed_with_receipts() {
    let (runtime, _descriptor, effective, invocation) = effect_fixture();
    let mut journal = MemoryToolEffectJournal::default();
    let outcome = runtime
        .dispatch_with_journal(
            invocation,
            &effective,
            ToolDispatchControl::uncancellable(),
            &mut journal,
        )
        .unwrap();

    assert_eq!(
        journal
            .records()
            .iter()
            .map(|record| record.state())
            .collect::<Vec<_>>(),
        [
            ToolEffectLifecycleState::Admitted,
            ToolEffectLifecycleState::Started,
            ToolEffectLifecycleState::Committed,
        ]
    );
    assert_eq!(outcome.admitted_effect_receipt_refs.len(), 1);
    assert_eq!(outcome.observed_effect_receipt_refs.len(), 1);
    assert_eq!(
        journal.records()[2].observed_effects()[0].scope["path"],
        "README.md"
    );
}

// KCT-EFFECT-004. Mutation: map handler failure to success or omit the Failed transition.
#[test]
fn handler_failure_drives_a_failed_terminal_effect_transition() {
    let effect = EffectId::repo_files();
    let (runtime, _descriptor, effective, invocation) =
        effect_fixture_with_handler(ReturningEffectHandler {
            effect,
            result: Err(ToolHandlerError::execution_failed("injected failure")),
        });
    let mut journal = MemoryToolEffectJournal::default();
    assert!(matches!(
        runtime.dispatch_with_journal(
            invocation,
            &effective,
            ToolDispatchControl::uncancellable(),
            &mut journal,
        ),
        Err(ToolDispatchError::Handler(_))
    ));
    assert_eq!(
        journal
            .records()
            .iter()
            .map(|record| record.state())
            .collect::<Vec<_>>(),
        [
            ToolEffectLifecycleState::Admitted,
            ToolEffectLifecycleState::Started,
            ToolEffectLifecycleState::Failed,
        ]
    );
}

// KCT-CANCEL-001 and KCT-EFFECT-004. Mutation: discard a started effect on interruption.
#[test]
fn interruption_after_effect_start_records_outcome_unknown_before_returning() {
    let effect = EffectId::repo_files();
    let (runtime, _descriptor, effective, invocation) =
        effect_fixture_with_handler(PendingEffectHandler { effect });
    let mut journal = MemoryToolEffectJournal::default();
    let control =
        ToolDispatchControl::new(Arc::new(CancelOnSecondCheck(AtomicUsize::new(0))), None);
    assert!(matches!(
        runtime.dispatch_with_journal(invocation, &effective, control, &mut journal),
        Err(ToolDispatchError::OutcomeUnknown { .. })
    ));
    assert_eq!(
        journal
            .records()
            .iter()
            .map(|record| record.state())
            .collect::<Vec<_>>(),
        [
            ToolEffectLifecycleState::Admitted,
            ToolEffectLifecycleState::Started,
            ToolEffectLifecycleState::OutcomeUnknown,
        ]
    );
}

// KCT-EFFECT-004. Mutation: enter the handler after an already expired deadline.
#[test]
fn expired_deadline_after_admission_records_cancelled_before_handler_entry() {
    let (runtime, _descriptor, effective, invocation) = effect_fixture();
    let mut journal = MemoryToolEffectJournal::default();
    let expired = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap();
    let control = ToolDispatchControl::new(Arc::new(NeverCancelled), Some(expired));
    assert!(matches!(
        runtime.dispatch_with_journal(invocation, &effective, control, &mut journal),
        Err(ToolDispatchError::Cancelled { .. })
    ));
    assert_eq!(
        journal
            .records()
            .iter()
            .map(|record| record.state())
            .collect::<Vec<_>>(),
        [
            ToolEffectLifecycleState::Admitted,
            ToolEffectLifecycleState::Started,
            ToolEffectLifecycleState::Cancelled,
        ]
    );
}

// KCT-EFFECT-004. Mutation: treat an invalid observed receipt as committed evidence.
#[test]
fn invalid_receipt_after_effect_start_records_outcome_unknown() {
    let effect = EffectId::repo_files();
    let invalid = ToolResult::new(json!({"ok": true}))
        .with_observed_effects([ObservedEffect::new(effect.clone(), [])]);
    let (runtime, _descriptor, effective, invocation) =
        effect_fixture_with_handler(ReturningEffectHandler {
            effect,
            result: Ok(invalid),
        });
    let mut journal = MemoryToolEffectJournal::default();
    assert!(matches!(
        runtime.dispatch_with_journal(
            invocation,
            &effective,
            ToolDispatchControl::uncancellable(),
            &mut journal,
        ),
        Err(ToolDispatchError::OutcomeUnknown { .. })
    ));
    assert_eq!(
        journal.records().last().unwrap().state(),
        ToolEffectLifecycleState::OutcomeUnknown
    );
}

// KCT-EFFECT-006. Mutation: deduplicate observed receipts by EffectId alone.
#[test]
fn repeated_effect_id_preserves_distinct_observed_scopes_and_receipts() {
    let effect = EffectId::repo_files();
    let observed = ["first", "second"]
        .map(|path| ObservedEffect::new(effect.clone(), [("path".to_string(), path.to_string())]));
    let (runtime, _descriptor, effective, invocation) =
        effect_fixture_with_handler(ReturningEffectHandler {
            effect,
            result: Ok(ToolResult::new(json!({"ok": true})).with_observed_effects(observed)),
        });
    let mut journal = MemoryToolEffectJournal::default();
    let outcome = runtime
        .dispatch_with_journal(
            invocation,
            &effective,
            ToolDispatchControl::uncancellable(),
            &mut journal,
        )
        .unwrap();
    let terminal = journal.records().last().unwrap();
    assert_eq!(terminal.observed_effects().len(), 2);
    assert_eq!(outcome.observed_effect_receipt_refs.len(), 1);
    assert_eq!(terminal.observed_effects()[0].scope["path"], "first");
    assert_eq!(terminal.observed_effects()[1].scope["path"], "second");
}

struct RejectThirdAppend {
    appends: usize,
}

impl ToolEffectJournal for RejectThirdAppend {
    fn append(
        &mut self,
        _record: &ToolEffectJournalRecord,
    ) -> Result<String, ToolEffectJournalError> {
        self.appends += 1;
        if self.appends == 3 {
            Err(ToolEffectJournalError::new("terminal evidence lost"))
        } else {
            Ok(format!("effect:core-test:{}", self.appends))
        }
    }
}

// KCT-EFFECT-005. Mutation: report committed success after terminal journal loss.
#[test]
fn terminal_journal_loss_after_started_effect_is_outcome_unknown() {
    let (runtime, _descriptor, effective, invocation) = effect_fixture();
    let mut journal = RejectThirdAppend { appends: 0 };
    assert!(matches!(
        runtime.dispatch_with_journal(
            invocation,
            &effective,
            ToolDispatchControl::uncancellable(),
            &mut journal,
        ),
        Err(ToolDispatchError::OutcomeUnknown { .. })
    ));
    assert_eq!(journal.appends, 3);
}
