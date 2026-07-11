use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ActionDeclaration, ActionVisibility, CapabilityId, OperationKind, ProviderDeclaration,
    ProviderId, StateEffect,
};

pub const AGENT_DELEGATE_CAPABILITY_ID: &str = "agent.delegate";
pub const AGENT_DELEGATE_PROVIDER_ID: &str = "agent-supervisor";
pub const MAX_DELEGATED_TASK_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegateActionArgs {
    pub subagent_id: String,
    pub task: String,
}

impl DelegateActionArgs {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.subagent_id.trim().is_empty() || self.subagent_id.trim() != self.subagent_id {
            return Err("subagent_id must be nonblank without surrounding whitespace");
        }
        if self.task.trim().is_empty() {
            return Err("delegated task must be nonblank");
        }
        if self.task.len() > MAX_DELEGATED_TASK_BYTES {
            return Err("delegated task exceeds the byte limit");
        }
        Ok(())
    }
}

pub fn delegation_provider() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(AGENT_DELEGATE_PROVIDER_ID)
            .expect("builtin delegation provider id is valid"),
        "Agent Supervisor",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin delegation provider declaration is valid")
    .with_action(
        ActionDeclaration::from_schema::<DelegateActionArgs>(
            CapabilityId::new(AGENT_DELEGATE_CAPABILITY_ID)
                .expect("builtin delegation capability id is valid"),
            "Delegate one bounded task to a declared supervised subagent.",
            OperationKind::Execute,
        )
        .expect("builtin delegation action declaration is valid")
        .with_state_effects([StateEffect::SpawnSubagent])
        .with_run_step_idempotency()
        .with_visibility(ActionVisibility {
            visible_in_read_only: true,
        }),
    )
}
