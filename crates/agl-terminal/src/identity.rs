use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use uuid::{Uuid, Version};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseTerminalIdentityError {
    PrefixMismatch { expected: &'static str },
    InvalidUuid,
    NonCanonical,
    UnsupportedUuidVersion,
}

impl Display for ParseTerminalIdentityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixMismatch { expected } => write!(formatter, "ID must start with {expected}"),
            Self::InvalidUuid => formatter.write_str("ID payload must be a UUID"),
            Self::NonCanonical => {
                formatter.write_str("ID UUID must use canonical lowercase hyphenated form")
            }
            Self::UnsupportedUuidVersion => formatter.write_str("ID payload must be a UUIDv7"),
        }
    }
}

impl Error for ParseTerminalIdentityError {}

fn generate_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::now_v7())
}

fn parse_id(value: &str, prefix: &'static str) -> Result<String, ParseTerminalIdentityError> {
    let payload = value
        .strip_prefix(prefix)
        .ok_or(ParseTerminalIdentityError::PrefixMismatch { expected: prefix })?;
    let uuid = Uuid::parse_str(payload).map_err(|_| ParseTerminalIdentityError::InvalidUuid)?;
    if payload != uuid.hyphenated().to_string() {
        return Err(ParseTerminalIdentityError::NonCanonical);
    }
    if uuid.get_version() != Some(Version::SortRand) {
        return Err(ParseTerminalIdentityError::UnsupportedUuidVersion);
    }
    Ok(value.to_owned())
}

macro_rules! define_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn generate() -> Self {
                Self(generate_id($prefix))
            }

            pub fn parse(value: &str) -> Result<Self, ParseTerminalIdentityError> {
                parse_id(value, $prefix).map(Self)
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
            type Err = ParseTerminalIdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
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
                Self::parse(&value).map_err(D::Error::custom)
            }
        }
    };
}

define_id!(TerminalId, "term_");
define_id!(TerminalRequestId, "term_req_");
define_id!(TerminalStreamId, "term_stream_");

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_V7: &str = "01890f17-4a00-7000-8000-000000000001";
    const UUID_V4: &str = "550e8400-e29b-41d4-a716-446655440000";

    macro_rules! assert_identity {
        ($type:ty, $prefix:literal) => {{
            let generated = <$type>::generate();
            assert!(generated.as_str().starts_with($prefix));
            assert_eq!(generated.as_str().parse::<$type>().unwrap(), generated);
            let encoded = serde_json::to_string(&generated).unwrap();
            assert_eq!(serde_json::from_str::<$type>(&encoded).unwrap(), generated);
            assert!(matches!(
                <$type>::parse(&format!("wrong_{UUID_V7}")),
                Err(ParseTerminalIdentityError::PrefixMismatch { .. })
            ));
            assert_eq!(
                <$type>::parse(&format!("{}{UUID_V4}", $prefix)),
                Err(ParseTerminalIdentityError::UnsupportedUuidVersion)
            );
        }};
    }

    #[test]
    fn identities_are_terminal_owned_canonical_uuid_v7_values() {
        assert_identity!(TerminalId, "term_");
        assert_identity!(TerminalRequestId, "term_req_");
        assert_identity!(TerminalStreamId, "term_stream_");
    }
}
