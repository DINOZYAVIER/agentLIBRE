use std::collections::BTreeSet;

use agl_kernel::{
    ArtifactId, ToolEffectCorrelation, ToolEffectLifecycleState, ToolEffectRecoveryJournal,
};
use serde::{Deserialize, Serialize};

use crate::{ArtifactHandleError, ArtifactPath};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactChangeKind {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactChange {
    pub path: ArtifactPath,
    pub kind: ArtifactChangeKind,
}

impl ArtifactChange {
    pub fn new(
        path: impl Into<String>,
        kind: ArtifactChangeKind,
    ) -> Result<Self, ArtifactHandleError> {
        Ok(Self {
            path: ArtifactPath::new(path)?,
            kind,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactCommitEntryKind {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactCommitEntry {
    path: ArtifactPath,
    kind: ArtifactCommitEntryKind,
}

impl ArtifactCommitEntry {
    pub fn new(
        path: impl Into<String>,
        kind: ArtifactCommitEntryKind,
    ) -> Result<Self, ArtifactHandleError> {
        Ok(Self {
            path: ArtifactPath::new(path)?,
            kind,
        })
    }

    pub fn path(&self) -> &ArtifactPath {
        &self.path
    }

    pub fn kind(&self) -> ArtifactCommitEntryKind {
        self.kind
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactCommitRequest {
    operation_id: String,
    correlation: ToolEffectCorrelation,
    artifact_id: ArtifactId,
    entries: Vec<ArtifactCommitEntry>,
    message: String,
}

impl ArtifactCommitRequest {
    pub fn new(
        operation_id: impl Into<String>,
        correlation: ToolEffectCorrelation,
        artifact_id: ArtifactId,
        entries: impl IntoIterator<Item = ArtifactCommitEntry>,
        message: impl Into<String>,
    ) -> Result<Self, ArtifactCommitError> {
        let operation_id = operation_id.into();
        if operation_id.trim().is_empty() {
            return Err(ArtifactCommitError::InvalidRequest(
                "operation identity is blank".to_owned(),
            ));
        }
        let entries = entries.into_iter().collect::<Vec<_>>();
        let message = message.into();
        if entries.is_empty() {
            return Err(ArtifactCommitError::InvalidRequest(
                "entry list is empty".to_owned(),
            ));
        }
        if message.trim().is_empty() {
            return Err(ArtifactCommitError::InvalidRequest(
                "commit message is blank".to_owned(),
            ));
        }
        let mut paths = BTreeSet::new();
        for entry in &entries {
            if !paths.insert(entry.path.clone()) {
                return Err(ArtifactCommitError::InvalidRequest(format!(
                    "duplicate entry `{}`",
                    entry.path
                )));
            }
        }
        Ok(Self {
            operation_id,
            correlation,
            artifact_id,
            entries,
            message,
        })
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn correlation(&self) -> &ToolEffectCorrelation {
        &self.correlation
    }

    pub fn artifact_id(&self) -> &ArtifactId {
        &self.artifact_id
    }

    pub fn entries(&self) -> &[ArtifactCommitEntry] {
        &self.entries
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitCommitMaterial {
    pub parent: String,
    pub tree: String,
    pub commit: String,
    pub message: String,
    pub author: String,
    pub committer: String,
}

impl GitCommitMaterial {
    pub fn fixture(
        parent: impl Into<String>,
        tree: impl Into<String>,
        commit: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            parent: parent.into(),
            tree: tree.into(),
            commit: commit.into(),
            message: message.into(),
            author: "AGL Fixture <agl-fixture@example.invalid> 0 +0000".to_owned(),
            committer: "AGL Fixture <agl-fixture@example.invalid> 0 +0000".to_owned(),
        }
    }

    pub fn exact(
        parent: impl Into<String>,
        tree: impl Into<String>,
        commit: impl Into<String>,
        message: impl Into<String>,
        author: impl Into<String>,
        committer: impl Into<String>,
    ) -> Self {
        Self {
            parent: parent.into(),
            tree: tree.into(),
            commit: commit.into(),
            message: message.into(),
            author: author.into(),
            committer: committer.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCommitPrepare {
    pub operation_id: String,
    pub artifact_id: ArtifactId,
    pub correlation: ToolEffectCorrelation,
    pub parent_head: String,
    pub parent_gitlink: String,
    pub child_head: String,
    pub changes: Vec<ArtifactChange>,
    pub child_commit: GitCommitMaterial,
    pub parent_author: String,
    pub parent_committer: String,
    pub parent_message: String,
}

impl ArtifactCommitPrepare {
    pub fn builder(
        operation_id: impl Into<String>,
        artifact_id: ArtifactId,
        correlation: ToolEffectCorrelation,
    ) -> ArtifactCommitPrepareBuilder {
        ArtifactCommitPrepareBuilder {
            operation_id: operation_id.into(),
            artifact_id,
            correlation,
            parent_head: None,
            parent_gitlink: None,
            child_head: None,
            changes: Vec::new(),
            child_commit: None,
            parent_author: None,
            parent_committer: None,
            parent_message: None,
        }
    }

    pub fn correlation(&self) -> &ToolEffectCorrelation {
        &self.correlation
    }
}

pub struct ArtifactCommitPrepareBuilder {
    operation_id: String,
    artifact_id: ArtifactId,
    correlation: ToolEffectCorrelation,
    parent_head: Option<String>,
    parent_gitlink: Option<String>,
    child_head: Option<String>,
    changes: Vec<ArtifactChange>,
    child_commit: Option<GitCommitMaterial>,
    parent_author: Option<String>,
    parent_committer: Option<String>,
    parent_message: Option<String>,
}

impl ArtifactCommitPrepareBuilder {
    pub fn parent_head(mut self, value: impl Into<String>) -> Self {
        self.parent_head = Some(value.into());
        self
    }
    pub fn parent_gitlink(mut self, value: impl Into<String>) -> Self {
        self.parent_gitlink = Some(value.into());
        self
    }
    pub fn child_head(mut self, value: impl Into<String>) -> Self {
        self.child_head = Some(value.into());
        self
    }
    pub fn changes(mut self, value: impl IntoIterator<Item = ArtifactChange>) -> Self {
        self.changes = value.into_iter().collect();
        self
    }
    pub fn child_commit(mut self, value: GitCommitMaterial) -> Self {
        self.child_commit = Some(value);
        self
    }
    pub fn parent_identity(
        mut self,
        author: impl Into<String>,
        committer: impl Into<String>,
    ) -> Self {
        self.parent_author = Some(author.into());
        self.parent_committer = Some(committer.into());
        self
    }
    pub fn parent_message(mut self, value: impl Into<String>) -> Self {
        self.parent_message = Some(value.into());
        self
    }
    pub fn build(self) -> Result<ArtifactCommitPrepare, ArtifactCommitError> {
        if self.operation_id.trim().is_empty() || self.changes.is_empty() {
            return Err(ArtifactCommitError::InvalidRequest(
                "prepare identity and changes are required".to_owned(),
            ));
        }
        Ok(ArtifactCommitPrepare {
            operation_id: self.operation_id,
            artifact_id: self.artifact_id,
            correlation: self.correlation,
            parent_head: self.parent_head.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("parent_head is required".to_owned())
            })?,
            parent_gitlink: self.parent_gitlink.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("parent_gitlink is required".to_owned())
            })?,
            child_head: self.child_head.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("child_head is required".to_owned())
            })?,
            changes: self.changes,
            child_commit: self.child_commit.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("child_commit is required".to_owned())
            })?,
            parent_author: self.parent_author.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("parent author is required".to_owned())
            })?,
            parent_committer: self.parent_committer.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("parent committer is required".to_owned())
            })?,
            parent_message: self.parent_message.ok_or_else(|| {
                ArtifactCommitError::InvalidRequest("parent message is required".to_owned())
            })?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactCommitInput {
    Prepare(Box<ArtifactCommitPrepare>),
    RecordChildCommit {
        observed_commit: String,
        parent_commit: GitCommitMaterial,
    },
    RecordParentCommit {
        observed_commit: String,
    },
    ConfirmDurableEvidence,
    AbortBeforeMutation {
        reason: String,
    },
    ObserveUnexpectedChild {
        observed_commit: String,
    },
    ObserveUnsafeParent {
        observed_head: String,
        observed_gitlink: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactCommitState {
    Prepared {
        prepare: ArtifactCommitPrepare,
    },
    ChildCommitted {
        prepare: ArtifactCommitPrepare,
        child_commit: String,
        parent_commit: GitCommitMaterial,
    },
    ParentCommitted {
        child_commit: String,
        parent_commit: String,
    },
    Committed {
        child_commit: String,
        parent_commit: String,
    },
    Failed {
        reason: String,
    },
    Conflict {
        child_commit: String,
        observed_head: String,
        observed_gitlink: String,
    },
}

impl ArtifactCommitState {
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Prepared { .. } => "prepared",
            Self::ChildCommitted { .. } => "child_committed",
            Self::ParentCommitted { .. } => "parent_committed",
            Self::Committed { .. } => "committed",
            Self::Failed { .. } => "failed",
            Self::Conflict { .. } => "conflict",
        }
    }

    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ArtifactCommitMachine {
    state: Option<ArtifactCommitState>,
    revision: u64,
    accepted: Vec<(ArtifactCommitInput, ArtifactCommitState)>,
}

impl ArtifactCommitMachine {
    pub fn from_record(record: &ArtifactCommitRecord) -> Self {
        Self {
            state: Some(record.state.clone()),
            revision: record.revision,
            accepted: Vec::new(),
        }
    }
    pub fn state(&self) -> Option<&ArtifactCommitState> {
        self.state.as_ref()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn apply(
        &mut self,
        input: ArtifactCommitInput,
    ) -> Result<ArtifactCommitState, ArtifactCommitError> {
        if let Some((_, state)) = self.accepted.iter().find(|(seen, _)| seen == &input) {
            return Ok(state.clone());
        }
        let next = match (&self.state, &input) {
            (None, ArtifactCommitInput::Prepare(prepare)) => ArtifactCommitState::Prepared {
                prepare: prepare.as_ref().clone(),
            },
            (
                Some(ArtifactCommitState::Prepared { prepare }),
                ArtifactCommitInput::RecordChildCommit {
                    observed_commit,
                    parent_commit,
                },
            ) if observed_commit == &prepare.child_commit.commit => {
                ArtifactCommitState::ChildCommitted {
                    prepare: prepare.clone(),
                    child_commit: observed_commit.clone(),
                    parent_commit: parent_commit.clone(),
                }
            }
            (
                Some(ArtifactCommitState::Prepared { prepare }),
                ArtifactCommitInput::AbortBeforeMutation { reason },
            ) if !reason.trim().is_empty() => ArtifactCommitState::Failed {
                reason: reason.clone(),
            },
            (
                Some(ArtifactCommitState::Prepared { prepare }),
                ArtifactCommitInput::ObserveUnexpectedChild { observed_commit },
            ) => ArtifactCommitState::Conflict {
                child_commit: prepare.child_commit.commit.clone(),
                observed_head: observed_commit.clone(),
                observed_gitlink: prepare.parent_gitlink.clone(),
            },
            (
                Some(ArtifactCommitState::ChildCommitted {
                    child_commit,
                    parent_commit,
                    ..
                }),
                ArtifactCommitInput::RecordParentCommit { observed_commit },
            ) if observed_commit == &parent_commit.commit => ArtifactCommitState::ParentCommitted {
                child_commit: child_commit.clone(),
                parent_commit: observed_commit.clone(),
            },
            (
                Some(ArtifactCommitState::ChildCommitted { child_commit, .. }),
                ArtifactCommitInput::ObserveUnsafeParent {
                    observed_head,
                    observed_gitlink,
                },
            ) => ArtifactCommitState::Conflict {
                child_commit: child_commit.clone(),
                observed_head: observed_head.clone(),
                observed_gitlink: observed_gitlink.clone(),
            },
            (
                Some(ArtifactCommitState::ParentCommitted {
                    child_commit,
                    parent_commit,
                }),
                ArtifactCommitInput::ConfirmDurableEvidence,
            ) => ArtifactCommitState::Committed {
                child_commit: child_commit.clone(),
                parent_commit: parent_commit.clone(),
            },
            (
                Some(ArtifactCommitState::Failed { .. } | ArtifactCommitState::Conflict { .. }),
                _,
            ) => return Err(ArtifactCommitError::Terminal),
            (
                Some(ArtifactCommitState::Prepared { .. }),
                ArtifactCommitInput::RecordChildCommit { .. },
            ) => return Err(ArtifactCommitError::IdentityConflict),
            _ => return Err(ArtifactCommitError::IllegalTransition),
        };
        self.state = Some(next.clone());
        self.revision += 1;
        self.accepted.push((input, next.clone()));
        Ok(next)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArtifactCommitError {
    #[error("invalid Artifact commit request: {0}")]
    InvalidRequest(String),
    #[error("Artifact commit identity conflicts with prepared material")]
    IdentityConflict,
    #[error("illegal Artifact commit transition")]
    IllegalTransition,
    #[error("Artifact commit operation is terminal")]
    Terminal,
    #[error("Artifact commit repository failure: {0}")]
    Repository(String),
}

impl ArtifactCommitError {
    pub fn is_identity_conflict(&self) -> bool {
        matches!(self, Self::IdentityConflict)
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCommitRecord {
    operation_id: String,
    correlation: ToolEffectCorrelation,
    prepare: ArtifactCommitPrepare,
    revision: u64,
    state: ArtifactCommitState,
}

impl ArtifactCommitRecord {
    pub fn new(
        operation_id: impl Into<String>,
        correlation: ToolEffectCorrelation,
        prepare: ArtifactCommitPrepare,
        revision: u64,
        state: ArtifactCommitState,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            correlation,
            prepare,
            revision,
            state,
        }
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn state(&self) -> &ArtifactCommitState {
        &self.state
    }
    pub fn state_name(&self) -> &'static str {
        self.state.state_name()
    }
    pub fn correlation(&self) -> &ToolEffectCorrelation {
        &self.correlation
    }
    pub fn prepare(&self) -> &ArtifactCommitPrepare {
        &self.prepare
    }
}

pub trait ArtifactCommitRepository {
    fn save(&self, record: ArtifactCommitRecord) -> Result<(), ArtifactCommitError>;
    fn load(&self, operation_id: &str) -> Result<ArtifactCommitRecord, ArtifactCommitError>;
    fn incomplete(&self) -> Result<Vec<ArtifactCommitRecord>, ArtifactCommitError>;
}

#[derive(Default)]
pub struct MemoryArtifactCommitRepository {
    records: std::sync::RwLock<std::collections::BTreeMap<String, ArtifactCommitRecord>>,
}

impl ArtifactCommitRepository for MemoryArtifactCommitRepository {
    fn save(&self, record: ArtifactCommitRecord) -> Result<(), ArtifactCommitError> {
        let mut records = self.records.write().map_err(|_| {
            ArtifactCommitError::Repository("commit repository lock poisoned".to_owned())
        })?;
        if let Some(existing) = records.get(record.operation_id()) {
            if existing.correlation != record.correlation || existing.prepare != record.prepare {
                return Err(ArtifactCommitError::IdentityConflict);
            }
            if existing.revision > record.revision {
                return Err(ArtifactCommitError::Repository(
                    "artifact commit revision cannot move backwards".to_owned(),
                ));
            }
            if existing.revision == record.revision {
                return if existing == &record {
                    Ok(())
                } else {
                    Err(ArtifactCommitError::IdentityConflict)
                };
            }
            if existing.state.is_committed()
                || existing.state.is_conflict()
                || matches!(existing.state, ArtifactCommitState::Failed { .. })
            {
                return Err(ArtifactCommitError::Terminal);
            }
        }
        records.insert(record.operation_id.clone(), record);
        Ok(())
    }
    fn load(&self, operation_id: &str) -> Result<ArtifactCommitRecord, ArtifactCommitError> {
        self.records
            .read()
            .map_err(|_| {
                ArtifactCommitError::Repository("commit repository lock poisoned".to_owned())
            })?
            .get(operation_id)
            .cloned()
            .ok_or_else(|| {
                ArtifactCommitError::Repository(format!("operation `{operation_id}` not found"))
            })
    }
    fn incomplete(&self) -> Result<Vec<ArtifactCommitRecord>, ArtifactCommitError> {
        Ok(self
            .records
            .read()
            .map_err(|_| {
                ArtifactCommitError::Repository("commit repository lock poisoned".to_owned())
            })?
            .values()
            .filter(|record| {
                !matches!(
                    record.state,
                    ArtifactCommitState::Committed { .. }
                        | ArtifactCommitState::Failed { .. }
                        | ArtifactCommitState::Conflict { .. }
                )
            })
            .cloned()
            .collect())
    }
}

impl MemoryArtifactCommitRepository {
    pub fn load(&self, operation_id: &str) -> Result<ArtifactCommitRecord, ArtifactCommitError> {
        ArtifactCommitRepository::load(self, operation_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactCommitEvidence {
    operation_id: String,
    child_commit: String,
    parent_commit: String,
    state: ArtifactCommitState,
}

impl ArtifactCommitEvidence {
    pub fn new(
        operation_id: impl Into<String>,
        child_commit: impl Into<String>,
        parent_commit: impl Into<String>,
        state: ArtifactCommitState,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            child_commit: child_commit.into(),
            parent_commit: parent_commit.into(),
            state,
        }
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn child_commit(&self) -> &str {
        &self.child_commit
    }
    pub fn parent_commit(&self) -> &str {
        &self.parent_commit
    }
    pub fn is_committed(&self) -> bool {
        self.state.is_committed()
    }
    pub fn is_conflict(&self) -> bool {
        self.state.is_conflict()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolEffectReconciliation {
    tool_effect_state: ToolEffectLifecycleState,
    artifact_state: ArtifactCommitState,
    evidence_order: Vec<&'static str>,
}

impl ToolEffectReconciliation {
    pub fn tool_effect_state(&self) -> ToolEffectLifecycleState {
        self.tool_effect_state
    }
    pub fn artifact_state(&self) -> &ArtifactCommitState {
        &self.artifact_state
    }
    pub fn evidence_order(&self) -> &[&'static str] {
        &self.evidence_order
    }
}

pub fn reconcile_tool_effect(
    journal: &mut impl ToolEffectRecoveryJournal,
    correlation: &ToolEffectCorrelation,
    artifact_state: ArtifactCommitState,
) -> Result<ToolEffectReconciliation, ArtifactCommitError> {
    let records = journal.correlated_records(correlation);
    let current = records.last().map(|record| record.state()).ok_or_else(|| {
        ArtifactCommitError::Repository("correlated Tool Effect was not found".to_owned())
    })?;
    let terminal = matches!(
        current,
        ToolEffectLifecycleState::Committed
            | ToolEffectLifecycleState::Failed
            | ToolEffectLifecycleState::Cancelled
            | ToolEffectLifecycleState::OutcomeUnknown
    );
    let mut evidence_order = vec!["artifact.committed"];
    let tool_effect_state = if terminal {
        current
    } else if artifact_state.is_committed() {
        journal
            .append_recovery_terminal(correlation, ToolEffectLifecycleState::Committed)
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?;
        evidence_order.extend(["tool_effect.committed", "tool.success"]);
        ToolEffectLifecycleState::Committed
    } else {
        current
    };
    Ok(ToolEffectReconciliation {
        tool_effect_state,
        artifact_state,
        evidence_order,
    })
}
