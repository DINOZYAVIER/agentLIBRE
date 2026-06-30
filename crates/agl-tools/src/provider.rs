use serde::{Deserialize, Serialize};

use crate::hook::HookEvent;
use crate::ids::{HookId, ToolId, ToolProviderId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProviderDeclaration {
    pub id: ToolProviderId,
    pub name: String,
    pub version: String,
    pub source: ToolProviderSource,
    pub trust: ToolProviderTrust,
    pub hooks: Vec<HookDeclaration>,
    pub tools: Vec<ToolDeclaration>,
}

impl ToolProviderDeclaration {
    pub fn new(
        id: ToolProviderId,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, ToolProviderDeclarationError> {
        let name = name.into();
        let version = version.into();
        ensure_non_blank("provider name", &name)?;
        ensure_non_blank("provider version", &version)?;
        Ok(Self {
            id,
            name,
            version,
            source: ToolProviderSource::Builtin,
            trust: ToolProviderTrust::TrustedByBinary,
            hooks: Vec::new(),
            tools: Vec::new(),
        })
    }

    pub fn registered_third_party(
        id: ToolProviderId,
        name: impl Into<String>,
        version: impl Into<String>,
        trust: ToolProviderTrust,
    ) -> Result<Self, ToolProviderDeclarationError> {
        let mut declaration = Self::new(id, name, version)?;
        declaration.source = ToolProviderSource::ThirdPartyRegistered;
        declaration.trust = trust;
        Ok(declaration)
    }

    pub fn test_fixture(
        id: ToolProviderId,
        name: impl Into<String>,
        version: impl Into<String>,
        trust: ToolProviderTrust,
    ) -> Result<Self, ToolProviderDeclarationError> {
        let mut declaration = Self::new(id, name, version)?;
        declaration.source = ToolProviderSource::TestFixture;
        declaration.trust = trust;
        Ok(declaration)
    }

    pub fn with_hook(mut self, hook: HookDeclaration) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn with_tool(mut self, tool: ToolDeclaration) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_trust(mut self, trust: ToolProviderTrust) -> Self {
        self.trust = trust;
        self
    }

    pub fn permits_tool_execution(&self) -> bool {
        self.trust.permits_tool_execution()
    }

    pub fn validate(&self) -> Result<(), ToolProviderDeclarationError> {
        ensure_non_blank("provider name", &self.name)?;
        ensure_non_blank("provider version", &self.version)?;
        reject_duplicate_ids(self.hooks.iter().map(|hook| hook.id.as_str()), "hook")?;
        reject_duplicate_ids(self.tools.iter().map(|tool| tool.id.as_str()), "tool")?;
        for tool in &self.tools {
            ensure_non_blank("tool description", &tool.description)?;
            for argument in &tool.required_arguments {
                ensure_non_blank("tool required argument", argument)?;
            }
            reject_duplicate_ids(
                tool.required_arguments.iter().map(String::as_str),
                "tool required argument",
            )?;
            validate_tool_operation(tool)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProviderSource {
    Builtin,
    ThirdPartyRegistered,
    TestFixture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProviderTrust {
    TrustedByBinary,
    TrustedRegistered,
    Unsupported,
    Unknown,
    Changed,
    Revoked,
}

impl ToolProviderTrust {
    pub fn permits_tool_execution(self) -> bool {
        matches!(self, Self::TrustedByBinary | Self::TrustedRegistered)
    }

    pub fn block_reason(self) -> &'static str {
        match self {
            Self::TrustedByBinary | Self::TrustedRegistered => "tool provider is trusted",
            Self::Unsupported => "tool provider state is unsupported",
            Self::Unknown => "tool provider trust state is unknown",
            Self::Changed => "tool provider declaration has changed",
            Self::Revoked => "tool provider trust was revoked",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookDeclaration {
    pub id: HookId,
    pub event: HookEvent,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDeclaration {
    pub id: ToolId,
    pub description: String,
    pub capability: ToolCapability,
    pub operation_kind: ToolOperationKind,
    pub state_effects: Vec<ToolStateEffect>,
    pub visible_in_read_only: bool,
    pub required_arguments: Vec<String>,
}

impl ToolDeclaration {
    pub fn new(
        id: ToolId,
        description: impl Into<String>,
        capability: ToolCapability,
        required_arguments: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id,
            description: description.into(),
            operation_kind: capability.default_operation_kind(),
            capability,
            state_effects: Vec::new(),
            visible_in_read_only: capability.is_visible_in_read_only(),
            required_arguments: required_arguments.into_iter().map(Into::into).collect(),
        }
    }

    pub fn with_operation_kind(mut self, operation_kind: ToolOperationKind) -> Self {
        self.operation_kind = operation_kind;
        self
    }

    pub fn with_state_effects(
        mut self,
        state_effects: impl IntoIterator<Item = ToolStateEffect>,
    ) -> Self {
        self.state_effects = state_effects.into_iter().collect();
        self
    }

    pub fn visible_in_read_only(mut self, visible: bool) -> Self {
        self.visible_in_read_only = visible;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    Read,
    Write,
}

impl ToolCapability {
    pub fn is_visible_in_read_only(self) -> bool {
        matches!(self, Self::Read)
    }

    pub fn default_operation_kind(self) -> ToolOperationKind {
        match self {
            Self::Read => ToolOperationKind::Read,
            Self::Write => ToolOperationKind::Write,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOperationKind {
    Read,
    Write,
    Execute,
    Approve,
    Admin,
}

impl ToolOperationKind {
    pub fn is_state_mutating(self) -> bool {
        !matches!(self, Self::Read)
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Execute => 2,
            Self::Approve => 3,
            Self::Admin => 4,
        }
    }

    pub fn permits(self, requested: Self) -> bool {
        self.rank() >= requested.rank()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "execute" => Some(Self::Execute),
            "approve" => Some(Self::Approve),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStateEffect {
    RepoFiles,
    RepoWorkspace,
    RepoHooks,
    StoreMemoryEntries,
    StoreMemorySuggestions,
    StoreNotes,
    StoreNoteLinks,
    StoreCron,
    StoreSchema,
    MatrixOutbox,
    StoreIdempotency,
    StorePermissionRequests,
    StorePermissionGrants,
    SkillTrust,
}

impl ToolStateEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepoFiles => "repo_files",
            Self::RepoWorkspace => "repo_workspace",
            Self::RepoHooks => "repo_hooks",
            Self::StoreMemoryEntries => "store_memory_entries",
            Self::StoreMemorySuggestions => "store_memory_suggestions",
            Self::StoreNotes => "store_notes",
            Self::StoreNoteLinks => "store_note_links",
            Self::StoreCron => "store_cron",
            Self::StoreSchema => "store_schema",
            Self::MatrixOutbox => "matrix_outbox",
            Self::StoreIdempotency => "store_idempotency",
            Self::StorePermissionRequests => "store_permission_requests",
            Self::StorePermissionGrants => "store_permission_grants",
            Self::SkillTrust => "skill_trust",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolProviderDeclarationError {
    BlankField { field: &'static str },
    DuplicateId { kind: &'static str, id: String },
    InvalidToolOperation { id: String, message: String },
}

impl std::fmt::Display for ToolProviderDeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlankField { field } => write!(f, "{field} cannot be blank"),
            Self::DuplicateId { kind, id } => write!(f, "duplicate {kind} id `{id}`"),
            Self::InvalidToolOperation { id, message } => {
                write!(f, "tool `{id}` has invalid operation metadata: {message}")
            }
        }
    }
}

impl std::error::Error for ToolProviderDeclarationError {}

fn ensure_non_blank(field: &'static str, value: &str) -> Result<(), ToolProviderDeclarationError> {
    if value.trim().is_empty() {
        Err(ToolProviderDeclarationError::BlankField { field })
    } else {
        Ok(())
    }
}

fn reject_duplicate_ids<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    kind: &'static str,
) -> Result<(), ToolProviderDeclarationError> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(ToolProviderDeclarationError::DuplicateId {
                kind,
                id: id.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_tool_operation(tool: &ToolDeclaration) -> Result<(), ToolProviderDeclarationError> {
    match tool.capability {
        ToolCapability::Read if tool.operation_kind != ToolOperationKind::Read => {
            return Err(ToolProviderDeclarationError::InvalidToolOperation {
                id: tool.id.as_str().to_string(),
                message: "read capability must use read operation kind".to_string(),
            });
        }
        ToolCapability::Write if tool.operation_kind == ToolOperationKind::Read => {
            return Err(ToolProviderDeclarationError::InvalidToolOperation {
                id: tool.id.as_str().to_string(),
                message: "write capability cannot use read operation kind".to_string(),
            });
        }
        _ => {}
    }

    if tool.operation_kind.is_state_mutating() && tool.state_effects.is_empty() {
        return Err(ToolProviderDeclarationError::InvalidToolOperation {
            id: tool.id.as_str().to_string(),
            message: "state-mutating operations must declare state effects".to_string(),
        });
    }
    if tool.operation_kind == ToolOperationKind::Read && !tool.state_effects.is_empty() {
        return Err(ToolProviderDeclarationError::InvalidToolOperation {
            id: tool.id.as_str().to_string(),
            message: "read operations must not declare state effects".to_string(),
        });
    }
    Ok(())
}
