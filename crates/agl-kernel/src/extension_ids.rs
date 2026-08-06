use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentifierKind {
    Tool,
    Extension,
    Effect,
    WorkflowEvent,
    Hook,
    Skill,
}

impl IdentifierKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Extension => "extension",
            Self::Effect => "effect",
            Self::WorkflowEvent => "workflow event",
            Self::Hook => "hook",
            Self::Skill => "skill",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentifierError {
    kind: IdentifierKind,
    value: String,
}

impl IdentifierError {
    pub fn kind(&self) -> IdentifierKind {
        self.kind
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl Display for IdentifierError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if self.kind == IdentifierKind::Hook {
            write!(
                formatter,
                "hook ID must be extension-qualified as `<extension>:<hook>` using lowercase ASCII letters, digits, hyphens, underscores, and dots: {}",
                self.value
            )
        } else {
            write!(
                formatter,
                "{} ID must use lowercase ASCII letters, digits, hyphens, underscores, dots, or one namespace colon: {}",
                self.kind.as_str(),
                self.value
            )
        }
    }
}

impl std::error::Error for IdentifierError {}

fn validate_identifier(kind: IdentifierKind, value: &str) -> Result<(), IdentifierError> {
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
        Err(IdentifierError {
            kind,
            value: value.to_owned(),
        })
    }
}

fn validate_hook_identifier(kind: IdentifierKind, value: &str) -> Result<(), IdentifierError> {
    validate_identifier(kind, value)?;
    if value.matches(':').count() == 1 {
        Ok(())
    } else {
        Err(IdentifierError {
            kind,
            value: value.to_owned(),
        })
    }
}

fn validate_extension_identifier(kind: IdentifierKind, value: &str) -> Result<(), IdentifierError> {
    validate_identifier(kind, value)?;
    if value.contains(':') {
        Err(IdentifierError {
            kind,
            value: value.to_owned(),
        })
    } else {
        Ok(())
    }
}

macro_rules! identifier_type {
    ($name:ident, $kind:expr, $validator:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                $validator($kind, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

identifier_type!(ToolId, IdentifierKind::Tool, validate_hook_identifier);
identifier_type!(
    ExtensionId,
    IdentifierKind::Extension,
    validate_extension_identifier
);
identifier_type!(EffectId, IdentifierKind::Effect, validate_hook_identifier);
identifier_type!(
    WorkflowEventId,
    IdentifierKind::WorkflowEvent,
    validate_hook_identifier
);
identifier_type!(HookId, IdentifierKind::Hook, validate_hook_identifier);
identifier_type!(SkillId, IdentifierKind::Skill, validate_identifier);

impl HookId {
    pub fn extension_namespace(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated hook IDs are extension-qualified")
            .0
    }

    pub fn local_name(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated hook IDs are extension-qualified")
            .1
    }
}

impl ToolId {
    pub fn extension_namespace(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated tool IDs are extension-qualified")
            .0
    }

    pub fn local_name(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated tool IDs are extension-qualified")
            .1
    }
}

impl EffectId {
    pub fn extension_namespace(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated effect IDs are extension-qualified")
            .0
    }

    pub fn local_name(&self) -> &str {
        self.0
            .split_once(':')
            .expect("validated effect IDs are extension-qualified")
            .1
    }

    pub fn host_screen_capture() -> Self {
        Self::standard("agl:host.screen_capture")
    }

    pub fn spawn_subagent() -> Self {
        Self::standard("agl:agent.spawn")
    }

    pub fn session_working_directory() -> Self {
        Self::standard("agl:session.working_directory")
    }

    pub fn spawn_process() -> Self {
        Self::standard("agl:process.spawn")
    }

    pub fn control_process() -> Self {
        Self::standard("agl:process.control")
    }

    pub fn host_process_execution() -> Self {
        Self::standard("agl:process.host_execution")
    }

    pub fn shell_login_startup() -> Self {
        Self::standard("agl:process.shell_login_startup")
    }

    pub fn repo_files() -> Self {
        Self::standard("agl:repo.files")
    }

    pub fn repo_workspace() -> Self {
        Self::standard("agl:repo.workspace")
    }

    pub fn repo_hooks() -> Self {
        Self::standard("agl:repo.hooks")
    }

    pub fn store_memory_entries() -> Self {
        Self::standard("agl:store.memory_entries")
    }

    pub fn store_memory_suggestions() -> Self {
        Self::standard("agl:store.memory_suggestions")
    }

    pub fn store_notes() -> Self {
        Self::standard("agl:store.notes")
    }

    pub fn store_note_links() -> Self {
        Self::standard("agl:store.note_links")
    }

    pub fn store_cron() -> Self {
        Self::standard("agl:store.cron")
    }

    pub fn store_schema() -> Self {
        Self::standard("agl:store.schema")
    }

    pub fn matrix_outbox() -> Self {
        Self::standard("agl:matrix.outbox")
    }

    pub fn store_idempotency() -> Self {
        Self::standard("agl:store.idempotency")
    }

    pub fn store_permission_requests() -> Self {
        Self::standard("agl:store.permission_requests")
    }

    pub fn store_permission_grants() -> Self {
        Self::standard("agl:store.permission_grants")
    }

    pub fn skill_trust() -> Self {
        Self::standard("agl:skill.trust")
    }

    fn standard(value: &'static str) -> Self {
        Self::new(value).expect("standard effect ID is valid")
    }
}
