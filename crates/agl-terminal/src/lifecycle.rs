use agl_exec::{
    AuthorityFingerprint, CallerOwner, CallerOwnerKind, CallerRole, ExecutionId, ExecutionProfile,
    OpaqueOwnerId, ServiceGenerationId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::TerminalId;

/// Policy-neutral terminal lifecycle owner. Promotion replaces the active
/// caller while retaining the immediately previous opaque owner for fencing;
/// neither value is parsed by the terminal domain.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalOwner {
    caller: CallerOwner,
    previous_owner: Option<CallerOwner>,
}

impl TerminalOwner {
    pub fn new(caller: CallerOwner) -> Self {
        Self {
            caller,
            previous_owner: None,
        }
    }

    pub fn promoted(caller: CallerOwner, previous_owner: CallerOwner) -> Self {
        Self {
            caller,
            previous_owner: Some(previous_owner),
        }
    }

    pub fn caller(&self) -> &CallerOwner {
        &self.caller
    }

    pub fn previous_owner(&self) -> Option<&CallerOwner> {
        self.previous_owner.as_ref()
    }

    pub fn is_human(&self) -> bool {
        self.caller.role() == CallerRole::Human
    }

    pub fn is_agent(&self) -> bool {
        self.caller.role() == CallerRole::Agent
    }

    pub fn accepts_human_control(&self) -> bool {
        self.is_human() || self.previous_owner.is_some()
    }

    pub fn is_persistent(&self) -> bool {
        self.caller.owner_kind() == CallerOwnerKind::Persistent
    }

    pub fn is_ephemeral(&self) -> bool {
        self.caller.owner_kind() == CallerOwnerKind::Ephemeral
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TerminalTopologyId(OpaqueOwnerId);

impl TerminalTopologyId {
    pub fn new(value: OpaqueOwnerId) -> Self {
        Self(value)
    }

    pub fn as_opaque(&self) -> &OpaqueOwnerId {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Reserved,
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
    OutcomeUnknown,
}

impl TerminalState {
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Reserved | Self::Starting | Self::Running | Self::Stopping
        )
    }

    pub const fn is_final(self) -> bool {
        matches!(self, Self::Exited | Self::Failed | Self::OutcomeUnknown)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOperation {
    Inspect,
    Attach,
    Read,
    Write,
    Resize,
    Interrupt,
    Terminate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalDescriptor {
    pub terminal_id: TerminalId,
    pub execution_id: ExecutionId,
    pub owner: CallerOwner,
    pub authority_fingerprint: AuthorityFingerprint,
    pub profile: ExecutionProfile,
    pub service_generation: ServiceGenerationId,
    pub state: TerminalState,
    pub command_sequence: u64,
    pub output_sequence: u64,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TerminalDomainError {
    #[error("terminal lifecycle cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: TerminalState,
        to: TerminalState,
    },
    #[error("terminal command sequence cannot move backwards")]
    CommandSequenceRegression,
    #[error("terminal output sequence cannot move backwards")]
    OutputSequenceRegression,
    #[error("a final terminal descriptor cannot be reported as live")]
    FinalStateReportedLive,
}

pub fn validate_terminal_transition(
    previous: &TerminalDescriptor,
    next: &TerminalDescriptor,
) -> Result<(), TerminalDomainError> {
    if previous.terminal_id != next.terminal_id
        || previous.execution_id != next.execution_id
        || previous.owner != next.owner
        || previous.authority_fingerprint != next.authority_fingerprint
        || previous.profile != next.profile
        || previous.service_generation != next.service_generation
    {
        return Err(TerminalDomainError::InvalidTransition {
            from: previous.state,
            to: next.state,
        });
    }
    if next.command_sequence < previous.command_sequence {
        return Err(TerminalDomainError::CommandSequenceRegression);
    }
    if next.output_sequence < previous.output_sequence {
        return Err(TerminalDomainError::OutputSequenceRegression);
    }
    let valid = previous.state == next.state
        || matches!(
            (previous.state, next.state),
            (TerminalState::Reserved, TerminalState::Starting)
                | (TerminalState::Reserved, TerminalState::Failed)
                | (TerminalState::Reserved, TerminalState::OutcomeUnknown)
                | (TerminalState::Starting, TerminalState::Running)
                | (TerminalState::Starting, TerminalState::Failed)
                | (TerminalState::Starting, TerminalState::OutcomeUnknown)
                | (TerminalState::Running, TerminalState::Stopping)
                | (TerminalState::Running, TerminalState::Exited)
                | (TerminalState::Running, TerminalState::Failed)
                | (TerminalState::Running, TerminalState::OutcomeUnknown)
                | (TerminalState::Stopping, TerminalState::Exited)
                | (TerminalState::Stopping, TerminalState::Failed)
                | (TerminalState::Stopping, TerminalState::OutcomeUnknown)
        );
    if valid {
        Ok(())
    } else {
        Err(TerminalDomainError::InvalidTransition {
            from: previous.state,
            to: next.state,
        })
    }
}

#[cfg(test)]
mod tests {
    use agl_exec::{
        CallerNamespace, CallerOwnerKind, CallerRole, OpaqueOwnerId, ServiceGenerationId,
    };

    use super::*;

    fn descriptor(state: TerminalState) -> TerminalDescriptor {
        TerminalDescriptor {
            terminal_id: TerminalId::generate(),
            execution_id: ExecutionId::generate(),
            owner: CallerOwner::new(
                CallerNamespace::new("agentlibre", 1).unwrap(),
                OpaqueOwnerId::new("opaque-owner").unwrap(),
                CallerOwnerKind::Persistent,
                CallerRole::Human,
            ),
            authority_fingerprint: AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
            profile: ExecutionProfile::Workspace,
            service_generation: ServiceGenerationId::generate(),
            state,
            command_sequence: 0,
            output_sequence: 0,
        }
    }

    #[test]
    fn lifecycle_is_forward_only_and_identity_fenced() {
        let reserved = descriptor(TerminalState::Reserved);
        let mut starting = reserved.clone();
        starting.state = TerminalState::Starting;
        assert_eq!(validate_terminal_transition(&reserved, &starting), Ok(()));

        let mut stale = starting.clone();
        stale.state = TerminalState::Reserved;
        assert!(matches!(
            validate_terminal_transition(&starting, &stale),
            Err(TerminalDomainError::InvalidTransition { .. })
        ));

        let mut foreign = starting.clone();
        foreign.service_generation = ServiceGenerationId::generate();
        assert!(validate_terminal_transition(&starting, &foreign).is_err());
    }

    #[test]
    fn sequence_regressions_fail_closed() {
        let mut previous = descriptor(TerminalState::Running);
        previous.command_sequence = 2;
        previous.output_sequence = 7;
        let mut next = previous.clone();
        next.command_sequence = 1;
        assert_eq!(
            validate_terminal_transition(&previous, &next),
            Err(TerminalDomainError::CommandSequenceRegression)
        );
    }
}
