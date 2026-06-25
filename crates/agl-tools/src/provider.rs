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
    pub required_arguments: Vec<String>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolProviderDeclarationError {
    BlankField { field: &'static str },
    DuplicateId { kind: &'static str, id: String },
}

impl std::fmt::Display for ToolProviderDeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlankField { field } => write!(f, "{field} cannot be blank"),
            Self::DuplicateId { kind, id } => write!(f, "duplicate {kind} id `{id}`"),
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
