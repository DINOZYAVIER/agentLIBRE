//! Format-neutral contracts shared by typed agentLIBRE artifacts.
//!
//! This crate intentionally contains no package discovery or payload-specific
//! code.  It is the dependency leaf for the artifact layer.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

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
    #[error("invalid artifact relative path `{value}`")]
    InvalidRelativePath { value: String },
    #[error("invalid artifact entrypoint `{value}`")]
    InvalidEntrypoint { value: String },
    #[error("invalid artifact source ID `{value}`")]
    InvalidSourceId { value: String },
    #[error("artifact package file `{path}` is duplicated")]
    DuplicatePackageFile { path: String },
    #[error("artifact package file `{path}` was not found")]
    PackageFileNotFound { path: String },
    #[error("artifact package path `{path}` is not a regular file")]
    PackagePathNotRegular { path: String },
    #[error("artifact package path `{path}` is a symlink")]
    PackageSymlinkRejected { path: String },
    #[error("failed to inspect artifact package path `{path}`: {reason}")]
    PackageIo { path: String, reason: String },
    #[error(
        "artifact candidate version `{candidate}` disagrees with envelope version `{envelope}`"
    )]
    CandidateVersionMismatch { candidate: String, envelope: String },
    #[error("adapter `{type_id}` returned an envelope for `{actual_type}`")]
    AdapterTypeMismatch {
        type_id: String,
        actual_type: String,
    },
    #[error("adapter `{type_id}` returned an envelope for package `{actual_id}`")]
    AdapterPackageMismatch { type_id: String, actual_id: String },
    #[error("adapter `{type_id}` rejected package payload: {reason}")]
    AdapterPayload { type_id: String, reason: String },
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
            Self::InvalidRelativePath { .. } => "invalid_relative_path",
            Self::InvalidEntrypoint { .. } => "invalid_entrypoint",
            Self::InvalidSourceId { .. } => "invalid_source_id",
            Self::DuplicatePackageFile { .. } => "duplicate_package_file",
            Self::PackageFileNotFound { .. } => "package_file_not_found",
            Self::PackagePathNotRegular { .. } => "package_path_not_regular",
            Self::PackageSymlinkRejected { .. } => "package_symlink_rejected",
            Self::PackageIo { .. } => "package_io",
            Self::CandidateVersionMismatch { .. } => "candidate_version_mismatch",
            Self::AdapterTypeMismatch { .. } => "adapter_type_mismatch",
            Self::AdapterPackageMismatch { .. } => "adapter_package_mismatch",
            Self::AdapterPayload { .. } => "adapter_payload",
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactRelativePath(String);

