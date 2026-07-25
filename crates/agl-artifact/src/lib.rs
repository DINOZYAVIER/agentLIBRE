//! Format-neutral contracts shared by typed agentLIBRE artifacts.
//!
//! This crate intentionally contains no package discovery or payload-specific
//! code.  It is the dependency leaf for the artifact layer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use semver::{Version, VersionReq};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub const ARTIFACT_SCHEMA: &str = "agentlibre.artifact/v1";
pub const FUNCTION_TYPE: &str = "function";
pub const EXTENSION_TYPE: &str = "extension";
pub const SKILL_TYPE: &str = "skill";
pub const MODEL_TYPE: &str = "model";

pub const FUNCTION_ROOT: &str = "functions";
pub const EXTENSION_ROOT: &str = "extensions";
pub const SKILL_ROOT: &str = "skills";
pub const MODEL_ROOT: &str = "models";

const CORE_TYPES: [&str; 4] = [FUNCTION_TYPE, EXTENSION_TYPE, SKILL_TYPE, MODEL_TYPE];

const RESERVED_ROOTS: [&str; 4] = [FUNCTION_ROOT, EXTENSION_ROOT, SKILL_ROOT, MODEL_ROOT];

/// Errors returned by the public artifact contract.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ArtifactError {
    #[error("invalid artifact type ID `{value}`")]
    InvalidTypeId { value: String },
    #[error("invalid artifact package ID `{value}`")]
    InvalidPackageId { value: String },
    #[error("invalid artifact schema ID `{value}`")]
    InvalidSchemaId { value: String },
    #[error("invalid artifact version `{value}`: {reason}")]
    InvalidVersion { value: String, reason: String },
    #[error("invalid artifact version requirement `{value}`: {reason}")]
    InvalidVersionReq { value: String, reason: String },
    #[error("invalid artifact package reference `{value}`: {reason}")]
    InvalidReference { value: String, reason: String },
    #[error("artifact envelope must contain at least one tested AGL version")]
    EmptyTestedVersions,
    #[error("tested AGL version `{version}` is outside compatible range `{compatible}`")]
    TestedVersionIncompatible { version: String, compatible: String },
    #[error("artifact envelope uses unsupported schema `{value}`")]
    InvalidEnvelopeSchema { value: String },
    #[error("artifact payload schema must differ from `{common}`")]
    PayloadSchemaConflicts { common: String },
    #[error("duplicate artifact requirement target `{type_id}:{package_id}`")]
    DuplicateRequirement { type_id: String, package_id: String },
    #[error("invalid adapter root `{value}`")]
    InvalidAdapterRoot { value: String },
    #[error("invalid adapter entrypoint `{value}`")]
    InvalidAdapterEntrypoint { value: String },
    #[error("duplicate artifact adapter type `{type_id}`")]
    DuplicateAdapterType { type_id: String },
    #[error("duplicate artifact adapter root `{root}`")]
    DuplicateAdapterRoot { root: String },
    #[error("adapter root `{root}` is reserved")]
    ReservedRootCollision { root: String },
    #[error("core type `{type_id}` must use root `{expected}`, not `{actual}`")]
    CoreRootMismatch {
        type_id: String,
        expected: String,
        actual: String,
    },
    #[error("no adapter is registered for artifact type `{type_id}`")]
    UnsupportedType { type_id: String },
}

impl ArtifactError {
    /// Stable machine-readable code for diagnostics and CLI projections.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidTypeId { .. } => "invalid_type_id",
            Self::InvalidPackageId { .. } => "invalid_package_id",
            Self::InvalidSchemaId { .. } => "invalid_schema_id",
            Self::InvalidVersion { .. } => "invalid_version",
            Self::InvalidVersionReq { .. } => "invalid_version_requirement",
            Self::InvalidReference { .. } => "invalid_reference",
            Self::EmptyTestedVersions => "empty_tested_versions",
            Self::TestedVersionIncompatible { .. } => "tested_version_incompatible",
            Self::InvalidEnvelopeSchema { .. } => "invalid_envelope_schema",
            Self::PayloadSchemaConflicts { .. } => "payload_schema_conflicts",
            Self::DuplicateRequirement { .. } => "duplicate_requirement",
            Self::InvalidAdapterRoot { .. } => "invalid_adapter_root",
            Self::InvalidAdapterEntrypoint { .. } => "invalid_adapter_entrypoint",
            Self::DuplicateAdapterType { .. } => "duplicate_adapter_type",
            Self::DuplicateAdapterRoot { .. } => "duplicate_adapter_root",
            Self::ReservedRootCollision { .. } => "reserved_root_collision",
            Self::CoreRootMismatch { .. } => "core_root_mismatch",
            Self::UnsupportedType { .. } => "unsupported_type",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactTypeId(String);

impl ArtifactTypeId {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        if CORE_TYPES.contains(&value.as_str()) {
            return Ok(Self(value));
        }
        if value.starts_with("agentlibre.") || !valid_dotted_id(&value, 2) {
            return Err(ArtifactError::InvalidTypeId { value });
        }
        Ok(Self(value))
    }

