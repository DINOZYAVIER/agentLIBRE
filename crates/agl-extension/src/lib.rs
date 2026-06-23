mod registry;

pub use registry::{StaticExtensionRegistry, StaticExtensionRegistryError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionIdKind {
    Extension,
    Hook,
    Tool,
    Skill,
}

impl ExtensionIdKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Hook => "hook",
            Self::Tool => "tool",
            Self::Skill => "skill",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExtensionId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HookId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SkillId(String);

macro_rules! id_type {
    ($type:ident, $kind:expr) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, ExtensionIdError> {
                let value = value.into();
                validate_id($kind, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

id_type!(ExtensionId, ExtensionIdKind::Extension);
id_type!(HookId, ExtensionIdKind::Hook);
id_type!(ToolId, ExtensionIdKind::Tool);
id_type!(SkillId, ExtensionIdKind::Skill);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionIdError {
    kind: ExtensionIdKind,
    value: String,
}

impl std::fmt::Display for ExtensionIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} id must use lowercase ASCII letters, digits, hyphens, underscores, dots, or one namespace colon: {}",
            self.kind.as_str(),
            self.value
        )
    }
}

impl std::error::Error for ExtensionIdError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaticExtensionDeclaration {
    pub id: ExtensionId,
    pub name: String,
    pub version: String,
    pub hooks: Vec<HookDeclaration>,
    pub tools: Vec<ToolDeclaration>,
    pub bundled_skills: Vec<BundledSkillDeclaration>,
}

pub trait StaticExtension {
    fn declaration(&self) -> &StaticExtensionDeclaration;

    fn run_hook(&self, input: HookInput) -> HookResult;
}

impl StaticExtensionDeclaration {
    pub fn new(
        id: ExtensionId,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, StaticExtensionDeclarationError> {
        let name = name.into();
        let version = version.into();
        ensure_non_blank("extension name", &name)?;
        ensure_non_blank("extension version", &version)?;
        Ok(Self {
            id,
            name,
            version,
            hooks: Vec::new(),
            tools: Vec::new(),
            bundled_skills: Vec::new(),
        })
    }

    pub fn with_hook(mut self, hook: HookDeclaration) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn with_tool(mut self, tool: ToolDeclaration) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_bundled_skill(mut self, skill: BundledSkillDeclaration) -> Self {
        self.bundled_skills.push(skill);
        self
    }

    pub fn validate(&self) -> Result<(), StaticExtensionDeclarationError> {
        ensure_non_blank("extension name", &self.name)?;
        ensure_non_blank("extension version", &self.version)?;
        reject_duplicate_ids(self.hooks.iter().map(|hook| hook.id.as_str()), "hook")?;
        reject_duplicate_ids(self.tools.iter().map(|tool| tool.id.as_str()), "tool")?;
        reject_duplicate_ids(
            self.bundled_skills.iter().map(|skill| skill.id.as_str()),
            "bundled skill",
        )?;
        Ok(())
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundledSkillDeclaration {
    pub id: SkillId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    ContextPrepare,
    ModelRequest,
    ModelResponse,
    ArtifactWrite,
    TurnFinish,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextPrepare => "context.prepare",
            Self::ModelRequest => "model.request",
            Self::ModelResponse => "model.response",
            Self::ArtifactWrite => "artifact.write",
            Self::TurnFinish => "turn.finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStatus {
    Pass,
    Warn,
    Fail,
    Repair,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookMessage {
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookInput {
    pub hook_id: HookId,
    pub event: HookEvent,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookResult {
    pub hook_id: HookId,
    pub status: HookStatus,
    pub messages: Vec<HookMessage>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookBatchRequest {
    pub event: HookEvent,
    pub hooks: Vec<HookId>,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookBatchResult {
    pub event: HookEvent,
    pub results: Vec<HookResult>,
}

impl HookBatchResult {
    pub fn status(&self) -> HookStatus {
        if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Fail)
        {
            HookStatus::Fail
        } else if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Repair)
        {
            HookStatus::Repair
        } else if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Warn)
        {
            HookStatus::Warn
        } else {
            HookStatus::Pass
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StaticExtensionDeclarationError {
    BlankField { field: &'static str },
    DuplicateId { kind: &'static str, id: String },
}

impl std::fmt::Display for StaticExtensionDeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlankField { field } => write!(f, "{field} cannot be blank"),
            Self::DuplicateId { kind, id } => write!(f, "duplicate {kind} id `{id}`"),
        }
    }
}

impl std::error::Error for StaticExtensionDeclarationError {}

fn validate_id(kind: ExtensionIdKind, value: &str) -> Result<(), ExtensionIdError> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        })
        && value.matches(':').count() <= 1
        && !value.starts_with(':')
        && !value.ends_with(':');
    if valid {
        Ok(())
    } else {
        Err(ExtensionIdError {
            kind,
            value: value.to_string(),
        })
    }
}

fn ensure_non_blank(
    field: &'static str,
    value: &str,
) -> Result<(), StaticExtensionDeclarationError> {
    if value.trim().is_empty() {
        Err(StaticExtensionDeclarationError::BlankField { field })
    } else {
        Ok(())
    }
}

fn reject_duplicate_ids<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    kind: &'static str,
) -> Result<(), StaticExtensionDeclarationError> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(StaticExtensionDeclarationError::DuplicateId {
                kind,
                id: id.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_namespaced_values() {
        assert_eq!(
            HookId::new("task_spec.validate").unwrap().as_str(),
            "task_spec.validate"
        );
        assert_eq!(
            SkillId::new("core:task-spec").unwrap().as_str(),
            "core:task-spec"
        );
    }

    #[test]
    fn ids_reject_invalid_values() {
        assert!(HookId::new("").is_err());
        assert!(HookId::new("TaskSpec.Validate").is_err());
        assert!(HookId::new("a:b:c").is_err());
        assert!(HookId::new(":bad").is_err());
    }

    #[test]
    fn id_deserialization_uses_validation() {
        let hook: HookId = serde_json::from_str("\"task_spec.validate\"").unwrap();

        assert_eq!(hook.as_str(), "task_spec.validate");
        assert!(serde_json::from_str::<HookId>("\"TaskSpec.Validate\"").is_err());
    }

    #[test]
    fn declaration_rejects_duplicate_hooks() {
        let declaration = StaticExtensionDeclaration::new(
            ExtensionId::new("core-guards").unwrap(),
            "Core Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("json.validate").unwrap(),
            event: HookEvent::ModelResponse,
            required: true,
        })
        .with_hook(HookDeclaration {
            id: HookId::new("json.validate").unwrap(),
            event: HookEvent::ArtifactWrite,
            required: true,
        });

        assert_eq!(
            declaration.validate().unwrap_err(),
            StaticExtensionDeclarationError::DuplicateId {
                kind: "hook",
                id: "json.validate".to_string(),
            }
        );
    }
}