impl ArtifactRelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        if !valid_relative_path(&value) {
            return Err(ArtifactError::InvalidRelativePath { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn as_path(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl Display for ArtifactRelativePath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactRelativePath {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactRelativePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactRelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactEntrypoint(ArtifactRelativePath);

impl ArtifactEntrypoint {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        if value.is_empty() || value == "." || value == ".." {
            return Err(ArtifactError::InvalidAdapterEntrypoint { value });
        }
        ArtifactRelativePath::new(value)
            .map(Self)
            .map_err(|error| match error {
                ArtifactError::InvalidRelativePath { value } => {
                    ArtifactError::InvalidAdapterEntrypoint { value }
                }
                other => other,
            })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn path(&self) -> &ArtifactRelativePath {
        &self.0
    }
}

impl Display for ArtifactEntrypoint {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactEntrypoint {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactEntrypoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactEntrypoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\\')
        && !value.contains(':')
        && !value.chars().any(|character| character.is_control())
        && value
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactAdapterDescriptor {
    pub type_id: ArtifactTypeId,
    pub root: String,
    pub entrypoint: ArtifactEntrypoint,
}

impl ArtifactAdapterDescriptor {
    pub fn new(
        type_id: ArtifactTypeId,
        root: impl Into<String>,
        entrypoint: ArtifactEntrypoint,
    ) -> Result<Self, ArtifactError> {
        let root = root.into();
        if !valid_root(&root) {
            return Err(ArtifactError::InvalidAdapterRoot { value: root });
        }
        Ok(Self {
            type_id,
            root,
            entrypoint,
        })
    }
}

pub trait ArtifactPackageView: Send + Sync {
    fn files(&self) -> Result<Vec<ArtifactRelativePath>, ArtifactError>;

    fn read_file(&self, path: &ArtifactRelativePath) -> Result<Vec<u8>, ArtifactError>;
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryPackageView {
    files: BTreeMap<ArtifactRelativePath, Vec<u8>>,
}

impl InMemoryPackageView {
    pub fn new(
        files: impl IntoIterator<Item = (ArtifactRelativePath, Vec<u8>)>,
    ) -> Result<Self, ArtifactError> {
        let mut view = Self::default();
        for (path, content) in files {
            view.insert(path, content)?;
        }
        Ok(view)
    }

    pub fn insert(
        &mut self,
        path: ArtifactRelativePath,
        content: Vec<u8>,
    ) -> Result<(), ArtifactError> {
        if self.files.insert(path.clone(), content).is_some() {
            return Err(ArtifactError::DuplicatePackageFile {
                path: path.to_string(),
            });
        }
        Ok(())
    }
}

impl ArtifactPackageView for InMemoryPackageView {
    fn files(&self) -> Result<Vec<ArtifactRelativePath>, ArtifactError> {
        Ok(self.files.keys().cloned().collect())
    }

    fn read_file(&self, path: &ArtifactRelativePath) -> Result<Vec<u8>, ArtifactError> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| ArtifactError::PackageFileNotFound {
                path: path.to_string(),
            })
    }
}

#[derive(Clone, Debug)]
pub struct DirectoryPackageView {
    root: PathBuf,
}

impl DirectoryPackageView {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root).map_err(|error| package_io(&root, error))?;
        if metadata.file_type().is_symlink() {
            return Err(ArtifactError::PackageSymlinkRejected {
                path: root.display().to_string(),
            });
        }
        if !metadata.is_dir() {
            return Err(ArtifactError::PackagePathNotRegular {
                path: root.display().to_string(),
            });
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn checked_path(&self, relative: &ArtifactRelativePath) -> Result<PathBuf, ArtifactError> {
        let mut current = self.root.clone();
        for component in relative.as_path().components() {
            let Component::Normal(segment) = component else {
                return Err(ArtifactError::InvalidRelativePath {
                    value: relative.to_string(),
                });
            };
            current.push(segment);
            let metadata =
                fs::symlink_metadata(&current).map_err(|error| package_io(&current, error))?;
            if metadata.file_type().is_symlink() {
                return Err(ArtifactError::PackageSymlinkRejected {
                    path: relative.to_string(),
                });
            }
        }
        Ok(current)
    }
}

impl ArtifactPackageView for DirectoryPackageView {
    fn files(&self) -> Result<Vec<ArtifactRelativePath>, ArtifactError> {
        let mut paths = Vec::new();
        collect_directory_files(&self.root, "", &mut paths)?;
        paths.sort();
        Ok(paths)
    }

    fn read_file(&self, path: &ArtifactRelativePath) -> Result<Vec<u8>, ArtifactError> {
        let absolute = self.checked_path(path)?;
        let metadata =
            fs::symlink_metadata(&absolute).map_err(|error| package_io(&absolute, error))?;
        if !metadata.is_file() {
            return Err(ArtifactError::PackagePathNotRegular {
                path: path.to_string(),
            });
        }
        fs::read(&absolute).map_err(|error| package_io(&absolute, error))
    }
}

fn collect_directory_files(
    directory: &Path,
    prefix: &str,
    paths: &mut Vec<ArtifactRelativePath>,
) -> Result<(), ArtifactError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| package_io(directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| package_io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ArtifactError::PackageIo {
                path: entry.path().display().to_string(),
                reason: "package file name is not valid UTF-8".to_owned(),
            })?;
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| package_io(&entry.path(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(ArtifactError::PackageSymlinkRejected { path: relative });
        }
        if metadata.is_dir() {
            collect_directory_files(&entry.path(), &relative, paths)?;
        } else if metadata.is_file() {
            paths.push(ArtifactRelativePath::new(relative)?);
        } else {
            return Err(ArtifactError::PackagePathNotRegular { path: relative });
        }
    }
    Ok(())
}

fn package_io(path: &Path, error: std::io::Error) -> ArtifactError {
    ArtifactError::PackageIo {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactSourceId(String);

impl ArtifactSourceId {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        if value.is_empty()
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || !value.as_bytes()[0].is_ascii_lowercase()
        {
            return Err(ArtifactError::InvalidSourceId { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ArtifactSourceId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactSourceId {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactSourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactSourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSourceTier {
    Explicit,
    Workspace,
    User,
    System,
    Builtin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSourceKind {
    Directory,
    Git,
    Embedded,
}

#[derive(Clone)]
pub struct ArtifactCandidate {
    pub type_id: ArtifactTypeId,
    pub package_id: ArtifactPackageId,
    pub version: ArtifactVersion,
    pub source_id: ArtifactSourceId,
    pub tier: ArtifactSourceTier,
    pub kind: ArtifactSourceKind,
    view: Arc<dyn ArtifactPackageView>,
}

impl fmt::Debug for ArtifactCandidate {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactCandidate")
            .field("type_id", &self.type_id)
            .field("package_id", &self.package_id)
            .field("version", &self.version)
            .field("source_id", &self.source_id)
            .field("tier", &self.tier)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl ArtifactCandidate {
    pub fn new(
        type_id: ArtifactTypeId,
        package_id: ArtifactPackageId,
        version: ArtifactVersion,
        source_id: ArtifactSourceId,
        tier: ArtifactSourceTier,
        kind: ArtifactSourceKind,
        view: Arc<dyn ArtifactPackageView>,
    ) -> Self {
        Self {
            type_id,
            package_id,
            version,
            source_id,
            tier,
            kind,
            view,
        }
    }

    pub fn view(&self) -> &dyn ArtifactPackageView {
        self.view.as_ref()
    }
}

pub trait ArtifactSource: Send + Sync {
    fn id(&self) -> &ArtifactSourceId;
    fn tier(&self) -> ArtifactSourceTier;
    fn kind(&self) -> ArtifactSourceKind;
    fn candidates(&self, type_id: &ArtifactTypeId)
    -> Result<Vec<ArtifactCandidate>, ArtifactError>;
}

pub type ErasedArtifactPayload = Box<dyn Any + Send + Sync>;

/// Adapter lifecycle completed in H02; payload implementations remain in domain crates.
pub trait ArtifactAdapter: Send + Sync {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor;

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError>;

    fn validate_payload(
        &self,
        package: &dyn ArtifactPackageView,
        envelope: &ArtifactEnvelope,
    ) -> Result<ErasedArtifactPayload, ArtifactError>;
}

impl<T> ArtifactAdapter for Arc<T>
where
    T: ArtifactAdapter + ?Sized,
{
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        self.as_ref().descriptor()
    }

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        self.as_ref().extract_envelope(package)
    }

    fn validate_payload(
        &self,
        package: &dyn ArtifactPackageView,
        envelope: &ArtifactEnvelope,
    ) -> Result<ErasedArtifactPayload, ArtifactError> {
        self.as_ref().validate_payload(package, envelope)
    }
}

#[derive(Clone, Default)]
pub struct ArtifactAdapterRegistry {
    adapters: BTreeMap<ArtifactTypeId, Arc<dyn ArtifactAdapter>>,
}

impl fmt::Debug for ArtifactAdapterRegistry {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactAdapterRegistry")
            .field("types", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ArtifactAdapterRegistry {
    pub fn new<A>(adapters: impl IntoIterator<Item = A>) -> Result<Self, ArtifactError>
    where
        A: ArtifactAdapter + 'static,
    {
        let mut registry = Self::default();
        for adapter in adapters {
            registry.insert(Arc::new(adapter))?;
        }
        Ok(registry)
    }

    pub fn from_adapters<A>(adapters: impl IntoIterator<Item = A>) -> Result<Self, ArtifactError>
    where
        A: ArtifactAdapter + 'static,
    {
        Self::new(adapters)
    }

    pub fn lookup(&self, type_id: &ArtifactTypeId) -> Result<&dyn ArtifactAdapter, ArtifactError> {
        self.adapters
            .get(type_id)
            .map(AsRef::as_ref)
            .ok_or_else(|| ArtifactError::UnsupportedType {
                type_id: type_id.to_string(),
            })
    }

    pub fn get(&self, type_id: &ArtifactTypeId) -> Option<&dyn ArtifactAdapter> {
        self.adapters.get(type_id).map(AsRef::as_ref)
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn ArtifactAdapter> {
        self.adapters.values().map(AsRef::as_ref)
    }

    fn insert(&mut self, adapter: Arc<dyn ArtifactAdapter>) -> Result<(), ArtifactError> {
        let descriptor = adapter.descriptor();
        if self.adapters.contains_key(&descriptor.type_id) {
            return Err(ArtifactError::DuplicateAdapterType {
                type_id: descriptor.type_id.to_string(),
            });
        }
        if self
            .adapters
            .values()
            .any(|existing| existing.descriptor().root == descriptor.root)
        {
            return Err(ArtifactError::DuplicateAdapterRoot {
                root: descriptor.root.clone(),
            });
        }
        if !descriptor.type_id.is_core() && RESERVED_ROOTS.contains(&descriptor.root.as_str()) {
            return Err(ArtifactError::ReservedRootCollision {
                root: descriptor.root.clone(),
            });
        }
        if let Some(expected) = core_root(descriptor.type_id.as_str())
            && expected != descriptor.root
        {
            return Err(ArtifactError::CoreRootMismatch {
                type_id: descriptor.type_id.to_string(),
                expected: expected.to_owned(),
                actual: descriptor.root.clone(),
            });
        }
        self.adapters.insert(descriptor.type_id.clone(), adapter);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct StaticArtifactSource {
    source_id: ArtifactSourceId,
    tier: ArtifactSourceTier,
    kind: ArtifactSourceKind,
    candidates: Vec<ArtifactCandidate>,
}

impl StaticArtifactSource {
    pub fn new(
        source_id: ArtifactSourceId,
        tier: ArtifactSourceTier,
        kind: ArtifactSourceKind,
        candidates: Vec<ArtifactCandidate>,
    ) -> Result<Self, ArtifactError> {
        for candidate in &candidates {
            if candidate.source_id != source_id || candidate.tier != tier || candidate.kind != kind
            {
                return Err(ArtifactError::PackageIo {
                    path: candidate.package_id.to_string(),
                    reason: "candidate provenance does not match source".to_owned(),
                });
            }
        }
        Ok(Self {
            source_id,
            tier,
            kind,
            candidates,
        })
    }
}

impl ArtifactSource for StaticArtifactSource {
    fn id(&self) -> &ArtifactSourceId {
        &self.source_id
    }

    fn tier(&self) -> ArtifactSourceTier {
        self.tier
    }

    fn kind(&self) -> ArtifactSourceKind {
        self.kind
    }

    fn candidates(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<Vec<ArtifactCandidate>, ArtifactError> {
        let mut candidates = self
            .candidates
            .iter()
            .filter(|candidate| &candidate.type_id == type_id)
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            (&left.package_id, &left.version, &left.source_id, left.kind).cmp(&(
                &right.package_id,
                &right.version,
                &right.source_id,
                right.kind,
            ))
        });
        Ok(candidates)
    }
}

pub struct DirectoryArtifactSource {
    source_id: ArtifactSourceId,
    tier: ArtifactSourceTier,
    kind: ArtifactSourceKind,
    root: PathBuf,
    registry: Arc<ArtifactAdapterRegistry>,
}

impl fmt::Debug for DirectoryArtifactSource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryArtifactSource")
            .field("source_id", &self.source_id)
            .field("tier", &self.tier)
            .field("kind", &self.kind)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl DirectoryArtifactSource {
    pub fn new(
        source_id: ArtifactSourceId,
        tier: ArtifactSourceTier,
        kind: ArtifactSourceKind,
        root: impl Into<PathBuf>,
        registry: Arc<ArtifactAdapterRegistry>,
    ) -> Self {
        Self {
            source_id,
            tier,
            kind,
            root: root.into(),
            registry,
        }
    }
}

impl ArtifactSource for DirectoryArtifactSource {
    fn id(&self) -> &ArtifactSourceId {
        &self.source_id
    }

    fn tier(&self) -> ArtifactSourceTier {
        self.tier
    }

    fn kind(&self) -> ArtifactSourceKind {
        self.kind
    }

    fn candidates(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<Vec<ArtifactCandidate>, ArtifactError> {
        let adapter = self.registry.lookup(type_id)?;
        let typed_root = self.root.join(&adapter.descriptor().root);
        match fs::symlink_metadata(&typed_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ArtifactError::PackageSymlinkRejected {
                    path: typed_root.display().to_string(),
                });
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(ArtifactError::PackagePathNotRegular {
                    path: typed_root.display().to_string(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(package_io(&typed_root, error)),
        }

        let package_root = DirectoryPackageView::new(&typed_root)?;
        let entrypoint = adapter.descriptor().entrypoint.to_string();
        let mut candidates = Vec::new();
        for file in package_root.files()? {
            let file_name = file.to_string();
            let prefix = if file_name == entrypoint {
                ""
            } else if let Some(prefix) = file_name.strip_suffix(&format!("/{entrypoint}")) {
                prefix
            } else {
                continue;
            };
            if prefix.is_empty() {
                continue;
            }
            let parts = prefix.split('/').collect::<Vec<_>>();
            let (package_text, declared_version, package_dir_text) =
                if let Some(version_text) = parts.last().copied() {
                    if let Ok(version) = ArtifactVersion::new(version_text) {
                        if parts.len() < 2 {
                            continue;
                        }
                        let package_text = parts[..parts.len() - 1].join("/");
                        (package_text, Some(version), prefix.to_owned())
                    } else {
                        (prefix.to_owned(), None, prefix.to_owned())
                    }
                } else {
                    continue;
                };
            let package_id = ArtifactPackageId::new(package_text)?;
            let view = DirectoryPackageView::new(typed_root.join(package_dir_text))?;
            let envelope = adapter.extract_envelope(&view)?;
            envelope.validate()?;
            if envelope.type_id != *type_id {
                return Err(ArtifactError::AdapterTypeMismatch {
                    type_id: type_id.to_string(),
                    actual_type: envelope.type_id.to_string(),
                });
            }
            if envelope.id != package_id {
                return Err(ArtifactError::AdapterPackageMismatch {
                    type_id: type_id.to_string(),
                    actual_id: envelope.id.to_string(),
                });
            }
            let version = if let Some(declared_version) = declared_version {
                if declared_version != envelope.version {
                    return Err(ArtifactError::CandidateVersionMismatch {
                        candidate: declared_version.to_string(),
                        envelope: envelope.version.to_string(),
                    });
                }
                declared_version
            } else {
                envelope.version
            };
            candidates.push(ArtifactCandidate::new(
                type_id.clone(),
                package_id,
                version,
                self.source_id.clone(),
                self.tier,
                self.kind,
                Arc::new(view),
            ));
        }
        candidates.sort_by(|left, right| {
            (&left.package_id, &left.version).cmp(&(&right.package_id, &right.version))
        });
        Ok(candidates)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactDataClass {
    Package,
    Config,
    State,
    Cache,
}

#[derive(Clone, Debug)]
pub struct ArtifactPathRouter {
    workspace_root: PathBuf,
    data_root: PathBuf,
    config_root: PathBuf,
    state_root: PathBuf,
    cache_root: PathBuf,
    registry: Arc<ArtifactAdapterRegistry>,
}

impl ArtifactPathRouter {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        data_root: impl Into<PathBuf>,
        config_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        cache_root: impl Into<PathBuf>,
        registry: Arc<ArtifactAdapterRegistry>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            data_root: data_root.into(),
            config_root: config_root.into(),
            state_root: state_root.into(),
            cache_root: cache_root.into(),
            registry,
        }
    }

    pub fn root(
        &self,
        scope: ArtifactPathScope,
        data_class: ArtifactDataClass,
        type_id: &ArtifactTypeId,
    ) -> Result<PathBuf, ArtifactError> {
        let type_root = self.registry.lookup(type_id)?.descriptor().root.clone();
        let root = match (scope, data_class) {
            (ArtifactPathScope::Workspace, ArtifactDataClass::Package) => {
                self.workspace_root.join(".agl")
            }
            (ArtifactPathScope::Workspace, ArtifactDataClass::Config) => {
                self.workspace_root.join(".agl/config")
            }
            (ArtifactPathScope::Workspace, ArtifactDataClass::State) => {
                self.workspace_root.join(".agl/state")
            }
            (ArtifactPathScope::Workspace, ArtifactDataClass::Cache) => {
                self.workspace_root.join(".agl/cache")
            }
            (ArtifactPathScope::Xdg, ArtifactDataClass::Package) => self.data_root.clone(),
            (ArtifactPathScope::Xdg, ArtifactDataClass::Config) => self.config_root.clone(),
            (ArtifactPathScope::Xdg, ArtifactDataClass::State) => self.state_root.clone(),
            (ArtifactPathScope::Xdg, ArtifactDataClass::Cache) => self.cache_root.clone(),
        };
        Ok(root.join(type_root))
    }

    pub fn workspace_package_root(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<PathBuf, ArtifactError> {
        self.root(
            ArtifactPathScope::Workspace,
            ArtifactDataClass::Package,
            type_id,
        )
    }

    pub fn xdg_package_root(&self, type_id: &ArtifactTypeId) -> Result<PathBuf, ArtifactError> {
        self.root(ArtifactPathScope::Xdg, ArtifactDataClass::Package, type_id)
    }

    pub fn workspace_config_root(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<PathBuf, ArtifactError> {
        self.root(
            ArtifactPathScope::Workspace,
            ArtifactDataClass::Config,
            type_id,
        )
    }

    pub fn xdg_config_root(&self, type_id: &ArtifactTypeId) -> Result<PathBuf, ArtifactError> {
        self.root(ArtifactPathScope::Xdg, ArtifactDataClass::Config, type_id)
    }

    pub fn workspace_package_path(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
        version: &ArtifactVersion,
    ) -> Result<PathBuf, ArtifactError> {
        Ok(append_package_version(
            self.workspace_package_root(type_id)?,
            package_id,
            version,
        ))
    }

    pub fn xdg_package_path(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
        version: &ArtifactVersion,
    ) -> Result<PathBuf, ArtifactError> {
        Ok(append_package_version(
            self.xdg_package_root(type_id)?,
            package_id,
            version,
        ))
    }

    pub fn workspace_config_path(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
    ) -> Result<PathBuf, ArtifactError> {
        Ok(append_package_id(
            self.workspace_config_root(type_id)?,
            package_id,
        ))
    }

    pub fn xdg_config_path(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
    ) -> Result<PathBuf, ArtifactError> {
        Ok(append_package_id(
            self.xdg_config_root(type_id)?,
            package_id,
        ))
    }

    pub fn state_root(&self, type_id: &ArtifactTypeId) -> Result<PathBuf, ArtifactError> {
        self.root(ArtifactPathScope::Xdg, ArtifactDataClass::State, type_id)
    }

    pub fn cache_root(&self, type_id: &ArtifactTypeId) -> Result<PathBuf, ArtifactError> {
        self.root(ArtifactPathScope::Xdg, ArtifactDataClass::Cache, type_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ArtifactPathScope {
    Workspace,
    Xdg,
}

fn append_package_id(mut root: PathBuf, package_id: &ArtifactPackageId) -> PathBuf {
    for segment in package_id.as_str().split('/') {
        root.push(segment);
    }
    root
}

fn append_package_version(
    root: PathBuf,
    package_id: &ArtifactPackageId,
    version: &ArtifactVersion,
) -> PathBuf {
    let mut path = append_package_id(root, package_id);
    path.push(version.to_string());
    path
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

    struct TestAdapter {
        descriptor: ArtifactAdapterDescriptor,
    }

    impl ArtifactAdapter for TestAdapter {
        fn descriptor(&self) -> &ArtifactAdapterDescriptor {
            &self.descriptor
        }

        fn extract_envelope(
            &self,
            _package: &dyn ArtifactPackageView,
        ) -> Result<ArtifactEnvelope, ArtifactError> {
            Err(ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "test adapter".to_owned(),
            })
        }

        fn validate_payload(
            &self,
            _package: &dyn ArtifactPackageView,
            _envelope: &ArtifactEnvelope,
        ) -> Result<ErasedArtifactPayload, ArtifactError> {
            Err(ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "test adapter".to_owned(),
            })
        }
    }

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
            "FUNCTION.md".parse().unwrap(),
        )
        .unwrap();
        let registry = ArtifactAdapterRegistry::new([TestAdapter {
            descriptor: function,
        }])
        .unwrap();
        assert_eq!(
            registry
                .lookup(&ArtifactTypeId::function())
                .unwrap()
                .descriptor()
                .root,
            FUNCTION_ROOT
        );
        assert!(matches!(
            registry.lookup(&ArtifactTypeId::skill()),
            Err(ArtifactError::UnsupportedType { .. })
        ));
        let wrong = ArtifactAdapterDescriptor::new(
            ArtifactTypeId::skill(),
            FUNCTION_ROOT,
            "SKILL.md".parse().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ArtifactAdapterRegistry::new([TestAdapter { descriptor: wrong }]),
            Err(ArtifactError::CoreRootMismatch { .. })
        ));
    }
}
