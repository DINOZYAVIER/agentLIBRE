use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

use agl_extension::{
    DeclarationDigest, EffectId, ExtensionId, ObservedEffect, ToolDelivery, ToolId,
};
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
    pub call_id: String,
    pub tool_id: ToolId,
    pub extension_id: ExtensionId,
    pub schema_digest: DeclarationDigest,
    pub delivery: ToolDelivery,
    pub state: ToolEffectLifecycleState,
    pub admitted_effects: BTreeSet<EffectId>,
    pub observed_effects: Vec<ObservedEffect>,
    pub outcome_code: Option<String>,
}

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
        let reference = format!("effect:{}:{}", record.call_id, self.records.len() + 1);
        self.records.push(record.clone());
        Ok(reference)
    }
}
