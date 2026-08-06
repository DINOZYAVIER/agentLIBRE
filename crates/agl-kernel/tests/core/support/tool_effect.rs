use std::collections::BTreeSet;

use agl_kernel::ToolEffectLifecycleState;
use agl_kernel::{DeclarationDigest, EffectId, ExtensionId, ToolDelivery, ToolId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectTransition {
    Admit,
    Start,
    Commit,
    Fail,
    Cancel,
    MarkOutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectRecordView {
    pub state: ToolEffectLifecycleState,
    pub call_id: String,
    pub tool_id: ToolId,
    pub extension_id: ExtensionId,
    pub schema_digest: DeclarationDigest,
    pub delivery: ToolDelivery,
    pub admitted_effects: BTreeSet<EffectId>,
}

/// Test-only wiring point. AGL-170 implementation replaces this zero-sized
/// placeholder with calls to the production ToolEffectMachine. It must not
/// calculate transitions or synthesize records in the test adapter.
pub struct ProductionToolEffectMachine {
    machine: agl_kernel::ToolEffectMachine,
}

impl ProductionToolEffectMachine {
    pub fn new(
        call_id: &str,
        tool_id: ToolId,
        extension_id: ExtensionId,
        schema_digest: DeclarationDigest,
        delivery: ToolDelivery,
        admitted_effects: BTreeSet<EffectId>,
    ) -> Self {
        Self {
            machine: agl_kernel::ToolEffectMachine::new(
                call_id,
                tool_id,
                extension_id,
                schema_digest,
                delivery,
                admitted_effects,
            ),
        }
    }

    pub fn state(&self) -> Option<ToolEffectLifecycleState> {
        self.machine.state()
    }

    pub fn apply(&mut self, transition: EffectTransition) -> Result<EffectRecordView, String> {
        let state = match transition {
            EffectTransition::Admit => ToolEffectLifecycleState::Admitted,
            EffectTransition::Start => ToolEffectLifecycleState::Started,
            EffectTransition::Commit => ToolEffectLifecycleState::Committed,
            EffectTransition::Fail => ToolEffectLifecycleState::Failed,
            EffectTransition::Cancel => ToolEffectLifecycleState::Cancelled,
            EffectTransition::MarkOutcomeUnknown => ToolEffectLifecycleState::OutcomeUnknown,
        };
        let record = self
            .machine
            .apply(state, Vec::new(), None)
            .map_err(|error| error.to_string())?;
        Ok(EffectRecordView {
            state: record.state(),
            call_id: record.call_id().to_string(),
            tool_id: record.tool_id().clone(),
            extension_id: record.extension_id().clone(),
            schema_digest: record.schema_digest().clone(),
            delivery: record.delivery(),
            admitted_effects: record.admitted_effects().clone(),
        })
    }
}
