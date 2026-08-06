use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

use crate::{DeclarationDigest, EffectId, ExtensionId, ObservedEffect, ToolDelivery, ToolId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityClass {
    HostObservation,
    AgentDelegation,
    SessionMutation,
    ProcessSpawn,
    ProcessControl,
    HostExecution,
    ShellStartup,
    RepositoryMutation,
    RepositoryHooks,
    DurableStoreMutation,
    ExternalDelivery,
    PermissionMutation,
    TrustMutation,
}

impl AuthorityClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostObservation => "host_observation",
            Self::AgentDelegation => "agent_delegation",
            Self::SessionMutation => "session_mutation",
            Self::ProcessSpawn => "process_spawn",
            Self::ProcessControl => "process_control",
            Self::HostExecution => "host_execution",
            Self::ShellStartup => "shell_startup",
            Self::RepositoryMutation => "repository_mutation",
            Self::RepositoryHooks => "repository_hooks",
            Self::DurableStoreMutation => "durable_store_mutation",
            Self::ExternalDelivery => "external_delivery",
            Self::PermissionMutation => "permission_mutation",
            Self::TrustMutation => "trust_mutation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "host_observation" => Some(Self::HostObservation),
            "agent_delegation" => Some(Self::AgentDelegation),
            "session_mutation" => Some(Self::SessionMutation),
            "process_spawn" => Some(Self::ProcessSpawn),
            "process_control" => Some(Self::ProcessControl),
            "host_execution" => Some(Self::HostExecution),
            "shell_startup" => Some(Self::ShellStartup),
            "repository_mutation" => Some(Self::RepositoryMutation),
            "repository_hooks" => Some(Self::RepositoryHooks),
            "durable_store_mutation" => Some(Self::DurableStoreMutation),
            "external_delivery" => Some(Self::ExternalDelivery),
            "permission_mutation" => Some(Self::PermissionMutation),
            "trust_mutation" => Some(Self::TrustMutation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffectLifecycleState {
    Admitted,
    Started,
    Committed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

impl ToolEffectLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Started => "started",
            Self::Committed => "committed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEffectJournalRecord {
    call_id: String,
    tool_id: ToolId,
    extension_id: ExtensionId,
    schema_digest: DeclarationDigest,
    delivery: ToolDelivery,
    state: ToolEffectLifecycleState,
    admitted_effects: BTreeSet<EffectId>,
    observed_effects: Vec<ObservedEffect>,
    outcome_code: Option<String>,
}

impl ToolEffectJournalRecord {
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
    pub fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }
    pub fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }
    pub fn schema_digest(&self) -> &DeclarationDigest {
        &self.schema_digest
    }
    pub fn delivery(&self) -> ToolDelivery {
        self.delivery
    }
    pub fn state(&self) -> ToolEffectLifecycleState {
        self.state
    }
    pub fn admitted_effects(&self) -> &BTreeSet<EffectId> {
        &self.admitted_effects
    }
    pub fn observed_effects(&self) -> &[ObservedEffect] {
        &self.observed_effects
    }
    pub fn outcome_code(&self) -> Option<&str> {
        self.outcome_code.as_deref()
    }
}

#[derive(Clone, Debug)]
pub struct ToolEffectMachine {
    call_id: String,
    tool_id: ToolId,
    extension_id: ExtensionId,
    schema_digest: DeclarationDigest,
    delivery: ToolDelivery,
    admitted_effects: BTreeSet<EffectId>,
    state: Option<ToolEffectLifecycleState>,
}

impl ToolEffectMachine {
    pub fn new(
        call_id: impl Into<String>,
        tool_id: ToolId,
        extension_id: ExtensionId,
        schema_digest: DeclarationDigest,
        delivery: ToolDelivery,
        admitted_effects: BTreeSet<EffectId>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_id,
            extension_id,
            schema_digest,
            delivery,
            admitted_effects,
            state: None,
        }
    }

    pub fn state(&self) -> Option<ToolEffectLifecycleState> {
        self.state
    }
    pub fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }
    pub fn admitted_effects(&self) -> &BTreeSet<EffectId> {
        &self.admitted_effects
    }

    pub fn apply(
        &mut self,
        next: ToolEffectLifecycleState,
        observed_effects: Vec<ObservedEffect>,
        outcome_code: Option<String>,
    ) -> Result<ToolEffectJournalRecord, ToolEffectTransitionError> {
        let legal = matches!(
            (self.state, next),
            (None, ToolEffectLifecycleState::Admitted)
                | (
                    Some(ToolEffectLifecycleState::Admitted),
                    ToolEffectLifecycleState::Started
                )
                | (
                    Some(ToolEffectLifecycleState::Started),
                    ToolEffectLifecycleState::Committed
                )
                | (
                    Some(ToolEffectLifecycleState::Started),
                    ToolEffectLifecycleState::Failed
                )
                | (
                    Some(ToolEffectLifecycleState::Started),
                    ToolEffectLifecycleState::Cancelled
                )
                | (
                    Some(ToolEffectLifecycleState::Started),
                    ToolEffectLifecycleState::OutcomeUnknown
                )
        );
        if !legal {
            return Err(ToolEffectTransitionError {
                from: self.state,
                to: next,
            });
        }
        self.state = Some(next);
        Ok(ToolEffectJournalRecord {
            call_id: self.call_id.clone(),
            tool_id: self.tool_id.clone(),
            extension_id: self.extension_id.clone(),
            schema_digest: self.schema_digest.clone(),
            delivery: self.delivery,
            state: next,
            admitted_effects: self.admitted_effects.clone(),
            observed_effects,
            outcome_code,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolEffectTransitionError {
    from: Option<ToolEffectLifecycleState>,
    to: ToolEffectLifecycleState,
}

impl Display for ToolEffectTransitionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "illegal tool effect transition from {:?} to {}",
            self.from,
            self.to.as_str()
        )
    }
}

impl std::error::Error for ToolEffectTransitionError {}

pub trait ToolEffectJournal {
    fn append(
        &mut self,
        record: &ToolEffectJournalRecord,
    ) -> Result<String, ToolEffectJournalError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolEffectJournalError {
    message: String,
}

impl ToolEffectJournalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolEffectJournalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolEffectJournalError {}

#[derive(Default)]
pub struct MemoryToolEffectJournal {
    records: Vec<ToolEffectJournalRecord>,
}

impl MemoryToolEffectJournal {
    pub fn records(&self) -> &[ToolEffectJournalRecord] {
        &self.records
    }
}

impl ToolEffectJournal for MemoryToolEffectJournal {
    fn append(
        &mut self,
        record: &ToolEffectJournalRecord,
    ) -> Result<String, ToolEffectJournalError> {
        let reference = format!("effect:{}:{}", record.call_id(), self.records.len() + 1);
        self.records.push(record.clone());
        Ok(reference)
    }
}