    pub fn function() -> Self {
        Self(FUNCTION_TYPE.to_owned())
    }

    pub fn extension() -> Self {
        Self(EXTENSION_TYPE.to_owned())
    }

    pub fn skill() -> Self {
        Self(SKILL_TYPE.to_owned())
    }

    pub fn model() -> Self {
        Self(MODEL_TYPE.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_core(&self) -> bool {
        CORE_TYPES.contains(&self.as_str())
    }
}

impl Display for ArtifactTypeId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactTypeId {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactTypeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactTypeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactPackageId(String);

impl ArtifactPackageId {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        let valid = !value.is_empty()
            && !value.starts_with('/')
            && !value.ends_with('/')
            && !value.contains('\\')
            && !value.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || character == ':'
                    || character == '@'
            })
            && value.split('/').all(valid_package_segment);
        if !valid {
            return Err(ArtifactError::InvalidPackageId { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_package_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.as_bytes()[0].is_ascii_alphanumeric()
        && segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

impl Display for ArtifactPackageId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactPackageId {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactPackageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactPackageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactSchemaId(String);

impl ArtifactSchemaId {
    pub fn common() -> Self {
        Self(ARTIFACT_SCHEMA.to_owned())
    }

    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        let Some((namespace, revision)) = value.split_once('/') else {
            return Err(ArtifactError::InvalidSchemaId { value });
        };
        let valid_revision = revision.starts_with('v')
            && revision.len() > 1
            && revision[1..].bytes().all(|byte| byte.is_ascii_digit());
        if !valid_dotted_id(namespace, 1) || !valid_revision {
            return Err(ArtifactError::InvalidSchemaId { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ArtifactSchemaId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactSchemaId {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactSchemaId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactSchemaId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactVersion(Version);

impl ArtifactVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        Version::parse(&value)
            .map(Self)
            .map_err(|error| ArtifactError::InvalidVersion {
                value,
                reason: error.to_string(),
            })
    }

    pub fn as_semver(&self) -> &Version {
        &self.0
    }
}

impl Display for ArtifactVersion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ArtifactVersion {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArtifactVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ArtifactVersionReq(VersionReq);

impl ArtifactVersionReq {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        VersionReq::parse(&value)
            .map(Self)
            .map_err(|error| ArtifactError::InvalidVersionReq {
                value,
                reason: error.to_string(),
            })
    }

    pub fn matches(&self, version: &ArtifactVersion) -> bool {
        self.0.matches(version.as_semver())
    }

    pub fn as_semver(&self) -> &VersionReq {
        &self.0
    }
}

impl Display for ArtifactVersionReq {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ArtifactVersionReq {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactVersionReq {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArtifactVersionReq {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ArtifactPackageRef {
    pub type_id: ArtifactTypeId,
    pub package_id: ArtifactPackageId,
    pub version_req: ArtifactVersionReq,
}

impl ArtifactPackageRef {
    pub fn new(
        type_id: ArtifactTypeId,
        package_id: ArtifactPackageId,
        version_req: ArtifactVersionReq,
    ) -> Self {
        Self {
            type_id,
            package_id,
            version_req,
        }
    }

    pub fn parse(value: &str) -> Result<Self, ArtifactError> {
        let original = value.to_owned();
        let Some((type_text, remainder)) = value.split_once(':') else {
            return Err(ArtifactError::InvalidReference {
                value: original,
                reason: "missing `:` delimiter".to_owned(),
            });
        };
        let Some((package_text, requirement_text)) = remainder.rsplit_once('@') else {
            return Err(ArtifactError::InvalidReference {
                value: original,
                reason: "missing `@` delimiter".to_owned(),
            });
        };
        let type_id =
            ArtifactTypeId::new(type_text).map_err(|error| ArtifactError::InvalidReference {
                value: value.to_owned(),
                reason: error.to_string(),
            })?;
        let package_id = ArtifactPackageId::new(package_text).map_err(|error| {
            ArtifactError::InvalidReference {
                value: value.to_owned(),
                reason: error.to_string(),
            }
        })?;
        let version_req = ArtifactVersionReq::new(requirement_text).map_err(|error| {
            ArtifactError::InvalidReference {
                value: value.to_owned(),
                reason: error.to_string(),
            }
        })?;
        Ok(Self::new(type_id, package_id, version_req))
    }
}

impl Display for ArtifactPackageRef {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}@{}",
            self.type_id, self.package_id, self.version_req
        )
    }
}

impl FromStr for ArtifactPackageRef {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ArtifactPackageRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArtifactPackageRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ArtifactRequirement(ArtifactPackageRef);

impl ArtifactRequirement {
    pub fn new(reference: ArtifactPackageRef) -> Self {
        Self(reference)
    }

    pub fn parse(value: &str) -> Result<Self, ArtifactError> {
        ArtifactPackageRef::parse(value).map(Self)
    }

    pub fn reference(&self) -> &ArtifactPackageRef {
        &self.0
    }

    pub fn type_id(&self) -> &ArtifactTypeId {
        &self.0.type_id
    }

    pub fn package_id(&self) -> &ArtifactPackageId {
        &self.0.package_id
    }
}

impl Display for ArtifactRequirement {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ArtifactRequirement {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ArtifactRequirement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ArtifactRequirement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ArtifactPackageRef::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AglCompatibility {
    pub compatible: ArtifactVersionReq,
    pub tested: BTreeSet<ArtifactVersion>,
}

impl AglCompatibility {
    pub fn new(
        compatible: ArtifactVersionReq,
        tested: impl IntoIterator<Item = ArtifactVersion>,
    ) -> Result<Self, ArtifactError> {
        let compatibility = Self {
            compatible,
            tested: tested.into_iter().collect(),
        };
        compatibility.validate()?;
        Ok(compatibility)
    }

    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.tested.is_empty() {
            return Err(ArtifactError::EmptyTestedVersions);
        }
        if let Some(version) = self
            .tested
            .iter()
            .find(|version| !self.compatible.matches(version))
        {
            return Err(ArtifactError::TestedVersionIncompatible {
                version: version.to_string(),
                compatible: self.compatible.to_string(),
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for AglCompatibility {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            compatible: ArtifactVersionReq,
            tested: BTreeSet<ArtifactVersion>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.compatible, wire.tested).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEnvelope {
    pub schema: ArtifactSchemaId,
    #[serde(rename = "type")]
    pub type_id: ArtifactTypeId,
    pub id: ArtifactPackageId,
    pub version: ArtifactVersion,
    pub payload_schema: ArtifactSchemaId,
    pub agl: AglCompatibility,
    pub requires: Vec<ArtifactRequirement>,
}

impl ArtifactEnvelope {
    pub fn new(
        type_id: ArtifactTypeId,
        id: ArtifactPackageId,
        version: ArtifactVersion,
        payload_schema: ArtifactSchemaId,
        agl: AglCompatibility,
        requires: Vec<ArtifactRequirement>,
    ) -> Result<Self, ArtifactError> {
        let envelope = Self {
            schema: ArtifactSchemaId::common(),
            type_id,
            id,
            version,
            payload_schema,
            agl,
            requires,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.schema.as_str() != ARTIFACT_SCHEMA {
            return Err(ArtifactError::InvalidEnvelopeSchema {
                value: self.schema.to_string(),
            });
        }
        if self.payload_schema.as_str() == ARTIFACT_SCHEMA {
            return Err(ArtifactError::PayloadSchemaConflicts {
                common: ARTIFACT_SCHEMA.to_owned(),
            });
        }
        self.agl.validate()?;
        let mut targets = BTreeSet::new();
        for requirement in &self.requires {
            let target = (
                requirement.type_id().clone(),
                requirement.package_id().clone(),
            );
            if !targets.insert(target) {
                return Err(ArtifactError::DuplicateRequirement {
                    type_id: requirement.type_id().to_string(),
                    package_id: requirement.package_id().to_string(),
                });
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ArtifactEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: ArtifactSchemaId,
            #[serde(rename = "type")]
            type_id: ArtifactTypeId,
            id: ArtifactPackageId,
            version: ArtifactVersion,
            payload_schema: ArtifactSchemaId,
            agl: AglCompatibility,
            requires: Vec<ArtifactRequirement>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let envelope = Self {
            schema: wire.schema,
            type_id: wire.type_id,
            id: wire.id,
            version: wire.version,
            payload_schema: wire.payload_schema,
            agl: wire.agl,
            requires: wire.requires,
        };
        envelope.validate().map_err(D::Error::custom)?;
        Ok(envelope)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactAdapterDescriptor {
    pub type_id: ArtifactTypeId,
    pub root: String,
    pub entrypoint: String,
}

impl ArtifactAdapterDescriptor {
    pub fn new(
        type_id: ArtifactTypeId,
        root: impl Into<String>,
        entrypoint: impl Into<String>,
    ) -> Result<Self, ArtifactError> {
        let root = root.into();
        let entrypoint = entrypoint.into();
        if !valid_root(&root) {
            return Err(ArtifactError::InvalidAdapterRoot { value: root });
        }
        if !valid_entrypoint(&entrypoint) {
            return Err(ArtifactError::InvalidAdapterEntrypoint { value: entrypoint });
        }
        Ok(Self {
            type_id,
            root,
            entrypoint,
        })
    }
}

/// Minimal adapter boundary for H01. Payload reading and validation arrive in H02.
pub trait ArtifactAdapter: Send + Sync {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor;
}

impl ArtifactAdapter for ArtifactAdapterDescriptor {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct ArtifactAdapterRegistry {
    descriptors: BTreeMap<ArtifactTypeId, ArtifactAdapterDescriptor>,
}

impl ArtifactAdapterRegistry {
    pub fn new(
        descriptors: impl IntoIterator<Item = ArtifactAdapterDescriptor>,
    ) -> Result<Self, ArtifactError> {
        let mut registry = Self::default();
        for descriptor in descriptors {
            registry.insert(descriptor)?;
        }
        Ok(registry)
    }

    pub fn from_adapters<A>(adapters: impl IntoIterator<Item = A>) -> Result<Self, ArtifactError>
    where
        A: ArtifactAdapter,
    {
        Self::new(
            adapters
                .into_iter()
                .map(|adapter| adapter.descriptor().clone()),
        )
    }

    pub fn lookup(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<&ArtifactAdapterDescriptor, ArtifactError> {
        self.descriptors
            .get(type_id)
            .ok_or_else(|| ArtifactError::UnsupportedType {
                type_id: type_id.to_string(),
            })
    }

    pub fn get(&self, type_id: &ArtifactTypeId) -> Option<&ArtifactAdapterDescriptor> {
        self.descriptors.get(type_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ArtifactAdapterDescriptor> {
        self.descriptors.values()
    }

    fn insert(&mut self, descriptor: ArtifactAdapterDescriptor) -> Result<(), ArtifactError> {
        if self.descriptors.contains_key(&descriptor.type_id) {
            return Err(ArtifactError::DuplicateAdapterType {
                type_id: descriptor.type_id.to_string(),
            });
        }
        if self
            .descriptors
            .values()
            .any(|existing| existing.root == descriptor.root)
        {
            return Err(ArtifactError::DuplicateAdapterRoot {
                root: descriptor.root,
            });
        }
        if !descriptor.type_id.is_core() && RESERVED_ROOTS.contains(&descriptor.root.as_str()) {
            return Err(ArtifactError::ReservedRootCollision {
                root: descriptor.root,
            });
        }
        if let Some(expected) = core_root(descriptor.type_id.as_str())
            && expected != descriptor.root
        {
            return Err(ArtifactError::CoreRootMismatch {
                type_id: descriptor.type_id.to_string(),
                expected: expected.to_owned(),
                actual: descriptor.root,
            });
        }
        self.descriptors
            .insert(descriptor.type_id.clone(), descriptor);
        Ok(())
    }
}

fn valid_dotted_id(value: &str, minimum_segments: usize) -> bool {
    let segments: Vec<_> = value.split('.').collect();
    segments.len() >= minimum_segments
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.as_bytes()[0].is_ascii_lowercase()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn valid_root(value: &str) -> bool {
    !value.is_empty()
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_entrypoint(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('/')
        && !value.contains('/')
        && !value.contains('\\')
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

fn core_root(type_id: &str) -> Option<&'static str> {
    match type_id {
        FUNCTION_TYPE => Some(FUNCTION_ROOT),
        EXTENSION_TYPE => Some(EXTENSION_ROOT),
        SKILL_TYPE => Some(SKILL_ROOT),
        MODEL_TYPE => Some(MODEL_ROOT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> ArtifactVersion {
        value.parse().unwrap()
    }

    fn requirement(value: &str) -> ArtifactRequirement {
        value.parse().unwrap()
    }

    #[test]
    fn type_ids_enforce_core_and_custom_grammar() {
        assert!("function".parse::<ArtifactTypeId>().is_ok());
        assert!("vendor.workflow".parse::<ArtifactTypeId>().is_ok());
        for invalid in ["", "Vendor.workflow", "vendor", "agentlibre.custom", "a..b"] {
            assert!(invalid.parse::<ArtifactTypeId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn package_ids_reject_paths_and_delimiters() {
        for valid in ["example", "vendor/workflow.v2", "a-1/b_c"] {
            assert!(valid.parse::<ArtifactPackageId>().is_ok(), "{valid}");
        }
        for invalid in ["", "/absolute", "a/", "a//b", "a/../b", "a:b", "a@b", "A"] {
            assert!(invalid.parse::<ArtifactPackageId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn references_are_canonical() {
        let reference: ArtifactPackageRef = "skill:vendor/workflow@^1.0".parse().unwrap();
        assert_eq!(reference.to_string(), "skill:vendor/workflow@^1.0");
        for invalid in [
            "skill/vendor/workflow@^1",
            "skill:vendor/workflow",
            "skill:vendor@^1@x",
        ] {
            assert!(invalid.parse::<ArtifactPackageRef>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn compatibility_requires_tested_versions_inside_range() {
        let compatible: ArtifactVersionReq = ">=1.0.0, <2.0.0".parse().unwrap();
        assert!(AglCompatibility::new(compatible.clone(), [version("1.2.0")]).is_ok());
        assert_eq!(
            AglCompatibility::new(compatible, []).unwrap_err(),
            ArtifactError::EmptyTestedVersions
        );
        assert!(matches!(
            AglCompatibility::new(">=2.0.0".parse().unwrap(), [version("1.2.0")]),
            Err(ArtifactError::TestedVersionIncompatible { .. })
        ));
    }

    #[test]
    fn envelope_rejects_duplicate_requirement_targets() {
        let agl = AglCompatibility::new(">=1.0.0".parse().unwrap(), [version("1.0.0")]).unwrap();
        let result = ArtifactEnvelope::new(
            ArtifactTypeId::function(),
            "example".parse().unwrap(),
            version("1.0.0"),
            "agentlibre.function/v2".parse().unwrap(),
            agl,
            vec![
                requirement("skill:workflow@^1"),
                requirement("skill:workflow@^2"),
            ],
        );
        assert!(matches!(
            result,
            Err(ArtifactError::DuplicateRequirement { .. })
        ));
    }

    #[test]
    fn registry_checks_roots_and_unknown_types() {
        let function = ArtifactAdapterDescriptor::new(
            ArtifactTypeId::function(),
            FUNCTION_ROOT,
            "FUNCTION.md",
        )
        .unwrap();
        let registry = ArtifactAdapterRegistry::new([function]).unwrap();
        assert_eq!(
            registry.lookup(&ArtifactTypeId::function()).unwrap().root,
            FUNCTION_ROOT
        );
        assert!(matches!(
            registry.lookup(&ArtifactTypeId::skill()),
            Err(ArtifactError::UnsupportedType { .. })
        ));
        let wrong =
            ArtifactAdapterDescriptor::new(ArtifactTypeId::skill(), FUNCTION_ROOT, "SKILL.md")
                .unwrap();
        assert!(matches!(
            ArtifactAdapterRegistry::new([wrong]),
            Err(ArtifactError::CoreRootMismatch { .. })
        ));
    }
}
