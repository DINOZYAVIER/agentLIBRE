use std::fmt::{self, Display, Formatter, Write as _};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::render_canonical_json;

macro_rules! digest_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub(crate) fn from_json(value: &Value) -> Self {
                let canonical = render_canonical_json(value);
                Self(sha256_bytes(canonical.as_bytes()))
            }

            pub fn parse(value: &str) -> Result<Self, DigestParseError> {
                let Some(hex) = value.strip_prefix("sha256:") else {
                    return Err(DigestParseError);
                };
                if hex.len() != 64
                    || !hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                {
                    return Err(DigestParseError);
                }
                Ok(Self(value.to_owned()))
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

fn sha256_bytes(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut value = String::with_capacity("sha256:".len() + digest.len() * 2);
    value.push_str("sha256:");
    for byte in digest {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestParseError;

impl Display for DigestParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("digest must be `sha256:` followed by 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for DigestParseError {}

digest_type!(DeclarationDigest);
digest_type!(PolicyHash);
