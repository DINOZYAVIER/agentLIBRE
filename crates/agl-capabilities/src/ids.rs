use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentifierKind {
    Capability,
    Provider,
    Hook,
    Skill,
}

impl IdentifierKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Provider => "provider",
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
        write!(
            formatter,
            "{} ID must use lowercase ASCII letters, digits, hyphens, underscores, dots, or one namespace colon: {}",
            self.kind.as_str(),
            self.value
        )
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

macro_rules! identifier_type {
    ($name:ident, $kind:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                validate_identifier($kind, &value)?;
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

identifier_type!(CapabilityId, IdentifierKind::Capability);
identifier_type!(ProviderId, IdentifierKind::Provider);
identifier_type!(HookId, IdentifierKind::Hook);
identifier_type!(SkillId, IdentifierKind::Skill);
