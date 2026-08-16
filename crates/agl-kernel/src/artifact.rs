use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;

use crate::{DeclarationError, EffectId, ExtensionId};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactIdError> {
        let value = value.into();
        let Some((owner, local)) = value.split_once(':') else {
            return Err(ArtifactIdError { value });
        };
        let valid_local = !local.is_empty()
            && local.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            });
        if value.matches(':').count() != 1 || ExtensionId::new(owner).is_err() || !valid_local {
            return Err(ArtifactIdError { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn owner(&self) -> ExtensionId {
        ExtensionId::new(
            self.0
                .split_once(':')
                .expect("validated ArtifactId is owner-qualified")
                .0,
        )
        .expect("validated ArtifactId owner is an ExtensionId")
    }
}

impl Display for ArtifactId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactId {
    type Err = ArtifactIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactIdError {
    value: String,
}

impl ArtifactIdError {
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl Display for ArtifactIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Artifact ID must be `<extension-id>:<local-id>` using lowercase ASCII: {}",
            self.value
        )
    }
}

impl std::error::Error for ArtifactIdError {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactKindId(String);

impl ArtifactKindId {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactKindIdError> {
        let value = value.into();
        let segments = value.split('.').collect::<Vec<_>>();
        let valid = segments.len() >= 2
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && segment.as_bytes()[0].is_ascii_lowercase()
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'-' | b'_')
                    })
            });
        if !valid {
            return Err(ArtifactKindIdError { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ArtifactKindId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactKindId {
    type Err = ArtifactKindIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactKindId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactKindId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactKindIdError {
    value: String,
}

impl Display for ArtifactKindIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid dot-separated Artifact kind ID: {}",
            self.value
        )
    }
}

impl std::error::Error for ArtifactKindIdError {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAccess {
    ReadTree,
    MutateTree,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDeclaration {
    pub id: ArtifactId,
    pub kind: ArtifactKindId,
    pub access: BTreeSet<ArtifactAccess>,
}

impl ArtifactDeclaration {
    pub fn new(
        id: ArtifactId,
        kind: ArtifactKindId,
        access: impl IntoIterator<Item = ArtifactAccess>,
    ) -> Result<Self, DeclarationError> {
        let access = access.into_iter().collect::<BTreeSet<_>>();
        if access.is_empty() {
            return Err(DeclarationError::ArtifactAccessEmpty { artifact_id: id });
        }
        Ok(Self { id, kind, access })
    }

    pub fn permits(&self, access: ArtifactAccess) -> bool {
        self.access.contains(&access)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactTargetSelector {
    Fixed(ArtifactId),
    FromArgument {
        pointer: String,
        access: ArtifactAccess,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEffectLink {
    pub effect_id: EffectId,
    pub selector: ArtifactTargetSelector,
    pub access: ArtifactAccess,
}

impl ArtifactEffectLink {
    pub fn new(
        effect_id: EffectId,
        selector: ArtifactTargetSelector,
        access: ArtifactAccess,
    ) -> Self {
        Self {
            effect_id,
            selector,
            access,
        }
    }

    pub fn resolve(&self, arguments: &Value) -> Result<ArtifactId, DeclarationError> {
        match &self.selector {
            ArtifactTargetSelector::Fixed(id) => Ok(id.clone()),
            ArtifactTargetSelector::FromArgument { pointer, access } => {
                if access != &self.access || !pointer.starts_with('/') {
                    return Err(DeclarationError::InvalidArtifactSelector {
                        pointer: pointer.clone(),
                    });
                }
                let value = arguments
                    .pointer(pointer)
                    .and_then(Value::as_str)
                    .ok_or_else(|| DeclarationError::InvalidArtifactSelector {
                        pointer: pointer.clone(),
                    })?;
                ArtifactId::new(value).map_err(|_| DeclarationError::InvalidArtifactTarget {
                    value: value.to_owned(),
                })
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionRequirement {
    pub extension_id: ExtensionId,
    pub api_major: u32,
}

impl ExtensionRequirement {
    pub fn new(extension_id: ExtensionId, api_major: u32) -> Self {
        Self {
            extension_id,
            api_major,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedArtifactTarget {
    pub effect_id: EffectId,
    pub artifact_id: ArtifactId,
    pub access: ArtifactAccess,
}
