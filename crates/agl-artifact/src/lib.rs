//! Format-neutral contracts shared by typed agentLIBRE artifacts.
//!
//! This crate intentionally contains no package discovery or payload-specific
//! code.  It is the dependency leaf for the artifact layer.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use semver::{Version, VersionReq};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as ShaDigest, Sha256};
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
    #[error("artifact source path escapes its workspace boundary: `{path}`")]
    PathEscape { path: String },
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
    #[error("adapter `{type_id}` rejected package envelope: {reason}")]
    AdapterEnvelope { type_id: String, reason: String },
    #[error("adapter `{type_id}` rejected package payload: {reason}")]
    AdapterPayload { type_id: String, reason: String },
    #[error("ambiguous candidate for `{type_id}:{package_id}@{version}`: {sources:?}")]
    AmbiguousCandidate {
        type_id: String,
        package_id: String,
        version: String,
        sources: Vec<String>,
    },
    #[error("artifact package `{type_id}:{package_id}` was not found")]
    PackageNotFound { type_id: String, package_id: String },
    #[error(
        "artifact package `{type_id}:{package_id}` has no version compatible with {requirements:?}; available versions: {available:?}"
    )]
    IncompatibleVersion {
        type_id: String,
        package_id: String,
        requirements: Vec<String>,
        available: Vec<String>,
    },
    #[error("missing artifact dependency `{reference}` from `{parent}`")]
    MissingDependency { parent: String, reference: String },
    #[error("artifact dependency cycle: {path:?}")]
    DependencyCycle { path: Vec<String> },
    #[error("artifact constraints conflict for `{key}`: `{requirement}`")]
    ConstraintConflict { key: String, requirement: String },
    #[error("invalid package tree digest `{value}`")]
    InvalidPackageDigest { value: String },
    #[error("reserved mutable/control-plane file `{path}` is inside the package")]
    ReservedPackageFile { path: String },
    #[error("artifact lock has unsupported version `{version}`")]
    UnsupportedLockVersion { version: u32 },
    #[error("artifact lock is missing package `{key}`")]
    LockMissingPackage { key: String },
    #[error(
        "artifact lock package `{key}` has drifted {field}: expected `{expected}`, got `{actual}`"
    )]
    LockDrift {
        key: String,
        field: String,
        expected: String,
        actual: String,
    },
    #[error("artifact lock package key `{key}` is invalid")]
    InvalidLockPackageKey { key: String },
    #[error("failed to write artifact lock `{path}`: {reason}")]
    LockIo { path: String, reason: String },
}

