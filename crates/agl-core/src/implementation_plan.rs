//! Strict, canonical handoff documents between planning and implementation.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use uuid::{Uuid, Version};

use crate::ParseIdError;

pub const IMPLEMENTATION_PLAN_SCHEMA: &str = "agentlibre.implementation-plan/v1";
pub const MAX_PLAN_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_COLLECTION_ITEMS: usize = 256;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_PATH_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid implementation plan: {0}")]
pub struct PlanValidationError(pub String);

#[derive(Debug, Error)]
pub enum PlanError {
    #[error(transparent)]
    Validation(#[from] PlanValidationError),
    #[error("implementation plan JSON is not encodable: {0}")]
    Json(#[from] serde_json::Error),
    #[error("implementation plan JSON exceeds {MAX_PLAN_BYTES} bytes")]
    TooLarge,
    #[error("implementation plan JSON has unsupported nesting")]
    TooDeep,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanId(Uuid);

impl PlanId {
    pub fn new(value: Uuid) -> Result<Self, ParseIdError> {
        if value.get_version() != Some(Version::SortRand) {
            return Err(ParseIdError::UnsupportedUuidVersion);
        }
        Ok(Self(value))
    }
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, ParseIdError> {
        Self::new(Uuid::from_bytes(bytes))
    }

    pub fn parse(value: &str) -> Result<Self, ParseIdError> {
        crate::correlation_id::parse_uuid(value, "").map(Self)
    }
}

/// Durable lifecycle state for a generated implementation plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Draft,
    AwaitingDecisions,
    ReadyForApproval,
    Approved,
    Implementing,
    Completed,
    Failed,
    Stale,
}

impl PlanState {
    pub fn for_draft(plan: &ImplementationPlan) -> Self {
        if plan.open_decisions.is_empty() {
            Self::ReadyForApproval
        } else {
            Self::AwaitingDecisions
        }
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.hyphenated().to_string())
    }
}

impl FromStr for PlanId {
    type Err = ParseIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PlanId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PlanId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_str(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanDigest([u8; 32]);

impl PlanDigest {
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl fmt::Display for PlanDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", self.hex())
    }
}

impl FromStr for PlanDigest {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value
            .strip_prefix("sha256:")
            .ok_or("digest requires sha256 prefix")?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("digest requires 64 lowercase hexadecimal characters");
        }
        let mut bytes = [0; 32];
        for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)
                .map_err(|_| "invalid digest")?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for PlanDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PlanDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_str(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanText(String);

impl PlanText {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_TEXT_BYTES
            || value
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        {
            return Err(
                "text must be non-empty, bounded, and contain no control characters except newline and tab",
            );
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for PlanText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PlanText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlanItemId(String);

impl PlanItemId {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value.as_bytes()[0].is_ascii_lowercase()
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
        {
            return Err("ID must be a bounded lowercase identifier");
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for PlanItemId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PlanItemId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub type EvidenceId = PlanItemId;
pub type DecisionId = PlanItemId;
pub type InvariantId = PlanItemId;
pub type SliceId = PlanItemId;
pub type ReadRequirementId = PlanItemId;
pub type ConditionalReadId = PlanItemId;
pub type StepId = PlanItemId;
pub type CheckId = PlanItemId;
pub type OpenDecisionId = PlanItemId;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_PATH_BYTES || value.contains('\\') {
            return Err("workspace path must be non-empty, bounded, and use '/' separators");
        }
        if value != "." {
            let path = Path::new(&value);
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
                || value.split('/').any(|segment| segment.is_empty())
            {
                return Err("workspace path must be relative and contain no traversal");
            }
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for WorkspacePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationPlan {
    pub schema: PlanSchema,
    pub id: PlanId,
    pub workspace: WorkspaceEvidence,
    pub objective: Objective,
    pub evidence: Vec<Evidence>,
    pub decisions: Vec<Decision>,
    pub invariants: Vec<Invariant>,
    pub slices: Vec<ImplementationSlice>,
    pub open_decisions: Vec<OpenDecision>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PlanSchema {
    #[serde(rename = "agentlibre.implementation-plan/v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEvidence {
    pub canonical_root_sha256: PlanDigest,
    pub git_commit: Option<GitCommit>,
    pub dirty_paths: Vec<WorkspacePath>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommit(String);

impl Serialize for GitCommit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GitCommit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if (value.len() != 40 && value.len() != 64)
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(D::Error::custom(
                "git commit must be lowercase SHA-1 or SHA-256",
            ));
        }
        Ok(Self(value))
    }
}

impl GitCommit {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if (value.len() != 40 && value.len() != 64)
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("git commit must be lowercase SHA-1 or SHA-256");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Objective {
    pub outcome: PlanText,
    pub acceptance: Vec<Check>,
    pub sources: Vec<SourceRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub id: EvidenceId,
    pub statement: PlanText,
    pub sources: Vec<SourceRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub id: DecisionId,
    pub statement: PlanText,
    pub authority: DecisionAuthority,
    pub sources: Vec<SourceRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionAuthority {
    Repository,
    Human,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invariant {
    pub id: InvariantId,
    pub statement: PlanText,
    pub sources: Vec<SourceRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationSlice {
    pub id: SliceId,
    pub outcome: PlanText,
    pub depends_on: Vec<SliceId>,
    pub files: Vec<FileChange>,
    pub required_reads: Vec<ReadRequirement>,
    pub conditional_reads: Vec<ConditionalRead>,
    pub start_condition: Vec<PlanItemId>,
    pub steps: Vec<ImplementationStep>,
    pub verification: Vec<Verification>,
    pub done_when: Vec<Check>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: WorkspacePath,
    pub disposition: FileDisposition,
    pub purpose: PlanText,
    pub symbols: Vec<PlanText>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileDisposition {
    Create,
    Modify,
    Delete,
    Move,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequirement {
    pub id: ReadRequirementId,
    pub path: WorkspacePath,
    pub symbols_or_ranges: Vec<PlanText>,
    pub obtain: PlanText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalRead {
    pub id: ConditionalReadId,
    pub condition: PlanText,
    pub read: ReadRequirement,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationStep {
    pub id: StepId,
    pub action: PlanText,
    pub files: Vec<WorkspacePath>,
    pub symbols: Vec<PlanText>,
    pub depends_on: Vec<StepId>,
    pub satisfies: Vec<PlanItemId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub command: PlanText,
    pub working_directory: WorkspacePath,
    pub expected: VerificationExpected,
    pub covers: Vec<PlanItemId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationExpected {
    pub exit_code: i32,
    pub stdout_contains: Vec<PlanText>,
    pub stderr_contains: Vec<PlanText>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: CheckId,
    pub statement: PlanText,
    pub sources: Vec<SourceRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenDecision {
    pub id: OpenDecisionId,
    pub question: PlanText,
    pub consequences: PlanText,
    pub affected_slices: Vec<SliceId>,
    pub sources: Vec<SourceRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub kind: SourceKind,
    pub locator: PlanText,
    pub digest: Option<PlanDigest>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SliceState {
    Pending,
    Running,
    Completed,
    Failed,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFileDigest {
    pub path: WorkspacePath,
    pub digest: PlanDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub digest: PlanDigest,
    pub files: Vec<WorkspaceFileDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceChangedPath {
    pub path: WorkspacePath,
    pub before: Option<PlanDigest>,
    pub after: Option<PlanDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationResult {
    pub command: PlanText,
    pub passed: bool,
    pub evidence: PlanText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdditionalRead {
    pub path: WorkspacePath,
    pub justification: PlanText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceResult {
    pub schema: SliceResultSchema,
    pub plan_id: PlanId,
    pub plan_digest: PlanDigest,
    pub slice_id: SliceId,
    pub state: SliceState,
    pub changed_paths: Vec<SliceChangedPath>,
    pub verification: Vec<VerificationResult>,
    pub additional_reads: Vec<AdditionalRead>,
    pub conversation_id: Option<crate::ConversationId>,
    pub run_id: Option<crate::AgentRunId>,
    pub workspace: WorkspaceSnapshot,
    pub failure: Option<PlanText>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SliceResultSchema {
    #[serde(rename = "agentlibre.slice-result/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    File,
    Message,
    Decision,
    Test,
    Config,
}

impl ImplementationPlan {
    pub fn validate(&self) -> Result<(), PlanValidationError> {
        if self.schema != PlanSchema::V1 {
            return Err(PlanValidationError(
                "schema must be agentlibre.implementation-plan/v1".into(),
            ));
        }
        for (name, length) in [
            ("evidence", self.evidence.len()),
            ("decisions", self.decisions.len()),
            ("invariants", self.invariants.len()),
            ("slices", self.slices.len()),
            ("open_decisions", self.open_decisions.len()),
        ] {
            bounded_collection(name, length)?;
        }
        bounded_collection("workspace.dirty_paths", self.workspace.dirty_paths.len())?;
        let mut dirty_paths = HashSet::new();
        for path in &self.workspace.dirty_paths {
            validate_file_path("workspace.dirty_paths", path)?;
            if !dirty_paths.insert(path.as_str()) {
                return Err(PlanValidationError(format!(
                    "workspace.dirty_paths contains duplicate {}",
                    path.as_str()
                )));
            }
        }
        if self.evidence.is_empty() || self.slices.is_empty() {
            return Err(PlanValidationError(
                "evidence and slices must be non-empty".into(),
            ));
        }
        for slice in &self.slices {
            bounded_slice(slice)?;
        }
        if self.objective.acceptance.is_empty() {
            return Err(PlanValidationError(
                "objective.acceptance must be non-empty".into(),
            ));
        }
        bounded_collection("objective.acceptance", self.objective.acceptance.len())?;
        validate_sources("objective.sources", &self.objective.sources)?;
        for check in &self.objective.acceptance {
            validate_check(check, "objective.acceptance")?;
        }
        unique_ids(
            "evidence",
            self.evidence.iter().map(|value| value.id.as_str()),
        )?;
        unique_ids(
            "decisions",
            self.decisions.iter().map(|value| value.id.as_str()),
        )?;
        unique_ids(
            "invariants",
            self.invariants.iter().map(|value| value.id.as_str()),
        )?;
        unique_ids("slices", self.slices.iter().map(|value| value.id.as_str()))?;
        unique_ids(
            "open_decisions",
            self.open_decisions.iter().map(|value| value.id.as_str()),
        )?;
        unique_ids(
            "acceptance checks",
            self.objective
                .acceptance
                .iter()
                .map(|value| value.id.as_str()),
        )?;
        for evidence in &self.evidence {
            validate_sources("evidence.sources", &evidence.sources)?;
        }
        for decision in &self.decisions {
            validate_sources("decision.sources", &decision.sources)?;
        }
        for invariant in &self.invariants {
            validate_sources("invariant.sources", &invariant.sources)?;
        }

        let evidence_ids = self
            .evidence
            .iter()
            .map(|value| value.id.as_str().to_owned())
            .collect::<HashSet<_>>();
        let invariant_ids = self
            .invariants
            .iter()
            .map(|value| value.id.as_str().to_owned())
            .collect::<HashSet<_>>();
        let acceptance_ids = self
            .objective
            .acceptance
            .iter()
            .map(|value| value.id.as_str().to_owned())
            .collect::<HashSet<_>>();
        let mut all_check_ids = acceptance_ids.clone();
        for slice in &self.slices {
            unique_ids(
                "done_when checks",
                slice.done_when.iter().map(|value| value.id.as_str()),
            )?;
            for check in &slice.done_when {
                if !all_check_ids.insert(check.id.as_str().to_owned()) {
                    return Err(PlanValidationError(format!(
                        "duplicate check ID {}",
                        check.id
                    )));
                }
            }
        }
        let mut all_read_requirement_ids = HashSet::new();
        let mut all_conditional_read_ids = HashSet::new();
        for slice in &self.slices {
            for read in &slice.required_reads {
                validate_read(read)?;
                if !all_read_requirement_ids.insert(read.id.as_str().to_owned()) {
                    return Err(PlanValidationError(format!(
                        "duplicate ReadRequirement ID {}",
                        read.id
                    )));
                }
            }
            for conditional in &slice.conditional_reads {
                if !all_conditional_read_ids.insert(conditional.id.as_str().to_owned()) {
                    return Err(PlanValidationError(format!(
                        "duplicate ConditionalRead ID {}",
                        conditional.id
                    )));
                }
                validate_read(&conditional.read)?;
                if !all_read_requirement_ids.insert(conditional.read.id.as_str().to_owned()) {
                    return Err(PlanValidationError(format!(
                        "duplicate ReadRequirement ID {}",
                        conditional.read.id
                    )));
                }
            }
        }
        let slice_ids = self
            .slices
            .iter()
            .map(|value| value.id.as_str().to_owned())
            .collect::<HashSet<_>>();
        let mut slice_graph = BTreeMap::new();
        let mut covered_invariants = HashSet::new();
        let mut covered_acceptance = HashSet::new();
        for slice in &self.slices {
            for dependency in &slice.depends_on {
                if !slice_ids.contains(dependency.as_str()) {
                    return Err(PlanValidationError(format!(
                        "slice {} has dangling dependency {}",
                        slice.id, dependency
                    )));
                }
            }
            slice_graph.insert(
                slice.id.as_str().to_owned(),
                slice
                    .depends_on
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect::<Vec<_>>(),
            );
            let file_ids = slice
                .files
                .iter()
                .map(|file| file.path.as_str().to_owned())
                .collect::<HashSet<_>>();
            if file_ids.len() != slice.files.len() {
                return Err(PlanValidationError(format!(
                    "slice {} declares duplicate files",
                    slice.id
                )));
            }
            for file in &slice.files {
                validate_file_path("slice.files.path", &file.path)?;
                bounded_collection("slice.files.symbols", file.symbols.len())?;
            }
            let mut local_read_ids = HashSet::new();
            for read in &slice.required_reads {
                local_read_ids.insert(read.id.as_str());
            }
            for conditional in &slice.conditional_reads {
                local_read_ids.insert(conditional.read.id.as_str());
            }
            if slice.start_condition.is_empty() {
                return Err(PlanValidationError(format!(
                    "slice {} requires a non-empty start_condition",
                    slice.id
                )));
            }
            for reference in &slice.start_condition {
                let evidence = evidence_ids.contains(reference.as_str());
                let read = local_read_ids.contains(reference.as_str());
                if evidence == read {
                    return Err(PlanValidationError(format!(
                        "start condition reference {} is missing or ambiguous",
                        reference
                    )));
                }
            }
            let step_ids = slice
                .steps
                .iter()
                .map(|step| step.id.as_str().to_owned())
                .collect::<HashSet<_>>();
            if step_ids.len() != slice.steps.len() {
                return Err(PlanValidationError(format!(
                    "slice {} has duplicate steps",
                    slice.id
                )));
            }
            for step in &slice.steps {
                bounded_collection("step.files", step.files.len())?;
                bounded_collection("step.symbols", step.symbols.len())?;
                bounded_collection("step.depends_on", step.depends_on.len())?;
                bounded_collection("step.satisfies", step.satisfies.len())?;
                if step.files.is_empty() {
                    return Err(PlanValidationError(format!(
                        "step {} must name affected files",
                        step.id
                    )));
                }
                for path in &step.files {
                    validate_file_path("step.files", path)?;
                    if !file_ids.contains(path.as_str()) {
                        return Err(PlanValidationError(format!(
                            "step {} writes undeclared file {}",
                            step.id,
                            path.as_str()
                        )));
                    }
                }
                for dependency in &step.depends_on {
                    if !step_ids.contains(dependency.as_str()) {
                        return Err(PlanValidationError(format!(
                            "step {} has dangling dependency {}",
                            step.id, dependency
                        )));
                    }
                }
                for reference in &step.satisfies {
                    cover(
                        reference,
                        &invariant_ids,
                        &all_check_ids,
                        &acceptance_ids,
                        &mut covered_invariants,
                        &mut covered_acceptance,
                        "step",
                    )?;
                }
            }
            if slice.steps.is_empty() || slice.verification.is_empty() || slice.done_when.is_empty()
            {
                return Err(PlanValidationError(format!(
                    "slice {} requires steps, verification, and done_when",
                    slice.id
                )));
            }
            if slice
                .verification
                .iter()
                .all(|verification| metadata_only_verification(verification.command.as_str()))
            {
                return Err(PlanValidationError(format!(
                    "slice {} requires an executable behavior verification; repository-state checks are not sufficient",
                    slice.id
                )));
            }
            for check in &slice.done_when {
                validate_check(check, "slice.done_when")?;
            }
            for verification in &slice.verification {
                bounded_collection("verification.covers", verification.covers.len())?;
                bounded_collection(
                    "verification.expected.stdout_contains",
                    verification.expected.stdout_contains.len(),
                )?;
                bounded_collection(
                    "verification.expected.stderr_contains",
                    verification.expected.stderr_contains.len(),
                )?;
                for reference in &verification.covers {
                    cover(
                        reference,
                        &invariant_ids,
                        &all_check_ids,
                        &acceptance_ids,
                        &mut covered_invariants,
                        &mut covered_acceptance,
                        "verification",
                    )?;
                }
            }
            ensure_acyclic(&step_ids, &slice.steps, &slice.id)?;
        }
        ensure_graph_acyclic(&slice_graph, "slice dependency graph")?;
        if covered_invariants.len() != invariant_ids.len() {
            return Err(PlanValidationError(
                "every invariant must be covered by a step or verification".into(),
            ));
        }
        if covered_acceptance.len() != acceptance_ids.len() {
            return Err(PlanValidationError(
                "every acceptance check must be covered by a step or verification".into(),
            ));
        }
        for open in &self.open_decisions {
            bounded_collection("open_decision.affected_slices", open.affected_slices.len())?;
            validate_sources("open_decision.sources", &open.sources)?;
            if open.affected_slices.is_empty() {
                return Err(PlanValidationError(format!(
                    "open decision {} must affect a slice",
                    open.id
                )));
            }
            for slice in &open.affected_slices {
                if !slice_ids.contains(slice.as_str()) {
                    return Err(PlanValidationError(format!(
                        "open decision {} references missing slice {}",
                        open.id, slice
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn validate_for_implementation(&self) -> Result<(), PlanValidationError> {
        self.validate()?;
        if !self.open_decisions.is_empty() {
            return Err(PlanValidationError(
                "implementation-ready plan cannot contain open_decisions".into(),
            ));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PlanError> {
        self.validate()?;
        let value = canonicalize(serde_json::to_value(self)?, 0)?;
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge);
        }
        Ok(bytes)
    }

    pub fn digest(&self) -> Result<PlanDigest, PlanError> {
        Ok(PlanDigest::from_bytes(
            Sha256::digest(self.canonical_bytes()?).into(),
        ))
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, PlanError> {
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge);
        }
        let plan: Self = serde_json::from_slice(bytes)?;
        plan.validate()?;
        if plan.canonical_bytes()? != bytes {
            return Err(PlanValidationError("plan bytes are not canonical JSON".into()).into());
        }
        Ok(plan)
    }

    /// Workspace identity is the SHA-256 of the canonical root path bytes.
    pub fn workspace_matches(&self, workspace: &Path) -> std::io::Result<bool> {
        let root = workspace.canonicalize()?;
        let digest = workspace_root_digest(&root);
        Ok(self.workspace.canonical_root_sha256.as_bytes() == &digest)
    }
}

fn metadata_only_verification(command: &str) -> bool {
    let normalized = command.trim().to_ascii_lowercase();
    let command = normalized
        .strip_prefix("command ")
        .unwrap_or(&normalized)
        .trim();
    matches!(
        command,
        "git status"
            | "git status --short"
            | "git diff"
            | "git diff --check"
            | "git diff --name-only"
            | "git diff --stat"
            | "git diff --name-status"
            | "git status --porcelain"
    )
}

pub fn workspace_root_digest(canonical_workspace: &Path) -> [u8; 32] {
    Sha256::digest(workspace_path_bytes(canonical_workspace)).into()
}

fn workspace_path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn bounded_collection(name: &str, length: usize) -> Result<(), PlanValidationError> {
    if length > MAX_COLLECTION_ITEMS {
        return Err(PlanValidationError(format!(
            "{name} exceeds {MAX_COLLECTION_ITEMS} items"
        )));
    }
    Ok(())
}

fn bounded_slice(slice: &ImplementationSlice) -> Result<(), PlanValidationError> {
    for (name, length) in [
        ("depends_on", slice.depends_on.len()),
        ("files", slice.files.len()),
        ("required_reads", slice.required_reads.len()),
        ("conditional_reads", slice.conditional_reads.len()),
        ("start_condition", slice.start_condition.len()),
        ("steps", slice.steps.len()),
        ("verification", slice.verification.len()),
        ("done_when", slice.done_when.len()),
    ] {
        bounded_collection(name, length)?;
    }
    Ok(())
}

fn validate_read(read: &ReadRequirement) -> Result<(), PlanValidationError> {
    bounded_collection("read.symbols_or_ranges", read.symbols_or_ranges.len())?;
    validate_file_path("read.path", &read.path)
}

fn validate_file_path(field: &str, path: &WorkspacePath) -> Result<(), PlanValidationError> {
    if path.as_str() == "." {
        return Err(PlanValidationError(format!(
            "{field} must identify a file, not the workspace root"
        )));
    }
    Ok(())
}

fn validate_check(check: &Check, field: &str) -> Result<(), PlanValidationError> {
    validate_sources(&format!("{field}.sources"), &check.sources)
}

fn validate_sources(field: &str, sources: &[SourceRef]) -> Result<(), PlanValidationError> {
    if sources.is_empty() {
        return Err(PlanValidationError(format!("{field} must be non-empty")));
    }
    bounded_collection(field, sources.len())?;
    for source in sources {
        if matches!(
            source.kind,
            SourceKind::File | SourceKind::Test | SourceKind::Config
        ) {
            let path = WorkspacePath::new(source.locator.as_str()).map_err(|reason| {
                PlanValidationError(format!("{field} has invalid workspace path: {reason}"))
            })?;
            validate_file_path(field, &path)?;
        }
        if source.line_start == Some(0) || source.line_end == Some(0) {
            return Err(PlanValidationError(format!(
                "{field} has invalid line range"
            )));
        }
        if let (Some(start), Some(end)) = (source.line_start, source.line_end)
            && (start == 0 || end < start)
        {
            return Err(PlanValidationError(format!(
                "{field} has invalid line range"
            )));
        }
    }
    Ok(())
}

fn unique_ids<'a>(
    namespace: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), PlanValidationError> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(PlanValidationError(format!(
                "duplicate ID {value} in {namespace}"
            )));
        }
    }
    Ok(())
}

fn cover(
    reference: &PlanItemId,
    invariants: &HashSet<String>,
    checks: &HashSet<String>,
    acceptance_checks: &HashSet<String>,
    covered_invariants: &mut HashSet<String>,
    covered_checks: &mut HashSet<String>,
    field: &str,
) -> Result<(), PlanValidationError> {
    let invariant = invariants.contains(reference.as_str());
    let check = checks.contains(reference.as_str());
    if !invariant && !check || invariant && check {
        return Err(PlanValidationError(format!(
            "{field} has missing or ambiguous coverage reference {}",
            reference
        )));
    }
    if invariant {
        covered_invariants.insert(reference.as_str().to_owned());
    } else if acceptance_checks.contains(reference.as_str()) {
        covered_checks.insert(reference.as_str().to_owned());
    }
    Ok(())
}

fn ensure_graph_acyclic(
    graph: &BTreeMap<String, Vec<String>>,
    name: &str,
) -> Result<(), PlanValidationError> {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        active: &mut HashSet<String>,
        done: &mut HashSet<String>,
    ) -> bool {
        if done.contains(node) {
            return true;
        }
        if !active.insert(node.to_owned()) {
            return false;
        }
        let valid = graph.get(node).is_none_or(|dependencies| {
            dependencies
                .iter()
                .all(|dependency| visit(dependency, graph, active, done))
        });
        active.remove(node);
        if valid {
            done.insert(node.to_owned());
        }
        valid
    }
    let mut active = HashSet::new();
    let mut done = HashSet::new();
    for node in graph.keys() {
        if !visit(node, graph, &mut active, &mut done) {
            return Err(PlanValidationError(format!("{name} contains a cycle")));
        }
    }
    Ok(())
}

fn ensure_acyclic(
    ids: &HashSet<String>,
    steps: &[ImplementationStep],
    slice: &SliceId,
) -> Result<(), PlanValidationError> {
    let graph = steps
        .iter()
        .map(|step| {
            (
                step.id.as_str().to_owned(),
                step.depends_on
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if graph
        .values()
        .flatten()
        .any(|dependency| !ids.contains(dependency))
    {
        return Err(PlanValidationError(format!(
            "slice {} has a dangling step dependency",
            slice
        )));
    }
    ensure_graph_acyclic(&graph, &format!("slice {} step dependency graph", slice))
}

fn canonicalize(value: Value, depth: usize) -> Result<Value, PlanError> {
    if depth > 64 {
        return Err(PlanError::TooDeep);
    }
    Ok(match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| canonicalize(value, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize(value, depth + 1)?);
            }
            Value::Object(canonical)
        }
        value => value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(value: &str) -> PlanText {
        PlanText::new(value).unwrap()
    }

    #[test]
    fn plan_text_accepts_multiline_content_but_rejects_other_controls() {
        assert_eq!(
            PlanText::new("first\n\tsecond").unwrap().as_str(),
            "first\n\tsecond"
        );
        assert!(PlanText::new("first\rsecond").is_err());
        assert!(PlanText::new("first\0second").is_err());
    }
    fn id(value: &str) -> PlanItemId {
        PlanItemId::new(value).unwrap()
    }
    fn source() -> SourceRef {
        SourceRef {
            kind: SourceKind::File,
            locator: text("reports/spec.md"),
            digest: None,
            line_start: None,
            line_end: None,
        }
    }
    fn check(name: &str) -> Check {
        Check {
            id: id(name),
            statement: text("check"),
            sources: vec![source()],
        }
    }
    fn plan() -> ImplementationPlan {
        let file = WorkspacePath::new("src/lib.rs").unwrap();
        ImplementationPlan {
            schema: PlanSchema::V1,
            id: PlanId::generate(),
            workspace: WorkspaceEvidence {
                canonical_root_sha256: PlanDigest::from_bytes([0; 32]),
                git_commit: None,
                dirty_paths: vec![],
            },
            objective: Objective {
                outcome: text("outcome"),
                acceptance: vec![check("accept")],
                sources: vec![source()],
            },
            evidence: vec![Evidence {
                id: id("evidence"),
                statement: text("fact"),
                sources: vec![source()],
            }],
            decisions: vec![],
            invariants: vec![Invariant {
                id: id("invariant"),
                statement: text("safe"),
                sources: vec![source()],
            }],
            slices: vec![ImplementationSlice {
                id: id("slice"),
                outcome: text("done"),
                depends_on: vec![],
                files: vec![FileChange {
                    path: file.clone(),
                    disposition: FileDisposition::Modify,
                    purpose: text("change"),
                    symbols: vec![],
                }],
                required_reads: vec![ReadRequirement {
                    id: id("read"),
                    path: file.clone(),
                    symbols_or_ranges: vec![],
                    obtain: text("understand"),
                }],
                conditional_reads: vec![],
                start_condition: vec![id("evidence")],
                steps: vec![ImplementationStep {
                    id: id("step"),
                    action: text("change"),
                    files: vec![file],
                    symbols: vec![],
                    depends_on: vec![],
                    satisfies: vec![id("invariant"), id("accept")],
                }],
                verification: vec![Verification {
                    command: text("cargo test"),
                    working_directory: WorkspacePath::new(".").unwrap(),
                    expected: VerificationExpected {
                        exit_code: 0,
                        stdout_contains: vec![],
                        stderr_contains: vec![],
                    },
                    covers: vec![],
                }],
                done_when: vec![check("done")],
            }],
            open_decisions: vec![],
        }
    }

    fn add_second_slice(value: &mut ImplementationPlan) {
        let mut slice = value.slices[0].clone();
        slice.id = id("slice-two");
        slice.required_reads[0].id = id("read-two");
        slice.steps[0].id = id("step-two");
        slice.done_when[0].id = id("done-two");
        value.slices.push(slice);
    }

    fn assert_nested_bound(mutator: impl FnOnce(&mut ImplementationPlan)) {
        let mut value = plan();
        mutator(&mut value);
        let error = value.validate().unwrap_err();
        assert!(error.0.contains("exceeds 256 items"), "{error}");
    }

    #[test]
    fn canonical_bytes_are_sorted_and_digest_is_stable() {
        let value = plan();
        let first = value.canonical_bytes().unwrap();
        let second = ImplementationPlan::from_canonical_bytes(&first).unwrap();
        assert_eq!(first, second.canonical_bytes().unwrap());
        assert_eq!(value.digest().unwrap(), second.digest().unwrap());
        assert!(
            String::from_utf8(first)
                .unwrap()
                .starts_with("{\"decisions\"")
        );
    }

    #[test]
    fn strict_shape_rejects_unknown_fields_and_noncanonical_bytes() {
        let mut object = serde_json::to_value(plan()).unwrap();
        object
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(true));
        assert!(serde_json::from_value::<ImplementationPlan>(object).is_err());
        let bytes = serde_json::to_vec(&plan()).unwrap();
        assert!(ImplementationPlan::from_canonical_bytes(&bytes).is_err());
    }

    #[test]
    fn validation_rejects_traversal_dangling_and_cycles() {
        assert!(WorkspacePath::new("../secret").is_err());
        assert!(WorkspacePath::new("/etc/passwd").is_err());
        let mut value = plan();
        value.slices[0].steps[0].files = vec![WorkspacePath::new("src/other.rs").unwrap()];
        assert!(value.validate().is_err());
        let mut value = plan();
        value.slices[0].steps[0].depends_on = vec![id("missing")];
        assert!(value.validate().is_err());
        let mut value = plan();
        value.slices[0].steps[0].depends_on = vec![id("step")];
        assert!(value.validate().is_err());
    }

    #[test]
    fn validation_rejects_repository_state_as_only_verification() {
        let mut value = plan();
        value.slices[0].verification[0].command = text("git diff --name-only");
        let error = value.validate().unwrap_err();
        assert!(
            error.0.contains("executable behavior verification"),
            "{error}"
        );

        value.slices[0].verification[0].command = text("pytest -q");
        assert!(value.validate().is_ok());
    }

    #[test]
    fn implementation_gate_rejects_open_decisions() {
        let mut value = plan();
        value.open_decisions.push(OpenDecision {
            id: id("choice"),
            question: text("which"),
            consequences: text("changes"),
            affected_slices: vec![id("slice")],
            sources: vec![source()],
        });
        assert!(value.validate().is_ok());
        assert!(value.validate_for_implementation().is_err());
    }

    #[test]
    fn every_nested_collection_is_bounded() {
        let count = MAX_COLLECTION_ITEMS + 1;
        assert_nested_bound(|value| {
            value.workspace.dirty_paths = vec![WorkspacePath::new("src/lib.rs").unwrap(); count]
        });
        assert_nested_bound(|value| value.objective.acceptance = vec![check("accept"); count]);
        assert_nested_bound(|value| value.objective.sources = vec![source(); count]);
        assert_nested_bound(|value| value.evidence[0].sources = vec![source(); count]);
        assert_nested_bound(|value| value.slices[0].depends_on = vec![id("slice"); count]);
        assert_nested_bound(|value| {
            value.slices[0].files = vec![value.slices[0].files[0].clone(); count]
        });
        assert_nested_bound(|value| value.slices[0].files[0].symbols = vec![text("symbol"); count]);
        assert_nested_bound(|value| {
            value.slices[0].required_reads = vec![value.slices[0].required_reads[0].clone(); count]
        });
        assert_nested_bound(|value| {
            value.slices[0].conditional_reads = vec![
                ConditionalRead {
                    id: id("conditional"),
                    condition: text("condition"),
                    read: value.slices[0].required_reads[0].clone()
                };
                count
            ]
        });
        assert_nested_bound(|value| value.slices[0].start_condition = vec![id("evidence"); count]);
        assert_nested_bound(|value| {
            value.slices[0].steps = vec![value.slices[0].steps[0].clone(); count]
        });
        assert_nested_bound(|value| {
            value.slices[0].steps[0].files = vec![WorkspacePath::new("src/lib.rs").unwrap(); count]
        });
        assert_nested_bound(|value| value.slices[0].steps[0].symbols = vec![text("symbol"); count]);
        assert_nested_bound(|value| value.slices[0].steps[0].depends_on = vec![id("step"); count]);
        assert_nested_bound(|value| value.slices[0].steps[0].satisfies = vec![id("accept"); count]);
        assert_nested_bound(|value| {
            value.slices[0].verification = vec![value.slices[0].verification[0].clone(); count]
        });
        assert_nested_bound(|value| {
            value.slices[0].verification[0].covers = vec![id("accept"); count]
        });
        assert_nested_bound(|value| value.slices[0].done_when = vec![check("done"); count]);
        assert_nested_bound(|value| {
            value.slices[0].required_reads[0].symbols_or_ranges = vec![text("symbol"); count]
        });
        assert_nested_bound(|value| {
            value.open_decisions = vec![OpenDecision {
                id: id("choice"),
                question: text("which"),
                consequences: text("changes"),
                affected_slices: vec![id("slice"); count],
                sources: vec![source()],
            }]
        });
    }

    #[test]
    fn start_conditions_resolve_only_local_read_requirements() {
        let mut value = plan();
        add_second_slice(&mut value);
        value.slices[0].start_condition = vec![id("read-two")];
        assert!(value.validate().is_err());

        let mut value = plan();
        add_second_slice(&mut value);
        value.slices[1].start_condition = vec![id("read")];
        assert!(value.validate().is_err());

        let mut value = plan();
        value.slices[0].conditional_reads.push(ConditionalRead {
            id: id("conditional"),
            condition: text("when needed"),
            read: ReadRequirement {
                id: id("conditional-read"),
                path: WorkspacePath::new("src/other.rs").unwrap(),
                symbols_or_ranges: vec![],
                obtain: text("more evidence"),
            },
        });
        value.slices[0].start_condition = vec![id("conditional")];
        assert!(value.validate().is_err());
        value.slices[0].start_condition = vec![id("conditional-read")];
        assert!(value.validate().is_ok());
    }

    #[test]
    fn file_like_sources_and_file_contexts_reject_root_or_traversal() {
        for kind in [SourceKind::File, SourceKind::Test, SourceKind::Config] {
            let mut value = plan();
            value.evidence[0].sources[0].kind = kind;
            value.evidence[0].sources[0].locator = text("../outside");
            assert!(value.validate().is_err());
        }
        let mut value = plan();
        value.slices[0].files[0].path = WorkspacePath::new(".").unwrap();
        assert!(value.validate().is_err());
        let mut value = plan();
        value.slices[0].required_reads[0].path = WorkspacePath::new(".").unwrap();
        assert!(value.validate().is_err());
        let mut value = plan();
        value.slices[0].steps[0].files = vec![WorkspacePath::new(".").unwrap()];
        assert!(value.validate().is_err());
        let mut value = plan();
        value.workspace.dirty_paths = vec![WorkspacePath::new(".").unwrap()];
        assert!(value.validate().is_err());
        assert!(plan().validate().is_ok());
    }

    #[test]
    fn plan_ids_are_canonical_uuid_v7_without_a_prefix() {
        let value = PlanId::generate();
        let encoded = value.to_string();
        assert_eq!(encoded.parse::<PlanId>().unwrap(), value);
        assert!(!encoded.contains('_'));
        assert!(encoded.to_uppercase().parse::<PlanId>().is_err());
        assert!(encoded.replace('-', "").parse::<PlanId>().is_err());
        assert!(
            "550e8400-e29b-41d4-a716-446655440000"
                .parse::<PlanId>()
                .is_err()
        );
        assert!(PlanId::new(Uuid::nil()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_identity_hashes_non_utf8_os_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let parent = std::env::temp_dir().join(format!("agl-plan-path-{}", Uuid::now_v7()));
        let root = parent.join(OsStr::from_bytes(b"workspace-\xff"));
        std::fs::create_dir_all(&root).unwrap();
        let canonical = root.canonicalize().unwrap();
        let raw = workspace_root_digest(&canonical);
        let lossy: [u8; 32] = Sha256::digest(canonical.to_string_lossy().as_bytes()).into();
        assert_ne!(raw, lossy);
        let mut value = plan();
        value.workspace.canonical_root_sha256 = PlanDigest::from_bytes(raw);
        assert!(value.workspace_matches(&root).unwrap());
        std::fs::remove_dir_all(parent).unwrap();
    }
}
