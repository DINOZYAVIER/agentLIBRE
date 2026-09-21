use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Version};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseIdError {
    PrefixMismatch { expected: &'static str },
    InvalidUuid,
    NonCanonical,
    UnsupportedUuidVersion,
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixMismatch { expected } => {
                write!(formatter, "ID must start with {expected}")
            }
            Self::InvalidUuid => formatter.write_str("ID payload must be a UUID"),
            Self::NonCanonical => {
                formatter.write_str("ID UUID must be canonical lowercase hyphenated text")
            }
            Self::UnsupportedUuidVersion => formatter.write_str("ID payload must be a UUIDv7"),
        }
    }
}
impl std::error::Error for ParseIdError {}

pub(crate) fn parse_uuid(value: &str, prefix: &'static str) -> Result<Uuid, ParseIdError> {
    let payload = value
        .strip_prefix(prefix)
        .ok_or(ParseIdError::PrefixMismatch { expected: prefix })?;
    let uuid = Uuid::parse_str(payload).map_err(|_| ParseIdError::InvalidUuid)?;
    if payload != uuid.hyphenated().to_string() {
        return Err(ParseIdError::NonCanonical);
    }
    if uuid.get_version() != Some(Version::SortRand) {
        return Err(ParseIdError::UnsupportedUuidVersion);
    }
    Ok(uuid)
}

macro_rules! text_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);
        impl $name {
            pub fn generate() -> Self {
                Self(format!("{}{}", $prefix, Uuid::now_v7()))
            }
            pub fn parse(value: &str) -> Result<Self, ParseIdError> {
                parse_uuid(value, $prefix).map(|_| Self(value.to_owned()))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
        impl FromStr for $name {
            type Err = ParseIdError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

text_id!(RequestId, "req_");
text_id!(MessageId, "msg_");
text_id!(DaemonInstanceId, "daemon_");

macro_rules! compact_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);
        impl $name {
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }
            pub fn parse(value: &str) -> Result<Self, ParseIdError> {
                parse_uuid(value, $prefix).map(Self)
            }
            pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, ParseIdError> {
                let uuid = Uuid::from_bytes(bytes);
                if uuid.get_version() != Some(Version::SortRand) {
                    return Err(ParseIdError::UnsupportedUuidVersion);
                }
                Ok(Self(uuid))
            }
            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
            pub const fn into_bytes(self) -> [u8; 16] {
                self.0.into_bytes()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}{}", $prefix, self.0.hyphenated())
            }
        }
        impl FromStr for $name {
            type Err = ParseIdError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

compact_id!(AgentRunId, "run_");
compact_id!(ConversationId, "conv_");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_correlation_ids_are_compact_strict_uuid_v7_values() {
        let run = AgentRunId::generate();
        let conversation = ConversationId::generate();
        assert_eq!(std::mem::size_of_val(&run), 16);
        assert_eq!(AgentRunId::from_bytes(run.into_bytes()).unwrap(), run);
        assert!(AgentRunId::from_bytes([0; 16]).is_err());
        assert_eq!(run.to_string().parse::<AgentRunId>().unwrap(), run);
        assert_eq!(
            conversation.to_string().parse::<ConversationId>().unwrap(),
            conversation
        );
    }

    #[test]
    fn remaining_text_ids_are_strict() {
        for value in [
            RequestId::generate().to_string(),
            MessageId::generate().to_string(),
            DaemonInstanceId::generate().to_string(),
        ] {
            assert!(!value.contains(char::is_uppercase));
        }
        assert!(RequestId::parse("req_not-a-uuid").is_err());
        assert!(AgentRunId::parse("run_550e8400-e29b-41d4-a716-446655440000").is_err());
    }
}