impl ArtifactError {
    /// Stable machine-readable code for diagnostics and CLI projections.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidTypeId { .. } => "invalid_type_id",
            Self::InvalidPackageId { .. } => "invalid_package_id",
            Self::InvalidVersion { .. } => "invalid_version",
            Self::InvalidVersionReq { .. } => "invalid_version_requirement",
            Self::InvalidReference { .. } => "invalid_reference",
            Self::InvalidSchemaId { .. }
            | Self::EmptyTestedVersions
            | Self::TestedVersionIncompatible { .. }
            | Self::InvalidEnvelopeSchema { .. }
            | Self::PayloadSchemaConflicts { .. }
            | Self::DuplicateRequirement { .. }
            | Self::CandidateVersionMismatch { .. }
            | Self::AdapterTypeMismatch { .. }
            | Self::AdapterPackageMismatch { .. }
            | Self::AdapterEnvelope { .. } => "invalid_envelope",
            Self::InvalidAdapterRoot { .. } => "invalid_adapter_root",
            Self::InvalidAdapterEntrypoint { .. } => "invalid_adapter_entrypoint",
            Self::DuplicateAdapterType { .. } => "duplicate_adapter_type",
            Self::DuplicateAdapterRoot { .. } => "duplicate_adapter_root",
            Self::ReservedRootCollision { .. } => "reserved_root_collision",
            Self::CoreRootMismatch { .. } => "core_root_mismatch",
            Self::UnsupportedType { .. } => "unsupported_type",
            Self::InvalidRelativePath { .. } => "path_escape",
            Self::InvalidEntrypoint { .. } => "invalid_entrypoint",
            Self::InvalidSourceId { .. } => "invalid_source_id",
            Self::DuplicatePackageFile { .. } => "duplicate_package_file",
            Self::PackageFileNotFound { .. } => "package_file_not_found",
            Self::PackagePathNotRegular { .. } => "package_path_not_regular",
            Self::PackageSymlinkRejected { .. } | Self::PathEscape { .. } => "path_escape",
            Self::PackageIo { .. } => "package_io",
            Self::AdapterPayload { .. } => "invalid_payload",
            Self::AmbiguousCandidate { .. } => "ambiguous_candidate",
            Self::PackageNotFound { .. } | Self::MissingDependency { .. } => "not_found",
            Self::IncompatibleVersion { .. } => "incompatible_version",
            Self::DependencyCycle { .. } => "dependency_cycle",
            Self::ConstraintConflict { .. } => "incompatible_version",
            Self::InvalidPackageDigest { .. } => "invalid_package_digest",
            Self::ReservedPackageFile { .. } => "reserved_package_file",
            Self::UnsupportedLockVersion { .. } => "lock_stale",
            Self::LockMissingPackage { .. } => "lock_missing",
            Self::LockDrift { field, .. } if field == "package_digest" => "digest_drift",
            Self::LockDrift { field, .. }
                if field == "source_revision" || field == "source_tree" =>
            {
                "source_drift"
            }
            Self::LockDrift { .. } => "lock_stale",
            Self::InvalidLockPackageKey { .. } | Self::LockIo { .. } => "lock_stale",
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceComponentKind {
    Git,
    Submodule,
    Local,
    Generated,
    Ignored,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAccess {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSourceDeclaration {
    pub kind: ArtifactSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

impl ArtifactSourceDeclaration {
    pub fn validate(&self) -> Result<(), ArtifactError> {
        if let Some(path) = &self.path {
            validate_workspace_relative_path(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceComponent {
    pub kind: WorkspaceComponentKind,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    #[serde(default)]
    pub required: bool,
    pub access: ArtifactAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub create: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePolicy {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfigReferences {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overlays: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    pub version: u32,
    pub default_function: ArtifactPackageRef,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, ArtifactSourceDeclaration>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub components: BTreeMap<String, WorkspaceComponent>,
    #[serde(default)]
    pub policy: WorkspacePolicy,
    #[serde(default)]
    pub config: WorkspaceConfigReferences,
}

impl WorkspaceManifest {
    pub const VERSION: u32 = 2;

    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.version != Self::VERSION {
            return Err(ArtifactError::UnsupportedLockVersion {
                version: self.version,
            });
        }
        if self.default_function.type_id.as_str() != FUNCTION_TYPE {
            return Err(ArtifactError::InvalidReference {
                value: self.default_function.to_string(),
                reason: "default_function must reference a function".to_owned(),
            });
        }
        for (name, source) in &self.sources {
            ArtifactSourceId::new(name.clone())?;
            source.validate()?;
        }
        for component in self.components.values() {
            validate_workspace_relative_path(&component.path)?;
            for create in &component.create {
                validate_workspace_create_path(create)?;
            }
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String, ArtifactError> {
        self.validate()?;
        toml::to_string(self).map_err(|error| ArtifactError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })
    }

    pub fn from_toml(value: &str) -> Result<Self, ArtifactError> {
        let manifest: Self = toml::from_str(value).map_err(|error| ArtifactError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })?;
        manifest.validate()?;
        Ok(manifest)
    }
}

fn validate_workspace_relative_path(path: &Path) -> Result<(), ArtifactError> {
    let value = path
        .to_str()
        .ok_or_else(|| ArtifactError::InvalidRelativePath {
            value: path.display().to_string(),
        })?;
    ArtifactRelativePath::new(value.to_owned()).map(|_| ())
}

fn validate_workspace_create_path(path: &Path) -> Result<(), ArtifactError> {
    if path == Path::new(".") {
        return Ok(());
    }
    validate_workspace_relative_path(path)
}

#[derive(Clone)]
pub struct ArtifactCandidate {
    pub type_id: ArtifactTypeId,
    pub package_id: ArtifactPackageId,
    pub version: ArtifactVersion,
    pub source_id: ArtifactSourceId,
    pub tier: ArtifactSourceTier,
    pub kind: ArtifactSourceKind,
    pub source_revision: Option<String>,
    pub source_tree: Option<String>,
    pub package_root: Option<PathBuf>,
    discovery_error: Option<ArtifactError>,
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
            .field("package_root", &self.package_root)
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
            source_revision: None,
            source_tree: None,
            package_root: None,
            discovery_error: None,
            view,
        }
    }

    pub fn with_source_provenance(
        mut self,
        source_revision: impl Into<String>,
        source_tree: impl Into<String>,
    ) -> Self {
        self.source_revision = Some(source_revision.into());
        self.source_tree = Some(source_tree.into());
        self
    }

    pub fn with_package_root(mut self, package_root: impl Into<PathBuf>) -> Self {
        self.package_root = Some(package_root.into());
        self
    }

    fn with_discovery_error(mut self, error: ArtifactError) -> Self {
        self.discovery_error = Some(error);
        self
    }

    pub fn discovery_error(&self) -> Option<&ArtifactError> {
        self.discovery_error.as_ref()
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

    fn inventory_candidates(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<Vec<ArtifactCandidate>, ArtifactError> {
        self.candidates(type_id)
    }
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

    pub fn from_dyn(
        adapters: impl IntoIterator<Item = Arc<dyn ArtifactAdapter>>,
    ) -> Result<Self, ArtifactError> {
        let mut registry = Self::default();
        for adapter in adapters {
            registry.insert(adapter)?;
        }
        Ok(registry)
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
    source_revision: Option<String>,
    source_tree: Option<String>,
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
            source_revision: None,
            source_tree: None,
        }
    }

    pub fn with_source_provenance(
        mut self,
        source_revision: impl Into<String>,
        source_tree: impl Into<String>,
    ) -> Self {
        self.source_revision = Some(source_revision.into());
        self.source_tree = Some(source_tree.into());
        self
    }

    fn scan_candidates(
        &self,
        type_id: &ArtifactTypeId,
        preserve_invalid_envelopes: bool,
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
            let package_root = typed_root.join(package_dir_text);
            let view = DirectoryPackageView::new(&package_root)?;
            let validated_version = (|| {
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
                if let Some(declared_version) = &declared_version {
                    if declared_version != &envelope.version {
                        return Err(ArtifactError::CandidateVersionMismatch {
                            candidate: declared_version.to_string(),
                            envelope: envelope.version.to_string(),
                        });
                    }
                    Ok(declared_version.clone())
                } else {
                    Ok(envelope.version)
                }
            })();
            let (version, discovery_error) = match validated_version {
                Ok(version) => (version, None),
                Err(error) if preserve_invalid_envelopes => (
                    declared_version.clone().unwrap_or_else(|| {
                        ArtifactVersion::new("0.0.0-invalid")
                            .expect("invalid-envelope inventory version is valid SemVer")
                    }),
                    Some(error),
                ),
                Err(error) => return Err(error),
            };
            let mut candidate = ArtifactCandidate::new(
                type_id.clone(),
                package_id,
                version,
                self.source_id.clone(),
                self.tier,
                self.kind,
                Arc::new(view),
            )
            .with_package_root(package_root);
            if let Some(error) = discovery_error {
                candidate = candidate.with_discovery_error(error);
            }
            if let (Some(revision), Some(tree)) = (&self.source_revision, &self.source_tree) {
                candidate = candidate.with_source_provenance(revision.clone(), tree.clone());
            }
            candidates.push(candidate);
        }
        candidates.sort_by(|left, right| {
            (&left.package_id, &left.version).cmp(&(&right.package_id, &right.version))
        });
        Ok(candidates)
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
        self.scan_candidates(type_id, false)
    }

    fn inventory_candidates(
        &self,
        type_id: &ArtifactTypeId,
    ) -> Result<Vec<ArtifactCandidate>, ArtifactError> {
        self.scan_candidates(type_id, true)
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

    pub fn config_layers(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
    ) -> Result<Vec<ArtifactConfigEvidence>, ArtifactError> {
        let user = self.xdg_config_path(type_id, package_id)?;
        let workspace = self.workspace_config_path(type_id, package_id)?;
        Ok(vec![
            ArtifactConfigEvidence {
                layer: ArtifactConfigLayer::PackageDefaults,
                path: None,
                present: true,
            },
            ArtifactConfigEvidence {
                layer: ArtifactConfigLayer::User,
                path: Some(user.clone()),
                present: user.exists(),
            },
            ArtifactConfigEvidence {
                layer: ArtifactConfigLayer::Workspace,
                path: Some(workspace.clone()),
                present: workspace.exists(),
            },
        ])
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactConfigLayer {
    PackageDefaults,
    User,
    Workspace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactConfigEvidence {
    pub layer: ArtifactConfigLayer,
    pub path: Option<PathBuf>,
    pub present: bool,
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageTreeDigest(String);

impl PackageTreeDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactError> {
        let value = value.into();
        let valid = value.len() == 71
            && value.starts_with("sha256:")
            && value[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if !valid {
            return Err(ArtifactError::InvalidPackageDigest { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PackageTreeDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageTreeDigest {
    type Err = ArtifactError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageTreeDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageTreeDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub fn compute_package_digest(
    view: &dyn ArtifactPackageView,
) -> Result<PackageTreeDigest, ArtifactError> {
    let mut files = view.files()?;
    files.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.package-tree.v1\0");
    let mut previous = None;
    for path in files {
        if previous.as_ref() == Some(&path) {
            return Err(ArtifactError::DuplicatePackageFile {
                path: path.to_string(),
            });
        }
        if reserved_package_file(&path) {
            return Err(ArtifactError::ReservedPackageFile {
                path: path.to_string(),
            });
        }
        let bytes = view.read_file(&path)?;
        hasher.update((path.as_str().len() as u64).to_be_bytes());
        hasher.update(path.as_str().as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
        previous = Some(path);
    }
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    PackageTreeDigest::new(format!("sha256:{hex}"))
}

fn reserved_package_file(path: &ArtifactRelativePath) -> bool {
    let value = path.as_str();
    value.starts_with(".agl/")
        || value == "workspace.toml"
        || value == "artifact-lock.toml"
        || value.starts_with("config/")
        || value.starts_with("state/")
        || value.starts_with("cache/")
        || value.contains("/source-index")
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedWorkspaceComponent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<WorkspaceComponentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<ArtifactSourceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<ArtifactSourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedArtifactPackage {
    pub type_id: ArtifactTypeId,
    pub id: ArtifactPackageId,
    pub version: ArtifactVersion,
    pub source_tier: ArtifactSourceTier,
    pub source_kind: ArtifactSourceKind,
    pub source_id: ArtifactSourceId,
    pub package_digest: PackageTreeDigest,
    pub envelope_schema: ArtifactSchemaId,
    pub payload_schema: ArtifactSchemaId,
    #[serde(default)]
    pub source_revision: Option<String>,
    #[serde(default)]
    pub source_tree: Option<String>,
    pub dependencies: Vec<String>,
}

impl LockedArtifactPackage {
    pub fn key(&self) -> String {
        format!("{}:{}@{}", self.type_id, self.id, self.version)
    }

    fn validate(&self, key: &str) -> Result<(), ArtifactError> {
        if self.key() != key {
            return Err(ArtifactError::InvalidLockPackageKey {
                key: key.to_owned(),
            });
        }
        let mut sorted = self.dependencies.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != self.dependencies {
            return Err(ArtifactError::LockDrift {
                key: key.to_owned(),
                field: "dependencies".to_owned(),
                expected: sorted.join(","),
                actual: self.dependencies.join(","),
            });
        }
        if self.source_kind == ArtifactSourceKind::Git {
            for (field, value) in [
                ("source_revision", self.source_revision.as_deref()),
                ("source_tree", self.source_tree.as_deref()),
            ] {
                if value.is_none_or(str::is_empty) {
                    return Err(ArtifactError::LockDrift {
                        key: key.to_owned(),
                        field: field.to_owned(),
                        expected: "present".to_owned(),
                        actual: "missing".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLock {
    pub version: u32,
    #[serde(default)]
    pub components: BTreeMap<String, LockedWorkspaceComponent>,
    #[serde(default)]
    pub packages: BTreeMap<String, LockedArtifactPackage>,
}

impl ArtifactLock {
    pub const VERSION: u32 = 2;

    pub fn new(
        components: BTreeMap<String, LockedWorkspaceComponent>,
        packages: BTreeMap<String, LockedArtifactPackage>,
    ) -> Result<Self, ArtifactError> {
        let lock = Self {
            version: Self::VERSION,
            components,
            packages,
        };
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<(), ArtifactError> {
        if self.version != Self::VERSION {
            return Err(ArtifactError::UnsupportedLockVersion {
                version: self.version,
            });
        }
        for (key, component) in &self.components {
            for (field, present) in [
                ("kind", component.kind.is_some()),
                ("path", component.path.is_some()),
                ("definition_digest", component.definition_digest.is_some()),
            ] {
                if !present {
                    return Err(ArtifactError::LockDrift {
                        key: key.clone(),
                        field: field.to_owned(),
                        expected: "present".to_owned(),
                        actual: "missing".to_owned(),
                    });
                }
            }
        }
        for (key, package) in &self.packages {
            package.validate(key)?;
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String, ArtifactError> {
        self.validate()?;
        toml::to_string(self).map_err(|error| ArtifactError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })
    }

    pub fn from_toml(value: &str) -> Result<Self, ArtifactError> {
        let lock: Self = toml::from_str(value).map_err(|error| ArtifactError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn write_atomic(&self, path: impl AsRef<Path>) -> Result<(), ArtifactError> {
        let path = path.as_ref();
        let content = self.to_toml()?;
        let temporary = path.with_extension("toml.tmp");
        let result = (|| {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(ArtifactError::LockIo {
                path: path.display().to_string(),
                reason: error.to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    pub candidate: ArtifactCandidate,
    pub envelope: ArtifactEnvelope,
    pub package_digest: PackageTreeDigest,
    pub dependencies: Vec<String>,
}

impl ResolvedArtifact {
    pub fn key(&self) -> String {
        format!(
            "{}:{}@{}",
            self.candidate.type_id, self.candidate.package_id, self.candidate.version
        )
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedArtifactGraph {
    pub root: String,
    pub nodes: BTreeMap<String, ResolvedArtifact>,
}

impl ResolvedArtifactGraph {
    pub fn package_lock_entries(
        &self,
    ) -> Result<BTreeMap<String, LockedArtifactPackage>, ArtifactError> {
        let mut packages = BTreeMap::new();
        for node in self.nodes.values() {
            let locked = LockedArtifactPackage {
                type_id: node.candidate.type_id.clone(),
                id: node.candidate.package_id.clone(),
                version: node.candidate.version.clone(),
                source_tier: node.candidate.tier,
                source_kind: node.candidate.kind,
                source_id: node.candidate.source_id.clone(),
                package_digest: node.package_digest.clone(),
                envelope_schema: node.envelope.schema.clone(),
                payload_schema: node.envelope.payload_schema.clone(),
                source_revision: node.candidate.source_revision.clone(),
                source_tree: node.candidate.source_tree.clone(),
                dependencies: node.dependencies.clone(),
            };
            packages.insert(locked.key(), locked);
        }
        for (key, package) in &packages {
            package.validate(key)?;
        }
        Ok(packages)
    }

    pub fn lock(&self) -> Result<ArtifactLock, ArtifactError> {
        ArtifactLock::new(BTreeMap::new(), self.package_lock_entries()?)
    }

    pub fn verify_lock(&self, lock: &ArtifactLock) -> Result<(), ArtifactError> {
        lock.validate()?;
        for node in self.nodes.values() {
            let key = node.key();
            let Some(locked) = lock.packages.get(&key) else {
                return Err(ArtifactError::LockMissingPackage { key });
            };
            compare_lock_field(
                &key,
                "package_digest",
                locked.package_digest.to_string(),
                node.package_digest.to_string(),
            )?;
            compare_lock_field(
                &key,
                "source_tier",
                source_tier_name(locked.source_tier).to_owned(),
                source_tier_name(node.candidate.tier).to_owned(),
            )?;
            compare_lock_field(
                &key,
                "source_kind",
                source_kind_name(locked.source_kind).to_owned(),
                source_kind_name(node.candidate.kind).to_owned(),
            )?;
            compare_lock_field(
                &key,
                "source_id",
                locked.source_id.to_string(),
                node.candidate.source_id.to_string(),
            )?;
            compare_lock_field(
                &key,
                "source_revision",
                locked.source_revision.clone().unwrap_or_default(),
                node.candidate.source_revision.clone().unwrap_or_default(),
            )?;
            compare_lock_field(
                &key,
                "source_tree",
                locked.source_tree.clone().unwrap_or_default(),
                node.candidate.source_tree.clone().unwrap_or_default(),
            )?;
            compare_lock_field(
                &key,
                "envelope_schema",
                locked.envelope_schema.to_string(),
                node.envelope.schema.to_string(),
            )?;
            compare_lock_field(
                &key,
                "payload_schema",
                locked.payload_schema.to_string(),
                node.envelope.payload_schema.to_string(),
            )?;
            compare_lock_field(
                &key,
                "dependencies",
                locked.dependencies.join(","),
                node.dependencies.join(","),
            )?;
        }
        Ok(())
    }

    pub fn validate_payloads(
        &self,
        registry: &ArtifactAdapterRegistry,
    ) -> Result<(), ArtifactError> {
        for node in self.nodes.values() {
            let adapter = registry.lookup(&node.candidate.type_id)?;
            adapter.validate_payload(node.candidate.view(), &node.envelope)?;
        }
        Ok(())
    }
}

fn compare_lock_field(
    key: &str,
    field: &str,
    expected: String,
    actual: String,
) -> Result<(), ArtifactError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ArtifactError::LockDrift {
            key: key.to_owned(),
            field: field.to_owned(),
            expected,
            actual,
        })
    }
}

fn source_tier_name(value: ArtifactSourceTier) -> &'static str {
    match value {
        ArtifactSourceTier::Explicit => "explicit",
        ArtifactSourceTier::Workspace => "workspace",
        ArtifactSourceTier::User => "user",
        ArtifactSourceTier::System => "system",
        ArtifactSourceTier::Builtin => "builtin",
    }
}

fn source_kind_name(value: ArtifactSourceKind) -> &'static str {
    match value {
        ArtifactSourceKind::Directory => "directory",
        ArtifactSourceKind::Git => "git",
        ArtifactSourceKind::Embedded => "embedded",
    }
}

#[derive(Clone)]
pub struct ArtifactResolver {
    registry: Arc<ArtifactAdapterRegistry>,
    sources: Vec<Arc<dyn ArtifactSource>>,
}

impl fmt::Debug for ArtifactResolver {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactResolver")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

impl ArtifactResolver {
    pub fn new(
        registry: Arc<ArtifactAdapterRegistry>,
        sources: Vec<Arc<dyn ArtifactSource>>,
    ) -> Self {
        Self { registry, sources }
    }

    pub fn resolve(
        &self,
        root: &ArtifactPackageRef,
    ) -> Result<ResolvedArtifactGraph, ArtifactError> {
        let mut nodes = BTreeMap::new();
        let mut stack = Vec::new();
        let root_key = self.visit(
            &root.type_id,
            &root.package_id,
            std::slice::from_ref(&root.version_req),
            &mut stack,
            &mut nodes,
        )?;
        Ok(ResolvedArtifactGraph {
            root: root_key,
            nodes,
        })
    }

    pub fn resolve_with_lock(
        &self,
        root: &ArtifactPackageRef,
        lock: &ArtifactLock,
    ) -> Result<ResolvedArtifactGraph, ArtifactError> {
        let graph = self.resolve(root)?;
        graph.verify_lock(lock)?;
        Ok(graph)
    }

    pub fn resolve_and_validate(
        &self,
        root: &ArtifactPackageRef,
        lock: Option<&ArtifactLock>,
    ) -> Result<ResolvedArtifactGraph, ArtifactError> {
        let graph = self.resolve(root)?;
        if let Some(lock) = lock {
            graph.verify_lock(lock)?;
        }
        graph.validate_payloads(&self.registry)?;
        Ok(graph)
    }

    fn visit(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
        constraints: &[ArtifactVersionReq],
        stack: &mut Vec<String>,
        nodes: &mut BTreeMap<String, ResolvedArtifact>,
    ) -> Result<String, ArtifactError> {
        let identity = format!("{}:{}", type_id, package_id);
        if let Some(position) = stack.iter().position(|entry| entry == &identity) {
            let mut cycle = stack[position..].to_vec();
            cycle.push(identity);
            return Err(ArtifactError::DependencyCycle { path: cycle });
        }
        if let Some(existing) = nodes.values().find(|node| {
            node.candidate.type_id == *type_id && node.candidate.package_id == *package_id
        }) {
            for constraint in constraints {
                if !constraint.matches(&existing.candidate.version) {
                    return Err(ArtifactError::ConstraintConflict {
                        key: existing.key(),
                        requirement: constraint.to_string(),
                    });
                }
            }
            return Ok(existing.key());
        }

        let candidate = self.select_candidate(type_id, package_id, constraints)?;
        let adapter = self.registry.lookup(type_id)?;
        let envelope = adapter.extract_envelope(candidate.view())?;
        envelope.validate()?;
        if envelope.type_id != *type_id {
            return Err(ArtifactError::AdapterTypeMismatch {
                type_id: type_id.to_string(),
                actual_type: envelope.type_id.to_string(),
            });
        }
        if envelope.id != *package_id {
            return Err(ArtifactError::AdapterPackageMismatch {
                type_id: type_id.to_string(),
                actual_id: envelope.id.to_string(),
            });
        }
        if envelope.version != candidate.version {
            return Err(ArtifactError::CandidateVersionMismatch {
                candidate: candidate.version.to_string(),
                envelope: envelope.version.to_string(),
            });
        }
        let package_digest = compute_package_digest(candidate.view())?;
        let key = format!("{}:{}@{}", type_id, package_id, candidate.version);
        stack.push(identity);
        let mut dependencies = Vec::new();
        for requirement in &envelope.requires {
            let dependency_key = self
                .visit(
                    requirement.type_id(),
                    requirement.package_id(),
                    std::slice::from_ref(&requirement.reference().version_req),
                    stack,
                    nodes,
                )
                .map_err(|error| match error {
                    ArtifactError::PackageNotFound { .. } => ArtifactError::MissingDependency {
                        parent: key.clone(),
                        reference: requirement.to_string(),
                    },
                    other => other,
                })?;
            dependencies.push(dependency_key);
        }
        stack.pop();
        dependencies.sort();
        let node = ResolvedArtifact {
            candidate,
            envelope,
            package_digest,
            dependencies,
        };
        nodes.insert(key.clone(), node);
        Ok(key)
    }

    fn select_candidate(
        &self,
        type_id: &ArtifactTypeId,
        package_id: &ArtifactPackageId,
        constraints: &[ArtifactVersionReq],
    ) -> Result<ArtifactCandidate, ArtifactError> {
        let mut sources = self.sources.clone();
        sources.sort_by_key(|source| (source.tier(), source.id().clone()));
        let explicit = sources
            .iter()
            .any(|source| source.tier() == ArtifactSourceTier::Explicit);
        let tiers = [
            ArtifactSourceTier::Explicit,
            ArtifactSourceTier::Workspace,
            ArtifactSourceTier::User,
            ArtifactSourceTier::System,
            ArtifactSourceTier::Builtin,
        ];
        let mut available = BTreeSet::new();
        for tier in tiers {
            if explicit && tier != ArtifactSourceTier::Explicit {
                break;
            }
            let mut candidates = Vec::new();
            for source in sources.iter().filter(|source| source.tier() == tier) {
                for candidate in source.candidates(type_id)? {
                    if &candidate.package_id != package_id {
                        continue;
                    }
                    available.insert(candidate.version.to_string());
                    if constraints
                        .iter()
                        .all(|constraint| constraint.matches(&candidate.version))
                    {
                        candidates.push(candidate);
                    }
                }
            }
            if candidates.is_empty() {
                continue;
            }
            let version = candidates
                .iter()
                .map(|candidate| &candidate.version)
                .max()
                .expect("non-empty candidates")
                .clone();
            let matching = candidates
                .into_iter()
                .filter(|candidate| candidate.version == version)
                .collect::<Vec<_>>();
            if matching.len() > 1 {
                return Err(ArtifactError::AmbiguousCandidate {
                    type_id: type_id.to_string(),
                    package_id: package_id.to_string(),
                    version: version.to_string(),
                    sources: matching
                        .iter()
                        .map(|candidate| candidate.source_id.to_string())
                        .collect(),
                });
            }
            return Ok(matching.into_iter().next().expect("one candidate"));
        }
        if available.is_empty() {
            Err(ArtifactError::PackageNotFound {
                type_id: type_id.to_string(),
                package_id: package_id.to_string(),
            })
        } else {
            Err(ArtifactError::IncompatibleVersion {
                type_id: type_id.to_string(),
                package_id: package_id.to_string(),
                requirements: constraints.iter().map(ToString::to_string).collect(),
                available: available.into_iter().collect(),
            })
        }
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
