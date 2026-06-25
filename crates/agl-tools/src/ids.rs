use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdKind {
    Provider,
    Hook,
    Tool,
    Skill,
}

impl IdKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Hook => "hook",
            Self::Tool => "tool",
            Self::Skill => "skill",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolProviderId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HookId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SkillId(String);

macro_rules! id_type {
    ($type:ident, $kind:expr) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, ToolProviderIdError> {
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

id_type!(ToolProviderId, IdKind::Provider);
id_type!(HookId, IdKind::Hook);
id_type!(ToolId, IdKind::Tool);
id_type!(SkillId, IdKind::Skill);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProviderIdError {
    kind: IdKind,
    value: String,
}

impl std::fmt::Display for ToolProviderIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} id must use lowercase ASCII letters, digits, hyphens, underscores, dots, or one namespace colon: {}",
            self.kind.as_str(),
            self.value
        )
    }
}

impl std::error::Error for ToolProviderIdError {}

fn validate_id(kind: IdKind, value: &str) -> Result<(), ToolProviderIdError> {
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
        Err(ToolProviderIdError {
            kind,
            value: value.to_string(),
        })
    }
}
