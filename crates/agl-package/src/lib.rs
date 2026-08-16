//! Format-neutral APIs shared by versioned agentLIBRE packages.
//!
//! This crate intentionally contains no package discovery or payload-specific
//! code. It is the dependency leaf for package composition.

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

pub const PACKAGE_SCHEMA: &str = "agentlibre.package/v1";
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

/// Errors returned by the public package API.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PackageError {
    #[error("invalid package type ID `{value}`")]
    InvalidTypeId { value: String },
    #[error("invalid package ID `{value}`")]
    InvalidPackageId { value: String },
    #[error("invalid package schema ID `{value}`")]
    InvalidSchemaId { value: String },
    #[error("invalid package version `{value}`: {reason}")]
    InvalidVersion { value: String, reason: String },
    #[error("invalid package version requirement `{value}`: {reason}")]
    InvalidVersionReq { value: String, reason: String },
    #[error("invalid package reference `{value}`: {reason}")]
    InvalidReference { value: String, reason: String },
    #[error("package envelope must contain at least one tested AGL version")]
    EmptyTestedVersions,
    #[error("tested AGL version `{version}` is outside compatible range `{compatible}`")]
    TestedVersionIncompatible { version: String, compatible: String },
    #[error("package envelope uses unsupported schema `{value}`")]
    InvalidEnvelopeSchema { value: String },
    #[error("package payload schema must differ from `{common}`")]
    PayloadSchemaConflicts { common: String },
    #[error("duplicate package requirement target `{type_id}:{package_id}`")]
    DuplicateRequirement { type_id: String, package_id: String },
    #[error("invalid adapter root `{value}`")]
    InvalidAdapterRoot { value: String },
    #[error("invalid adapter entrypoint `{value}`")]
    InvalidAdapterEntrypoint { value: String },
    #[error("duplicate package adapter type `{type_id}`")]
    DuplicateAdapterType { type_id: String },
    #[error("duplicate package adapter root `{root}`")]
    DuplicateAdapterRoot { root: String },
    #[error("adapter root `{root}` is reserved")]
    ReservedRootCollision { root: String },
    #[error("core type `{type_id}` must use root `{expected}`, not `{actual}`")]
    CoreRootMismatch {
        type_id: String,
        expected: String,
        actual: String,
    },
    #[error("no adapter is registered for package type `{type_id}`")]
    UnsupportedType { type_id: String },
    #[error("invalid package relative path `{value}`")]
    InvalidRelativePath { value: String },
    #[error("invalid package entrypoint `{value}`")]
    InvalidEntrypoint { value: String },
    #[error("invalid package source ID `{value}`")]
    InvalidSourceId { value: String },
    #[error("invalid package source input `{source_id}`: {reason}")]
    InvalidSourceInput { source_id: String, reason: String },
    #[error("package file `{path}` is duplicated")]
    DuplicatePackageFile { path: String },
    #[error("package file `{path}` was not found")]
    PackageFileNotFound { path: String },
    #[error("package path `{path}` is not a regular file")]
    PackagePathNotRegular { path: String },
    #[error("package path `{path}` is a symlink")]
    PackageSymlinkRejected { path: String },
    #[error("package source path escapes its workspace boundary: `{path}`")]
    PathEscape { path: String },
    #[error("failed to inspect package path `{path}`: {reason}")]
    PackageIo { path: String, reason: String },
    #[error("package candidate version `{candidate}` disagrees with envelope version `{envelope}`")]
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
    #[error("package `{type_id}:{package_id}` was not found")]
    PackageNotFound { type_id: String, package_id: String },
    #[error(
        "package `{type_id}:{package_id}` has no version compatible with {requirements:?}; available versions: {available:?}"
    )]
    IncompatibleVersion {
        type_id: String,
        package_id: String,
        requirements: Vec<String>,
        available: Vec<String>,
    },
    #[error("missing package dependency `{reference}` from `{parent}`")]
    MissingDependency { parent: String, reference: String },
    #[error("package dependency cycle: {path:?}")]
    DependencyCycle { path: Vec<String> },
    #[error("package constraints conflict for `{key}`: `{requirement}`")]
    ConstraintConflict { key: String, requirement: String },
    #[error("invalid package tree digest `{value}`")]
    InvalidPackageDigest { value: String },
    #[error("reserved mutable/control-plane file `{path}` is inside the package")]
    ReservedPackageFile { path: String },
    #[error("package lock has unsupported version `{version}`")]
    UnsupportedLockVersion { version: u32 },
    #[error("package lock is missing package `{key}`")]
    LockMissingPackage { key: String },
    #[error(
        "package lock entry `{key}` has drifted {field}: expected `{expected}`, got `{actual}`"
    )]
    LockDrift {
        key: String,
        field: String,
        expected: String,
        actual: String,
    },
    #[error("package lock key `{key}` is invalid")]
    InvalidLockPackageKey { key: String },
    #[error("failed to write package lock `{path}`: {reason}")]
    LockIo { path: String, reason: String },
}

impl PackageError {
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
            Self::InvalidSourceInput { .. } => "invalid_source_input",
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
            Self::LockDrift { field, .. } if field == "package_tree_digest" => "digest_drift",
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
pub struct PackageTypeId(String);

impl PackageTypeId {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        if CORE_TYPES.contains(&value.as_str()) {
            return Ok(Self(value));
        }
        if value.starts_with("agentlibre.") || !valid_dotted_id(&value, 2) {
            return Err(PackageError::InvalidTypeId { value });
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

impl Display for PackageTypeId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageTypeId {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageTypeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageTypeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageId(String);

impl PackageId {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
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
            return Err(PackageError::InvalidPackageId { value });
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

impl Display for PackageId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageId {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageSchemaId(String);

impl PackageSchemaId {
    pub fn common() -> Self {
        Self(PACKAGE_SCHEMA.to_owned())
    }

    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        let Some((namespace, revision)) = value.split_once('/') else {
            return Err(PackageError::InvalidSchemaId { value });
        };
        let valid_revision = revision.starts_with('v')
            && revision.len() > 1
            && revision[1..].bytes().all(|byte| byte.is_ascii_digit());
        if !valid_dotted_id(namespace, 1) || !valid_revision {
            return Err(PackageError::InvalidSchemaId { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PackageSchemaId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageSchemaId {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageSchemaId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageSchemaId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageVersion(Version);

impl PackageVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        Version::parse(&value)
            .map(Self)
            .map_err(|error| PackageError::InvalidVersion {
                value,
                reason: error.to_string(),
            })
    }

    pub fn as_semver(&self) -> &Version {
        &self.0
    }
}

impl Display for PackageVersion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PackageVersion {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PackageVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PackageVersionReq(VersionReq);

impl PackageVersionReq {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        VersionReq::parse(&value)
            .map(Self)
            .map_err(|error| PackageError::InvalidVersionReq {
                value,
                reason: error.to_string(),
            })
    }

    pub fn matches(&self, version: &PackageVersion) -> bool {
        self.0.matches(version.as_semver())
    }

    pub fn as_semver(&self) -> &VersionReq {
        &self.0
    }
}

impl Display for PackageVersionReq {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PackageVersionReq {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageVersionReq {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PackageVersionReq {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PackageRef {
    pub type_id: PackageTypeId,
    pub package_id: PackageId,
    pub version_req: PackageVersionReq,
}

impl PackageRef {
    pub fn new(
        type_id: PackageTypeId,
        package_id: PackageId,
        version_req: PackageVersionReq,
    ) -> Self {
        Self {
            type_id,
            package_id,
            version_req,
        }
    }

    pub fn parse(value: &str) -> Result<Self, PackageError> {
        let original = value.to_owned();
        let Some((type_text, remainder)) = value.split_once(':') else {
            return Err(PackageError::InvalidReference {
                value: original,
                reason: "missing `:` delimiter".to_owned(),
            });
        };
        let Some((package_text, requirement_text)) = remainder.rsplit_once('@') else {
            return Err(PackageError::InvalidReference {
                value: original,
                reason: "missing `@` delimiter".to_owned(),
            });
        };
        let type_id =
            PackageTypeId::new(type_text).map_err(|error| PackageError::InvalidReference {
                value: value.to_owned(),
                reason: error.to_string(),
            })?;
        let package_id =
            PackageId::new(package_text).map_err(|error| PackageError::InvalidReference {
                value: value.to_owned(),
                reason: error.to_string(),
            })?;
        let version_req = PackageVersionReq::new(requirement_text).map_err(|error| {
            PackageError::InvalidReference {
                value: value.to_owned(),
                reason: error.to_string(),
            }
        })?;
        Ok(Self::new(type_id, package_id, version_req))
    }
}

impl Display for PackageRef {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}@{}",
            self.type_id, self.package_id, self.version_req
        )
    }
}

impl FromStr for PackageRef {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PackageRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PackageRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PackageRequirement(PackageRef);

impl PackageRequirement {
    pub fn new(reference: PackageRef) -> Self {
        Self(reference)
    }

    pub fn parse(value: &str) -> Result<Self, PackageError> {
        PackageRef::parse(value).map(Self)
    }

    pub fn reference(&self) -> &PackageRef {
        &self.0
    }

    pub fn type_id(&self) -> &PackageTypeId {
        &self.0.type_id
    }

    pub fn package_id(&self) -> &PackageId {
        &self.0.package_id
    }
}

impl Display for PackageRequirement {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PackageRequirement {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PackageRequirement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PackageRequirement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        PackageRef::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AglCompatibility {
    pub compatible: PackageVersionReq,
    pub tested: BTreeSet<PackageVersion>,
}

impl AglCompatibility {
    pub fn new(
        compatible: PackageVersionReq,
        tested: impl IntoIterator<Item = PackageVersion>,
    ) -> Result<Self, PackageError> {
        let compatibility = Self {
            compatible,
            tested: tested.into_iter().collect(),
        };
        compatibility.validate()?;
        Ok(compatibility)
    }

    pub fn validate(&self) -> Result<(), PackageError> {
        if self.tested.is_empty() {
            return Err(PackageError::EmptyTestedVersions);
        }
        if let Some(version) = self
            .tested
            .iter()
            .find(|version| !self.compatible.matches(version))
        {
            return Err(PackageError::TestedVersionIncompatible {
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
            compatible: PackageVersionReq,
            tested: BTreeSet<PackageVersion>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.compatible, wire.tested).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageEnvelope {
    pub schema: PackageSchemaId,
    #[serde(rename = "type")]
    pub type_id: PackageTypeId,
    pub id: PackageId,
    pub version: PackageVersion,
    pub payload_schema: PackageSchemaId,
    pub agl: AglCompatibility,
    pub requires: Vec<PackageRequirement>,
}

impl PackageEnvelope {
    pub fn new(
        type_id: PackageTypeId,
        id: PackageId,
        version: PackageVersion,
        payload_schema: PackageSchemaId,
        agl: AglCompatibility,
        requires: Vec<PackageRequirement>,
    ) -> Result<Self, PackageError> {
        let envelope = Self {
            schema: PackageSchemaId::common(),
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

    pub fn validate(&self) -> Result<(), PackageError> {
        if self.schema.as_str() != PACKAGE_SCHEMA {
            return Err(PackageError::InvalidEnvelopeSchema {
                value: self.schema.to_string(),
            });
        }
        if self.payload_schema.as_str() == PACKAGE_SCHEMA {
            return Err(PackageError::PayloadSchemaConflicts {
                common: PACKAGE_SCHEMA.to_owned(),
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
                return Err(PackageError::DuplicateRequirement {
                    type_id: requirement.type_id().to_string(),
                    package_id: requirement.package_id().to_string(),
                });
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for PackageEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: PackageSchemaId,
            #[serde(rename = "type")]
            type_id: PackageTypeId,
            id: PackageId,
            version: PackageVersion,
            payload_schema: PackageSchemaId,
            agl: AglCompatibility,
            requires: Vec<PackageRequirement>,
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
pub struct PackageRelativePath(String);

impl PackageRelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        if !valid_relative_path(&value) {
            return Err(PackageError::InvalidRelativePath { value });
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

impl Display for PackageRelativePath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageRelativePath {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageRelativePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageRelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageEntrypoint(PackageRelativePath);

impl PackageEntrypoint {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        if value.is_empty() || value == "." || value == ".." {
            return Err(PackageError::InvalidAdapterEntrypoint { value });
        }
        PackageRelativePath::new(value)
            .map(Self)
            .map_err(|error| match error {
                PackageError::InvalidRelativePath { value } => {
                    PackageError::InvalidAdapterEntrypoint { value }
                }
                other => other,
            })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn path(&self) -> &PackageRelativePath {
        &self.0
    }
}

impl Display for PackageEntrypoint {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageEntrypoint {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageEntrypoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageEntrypoint {
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
pub struct PackageAdapterDescriptor {
    pub type_id: PackageTypeId,
    pub root: String,
    pub entrypoint: PackageEntrypoint,
}

impl PackageAdapterDescriptor {
    pub fn new(
        type_id: PackageTypeId,
        root: impl Into<String>,
        entrypoint: PackageEntrypoint,
    ) -> Result<Self, PackageError> {
        let root = root.into();
        if !valid_root(&root) {
            return Err(PackageError::InvalidAdapterRoot { value: root });
        }
        Ok(Self {
            type_id,
            root,
            entrypoint,
        })
    }
}

pub trait PackageView: Send + Sync {
    fn files(&self) -> Result<Vec<PackageRelativePath>, PackageError>;

    fn read_file(&self, path: &PackageRelativePath) -> Result<Vec<u8>, PackageError>;
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryPackageView {
    files: BTreeMap<PackageRelativePath, Vec<u8>>,
}

impl InMemoryPackageView {
    pub fn new(
        files: impl IntoIterator<Item = (PackageRelativePath, Vec<u8>)>,
    ) -> Result<Self, PackageError> {
        let mut view = Self::default();
        for (path, content) in files {
            view.insert(path, content)?;
        }
        Ok(view)
    }

    pub fn insert(
        &mut self,
        path: PackageRelativePath,
        content: Vec<u8>,
    ) -> Result<(), PackageError> {
        if self.files.insert(path.clone(), content).is_some() {
            return Err(PackageError::DuplicatePackageFile {
                path: path.to_string(),
            });
        }
        Ok(())
    }
}

impl PackageView for InMemoryPackageView {
    fn files(&self) -> Result<Vec<PackageRelativePath>, PackageError> {
        Ok(self.files.keys().cloned().collect())
    }

    fn read_file(&self, path: &PackageRelativePath) -> Result<Vec<u8>, PackageError> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| PackageError::PackageFileNotFound {
                path: path.to_string(),
            })
    }
}

#[derive(Clone, Debug)]
pub struct DirectoryPackageView {
    root: PathBuf,
}

impl DirectoryPackageView {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PackageError> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root).map_err(|error| package_io(&root, error))?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::PackageSymlinkRejected {
                path: root.display().to_string(),
            });
        }
        if !metadata.is_dir() {
            return Err(PackageError::PackagePathNotRegular {
                path: root.display().to_string(),
            });
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn checked_path(&self, relative: &PackageRelativePath) -> Result<PathBuf, PackageError> {
        let mut current = self.root.clone();
        for component in relative.as_path().components() {
            let Component::Normal(segment) = component else {
                return Err(PackageError::InvalidRelativePath {
                    value: relative.to_string(),
                });
            };
            current.push(segment);
            let metadata =
                fs::symlink_metadata(&current).map_err(|error| package_io(&current, error))?;
            if metadata.file_type().is_symlink() {
                return Err(PackageError::PackageSymlinkRejected {
                    path: relative.to_string(),
                });
            }
        }
        Ok(current)
    }
}

impl PackageView for DirectoryPackageView {
    fn files(&self) -> Result<Vec<PackageRelativePath>, PackageError> {
        let mut paths = Vec::new();
        collect_directory_files(&self.root, "", &mut paths)?;
        paths.sort();
        Ok(paths)
    }

    fn read_file(&self, path: &PackageRelativePath) -> Result<Vec<u8>, PackageError> {
        let absolute = self.checked_path(path)?;
        let metadata =
            fs::symlink_metadata(&absolute).map_err(|error| package_io(&absolute, error))?;
        if !metadata.is_file() {
            return Err(PackageError::PackagePathNotRegular {
                path: path.to_string(),
            });
        }
        fs::read(&absolute).map_err(|error| package_io(&absolute, error))
    }
}

fn collect_directory_files(
    directory: &Path,
    prefix: &str,
    paths: &mut Vec<PackageRelativePath>,
) -> Result<(), PackageError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| package_io(directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| package_io(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| PackageError::PackageIo {
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
            return Err(PackageError::PackageSymlinkRejected { path: relative });
        }
        if metadata.is_dir() {
            collect_directory_files(&entry.path(), &relative, paths)?;
        } else if metadata.is_file() {
            paths.push(PackageRelativePath::new(relative)?);
        } else {
            return Err(PackageError::PackagePathNotRegular { path: relative });
        }
    }
    Ok(())
}

fn package_io(path: &Path, error: std::io::Error) -> PackageError {
    PackageError::PackageIo {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageSourceId(String);

impl PackageSourceId {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        if value.is_empty()
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            || !value.as_bytes()[0].is_ascii_lowercase()
        {
            return Err(PackageError::InvalidSourceId { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PackageSourceId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PackageSourceId {
    type Err = PackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PackageSourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PackageSourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSourceTier {
    Explicit,
    Workspace,
    User,
    System,
    Builtin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSourceKind {
    Directory,
    Git,
    Embedded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSourceDeclaration {
    pub id: PackageSourceId,
    pub tier: PackageSourceTier,
    pub kind: PackageSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageSourceProvenance {
    revision: String,
    tree: String,
}

impl PackageSourceProvenance {
    pub fn new(revision: impl Into<String>, tree: impl Into<String>) -> Self {
        Self {
            revision: revision.into(),
            tree: tree.into(),
        }
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn tree(&self) -> &str {
        &self.tree
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageSourceInput {
    id: PackageSourceId,
    tier: PackageSourceTier,
    kind: PackageSourceKind,
    root: PathBuf,
    provenance: Option<PackageSourceProvenance>,
}

impl PackageSourceInput {
    pub fn new(
        id: PackageSourceId,
        tier: PackageSourceTier,
        kind: PackageSourceKind,
        root: impl Into<PathBuf>,
        provenance: Option<PackageSourceProvenance>,
    ) -> Result<Self, PackageError> {
        let source_id = id.to_string();
        if provenance
            .as_ref()
            .is_some_and(|value| value.revision.trim().is_empty() || value.tree.trim().is_empty())
        {
            return Err(PackageError::InvalidSourceInput {
                source_id,
                reason: "Git provenance revision and tree must be non-empty".to_owned(),
            });
        }
        match (kind, provenance.is_some()) {
            (PackageSourceKind::Directory, false) | (PackageSourceKind::Git, true) => {}
            (PackageSourceKind::Directory, true) => {
                return Err(PackageError::InvalidSourceInput {
                    source_id,
                    reason: "Directory source must not carry Git provenance".to_owned(),
                });
            }
            (PackageSourceKind::Git, false) => {
                return Err(PackageError::InvalidSourceInput {
                    source_id,
                    reason: "Git source requires verified revision and tree provenance".to_owned(),
                });
            }
            (PackageSourceKind::Embedded, _) => {
                return Err(PackageError::InvalidSourceInput {
                    source_id,
                    reason: "Embedded source is not a materialized workspace input".to_owned(),
                });
            }
        }
        Ok(Self {
            id,
            tier,
            kind,
            root: root.into(),
            provenance,
        })
    }

    pub fn id(&self) -> &PackageSourceId {
        &self.id
    }

    pub fn tier(&self) -> PackageSourceTier {
        self.tier
    }

    pub fn kind(&self) -> PackageSourceKind {
        self.kind
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn provenance(&self) -> Option<&PackageSourceProvenance> {
        self.provenance.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageCompositionInput {
    workspace_root: PathBuf,
    sources: Vec<PackageSourceInput>,
}

impl PackageCompositionInput {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        sources: impl IntoIterator<Item = PackageSourceInput>,
    ) -> Result<Self, PackageError> {
        let sources = sources.into_iter().collect::<Vec<_>>();
        let mut ids = BTreeSet::new();
        for source in &sources {
            if !ids.insert(source.id.clone()) {
                return Err(PackageError::InvalidSourceId {
                    value: source.id.to_string(),
                });
            }
        }
        Ok(Self {
            workspace_root: workspace_root.into(),
            sources,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn sources(&self) -> &[PackageSourceInput] {
        &self.sources
    }
}

impl PackageSourceDeclaration {
    pub fn validate(&self) -> Result<(), PackageError> {
        if let Some(path) = &self.path {
            validate_workspace_relative_path(path)?;
        }
        Ok(())
    }
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
    pub default_function: PackageRef,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PackageSourceDeclaration>,
    #[serde(default)]
    pub policy: WorkspacePolicy,
    #[serde(default)]
    pub config: WorkspaceConfigReferences,
}

impl WorkspaceManifest {
    pub const VERSION: u32 = 3;

    pub fn validate(&self) -> Result<(), PackageError> {
        if self.version != Self::VERSION {
            return Err(PackageError::UnsupportedLockVersion {
                version: self.version,
            });
        }
        if self.default_function.type_id.as_str() != FUNCTION_TYPE {
            return Err(PackageError::InvalidReference {
                value: self.default_function.to_string(),
                reason: "default_function must reference a function".to_owned(),
            });
        }
        let mut source_ids = BTreeSet::new();
        for source in &self.sources {
            if !source_ids.insert(source.id.clone()) {
                return Err(PackageError::InvalidSourceId {
                    value: source.id.to_string(),
                });
            }
            source.validate()?;
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String, PackageError> {
        self.validate()?;
        toml::to_string(self).map_err(|error| PackageError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })
    }

    pub fn from_toml(value: &str) -> Result<Self, PackageError> {
        let manifest: Self = toml::from_str(value).map_err(|error| PackageError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })?;
        manifest.validate()?;
        Ok(manifest)
    }
}

fn validate_workspace_relative_path(path: &Path) -> Result<(), PackageError> {
    let value = path
        .to_str()
        .ok_or_else(|| PackageError::InvalidRelativePath {
            value: path.display().to_string(),
        })?;
    PackageRelativePath::new(value.to_owned()).map(|_| ())
}

#[derive(Clone)]
pub struct PackageCandidate {
    pub type_id: PackageTypeId,
    pub package_id: PackageId,
    pub version: PackageVersion,
    pub source_id: PackageSourceId,
    pub tier: PackageSourceTier,
    pub kind: PackageSourceKind,
    pub source_revision: Option<String>,
    pub source_tree: Option<String>,
    pub package_root: Option<PathBuf>,
    discovery_error: Option<PackageError>,
    view: Arc<dyn PackageView>,
}

impl fmt::Debug for PackageCandidate {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackageCandidate")
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

impl PackageCandidate {
    pub fn new(
        type_id: PackageTypeId,
        package_id: PackageId,
        version: PackageVersion,
        source_id: PackageSourceId,
        tier: PackageSourceTier,
        kind: PackageSourceKind,
        view: Arc<dyn PackageView>,
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

    fn with_discovery_error(mut self, error: PackageError) -> Self {
        self.discovery_error = Some(error);
        self
    }

    pub fn discovery_error(&self) -> Option<&PackageError> {
        self.discovery_error.as_ref()
    }

    pub fn view(&self) -> &dyn PackageView {
        self.view.as_ref()
    }

    /// Captures one immutable package-byte view for resolution and every later
    /// typed projection derived from the selected candidate.
    pub fn snapshot(&self) -> Result<Self, PackageError> {
        let files = self
            .view
            .files()?
            .into_iter()
            .map(|path| {
                let bytes = self.view.read_file(&path)?;
                Ok((path, bytes))
            })
            .collect::<Result<Vec<_>, PackageError>>()?;
        let mut candidate = self.clone();
        candidate.view = Arc::new(InMemoryPackageView::new(files)?);
        Ok(candidate)
    }
}

pub trait PackageSource: Send + Sync {
    fn id(&self) -> &PackageSourceId;
    fn tier(&self) -> PackageSourceTier;
    fn kind(&self) -> PackageSourceKind;
    fn candidates(&self, type_id: &PackageTypeId) -> Result<Vec<PackageCandidate>, PackageError>;

    fn inventory_candidates(
        &self,
        type_id: &PackageTypeId,
    ) -> Result<Vec<PackageCandidate>, PackageError> {
        self.candidates(type_id)
    }
}

pub type ErasedPackagePayload = Box<dyn Any + Send + Sync>;

/// Adapter lifecycle completed in H02; payload implementations remain in domain crates.
pub trait PackageAdapter: Send + Sync {
    fn descriptor(&self) -> &PackageAdapterDescriptor;

    fn extract_envelope(&self, package: &dyn PackageView) -> Result<PackageEnvelope, PackageError>;

    fn validate_payload(
        &self,
        package: &dyn PackageView,
        envelope: &PackageEnvelope,
    ) -> Result<ErasedPackagePayload, PackageError>;
}

impl<T> PackageAdapter for Arc<T>
where
    T: PackageAdapter + ?Sized,
{
    fn descriptor(&self) -> &PackageAdapterDescriptor {
        self.as_ref().descriptor()
    }

    fn extract_envelope(&self, package: &dyn PackageView) -> Result<PackageEnvelope, PackageError> {
        self.as_ref().extract_envelope(package)
    }

    fn validate_payload(
        &self,
        package: &dyn PackageView,
        envelope: &PackageEnvelope,
    ) -> Result<ErasedPackagePayload, PackageError> {
        self.as_ref().validate_payload(package, envelope)
    }
}

#[derive(Clone, Default)]
pub struct PackageAdapterRegistry {
    adapters: BTreeMap<PackageTypeId, Arc<dyn PackageAdapter>>,
}

impl fmt::Debug for PackageAdapterRegistry {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackageAdapterRegistry")
            .field("types", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PackageAdapterRegistry {
    pub fn new<A>(adapters: impl IntoIterator<Item = A>) -> Result<Self, PackageError>
    where
        A: PackageAdapter + 'static,
    {
        let mut registry = Self::default();
        for adapter in adapters {
            registry.insert(Arc::new(adapter))?;
        }
        Ok(registry)
    }

    pub fn from_adapters<A>(adapters: impl IntoIterator<Item = A>) -> Result<Self, PackageError>
    where
        A: PackageAdapter + 'static,
    {
        Self::new(adapters)
    }

    pub fn from_dyn(
        adapters: impl IntoIterator<Item = Arc<dyn PackageAdapter>>,
    ) -> Result<Self, PackageError> {
        let mut registry = Self::default();
        for adapter in adapters {
            registry.insert(adapter)?;
        }
        Ok(registry)
    }

    pub fn lookup(&self, type_id: &PackageTypeId) -> Result<&dyn PackageAdapter, PackageError> {
        self.adapters
            .get(type_id)
            .map(AsRef::as_ref)
            .ok_or_else(|| PackageError::UnsupportedType {
                type_id: type_id.to_string(),
            })
    }

    pub fn get(&self, type_id: &PackageTypeId) -> Option<&dyn PackageAdapter> {
        self.adapters.get(type_id).map(AsRef::as_ref)
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn PackageAdapter> {
        self.adapters.values().map(AsRef::as_ref)
    }

    fn insert(&mut self, adapter: Arc<dyn PackageAdapter>) -> Result<(), PackageError> {
        let descriptor = adapter.descriptor();
        if self.adapters.contains_key(&descriptor.type_id) {
            return Err(PackageError::DuplicateAdapterType {
                type_id: descriptor.type_id.to_string(),
            });
        }
        if self
            .adapters
            .values()
            .any(|existing| existing.descriptor().root == descriptor.root)
        {
            return Err(PackageError::DuplicateAdapterRoot {
                root: descriptor.root.clone(),
            });
        }
        if !descriptor.type_id.is_core() && RESERVED_ROOTS.contains(&descriptor.root.as_str()) {
            return Err(PackageError::ReservedRootCollision {
                root: descriptor.root.clone(),
            });
        }
        if let Some(expected) = core_root(descriptor.type_id.as_str())
            && expected != descriptor.root
        {
            return Err(PackageError::CoreRootMismatch {
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
pub struct StaticPackageSource {
    source_id: PackageSourceId,
    tier: PackageSourceTier,
    kind: PackageSourceKind,
    candidates: Vec<PackageCandidate>,
}

impl StaticPackageSource {
    pub fn new(
        source_id: PackageSourceId,
        tier: PackageSourceTier,
        kind: PackageSourceKind,
        candidates: impl IntoIterator<Item = PackageCandidate>,
    ) -> Result<Self, PackageError> {
        let candidates = candidates.into_iter().collect::<Vec<_>>();
        for candidate in &candidates {
            if candidate.source_id != source_id || candidate.tier != tier || candidate.kind != kind
            {
                return Err(PackageError::PackageIo {
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

    fn matching_candidates(&self, type_id: &PackageTypeId) -> Vec<PackageCandidate> {
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
        candidates
    }
}

impl PackageSource for StaticPackageSource {
    fn id(&self) -> &PackageSourceId {
        &self.source_id
    }

    fn tier(&self) -> PackageSourceTier {
        self.tier
    }

    fn kind(&self) -> PackageSourceKind {
        self.kind
    }

    fn candidates(&self, type_id: &PackageTypeId) -> Result<Vec<PackageCandidate>, PackageError> {
        let candidates = self.matching_candidates(type_id);
        if let Some(error) = candidates
            .iter()
            .find_map(PackageCandidate::discovery_error)
        {
            return Err(error.clone());
        }
        Ok(candidates)
    }

    fn inventory_candidates(
        &self,
        type_id: &PackageTypeId,
    ) -> Result<Vec<PackageCandidate>, PackageError> {
        Ok(self.matching_candidates(type_id))
    }
}

pub struct DirectoryPackageSource {
    source_id: PackageSourceId,
    tier: PackageSourceTier,
    kind: PackageSourceKind,
    root: PathBuf,
    registry: Arc<PackageAdapterRegistry>,
    source_revision: Option<String>,
    source_tree: Option<String>,
}

impl fmt::Debug for DirectoryPackageSource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryPackageSource")
            .field("source_id", &self.source_id)
            .field("tier", &self.tier)
            .field("kind", &self.kind)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl DirectoryPackageSource {
    pub fn new(
        source_id: PackageSourceId,
        tier: PackageSourceTier,
        kind: PackageSourceKind,
        root: impl Into<PathBuf>,
        registry: Arc<PackageAdapterRegistry>,
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
        type_id: &PackageTypeId,
        preserve_invalid_envelopes: bool,
    ) -> Result<Vec<PackageCandidate>, PackageError> {
        let adapter = self.registry.lookup(type_id)?;
        let typed_root = self.root.join(&adapter.descriptor().root);
        match fs::symlink_metadata(&typed_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PackageError::PackageSymlinkRejected {
                    path: typed_root.display().to_string(),
                });
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(PackageError::PackagePathNotRegular {
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
                    if let Ok(version) = PackageVersion::new(version_text) {
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
            let package_id = PackageId::new(package_text)?;
            let package_root = typed_root.join(package_dir_text);
            let view = DirectoryPackageView::new(&package_root)?;
            let validated_version = (|| {
                let envelope = adapter.extract_envelope(&view)?;
                envelope.validate()?;
                if envelope.type_id != *type_id {
                    return Err(PackageError::AdapterTypeMismatch {
                        type_id: type_id.to_string(),
                        actual_type: envelope.type_id.to_string(),
                    });
                }
                if envelope.id != package_id {
                    return Err(PackageError::AdapterPackageMismatch {
                        type_id: type_id.to_string(),
                        actual_id: envelope.id.to_string(),
                    });
                }
                if let Some(declared_version) = &declared_version {
                    if declared_version != &envelope.version {
                        return Err(PackageError::CandidateVersionMismatch {
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
                        PackageVersion::new("0.0.0-invalid")
                            .expect("invalid-envelope inventory version is valid SemVer")
                    }),
                    Some(error),
                ),
                Err(error) => return Err(error),
            };
            let mut candidate = PackageCandidate::new(
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

impl PackageSource for DirectoryPackageSource {
    fn id(&self) -> &PackageSourceId {
        &self.source_id
    }

    fn tier(&self) -> PackageSourceTier {
        self.tier
    }

    fn kind(&self) -> PackageSourceKind {
        self.kind
    }

    fn candidates(&self, type_id: &PackageTypeId) -> Result<Vec<PackageCandidate>, PackageError> {
        self.scan_candidates(type_id, false)
    }

    fn inventory_candidates(
        &self,
        type_id: &PackageTypeId,
    ) -> Result<Vec<PackageCandidate>, PackageError> {
        self.scan_candidates(type_id, true)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageDataClass {
    Package,
    Config,
    State,
    Cache,
}

#[derive(Clone, Debug)]
pub struct PackagePathRouter {
    workspace_root: PathBuf,
    data_root: PathBuf,
    config_root: PathBuf,
    state_root: PathBuf,
    cache_root: PathBuf,
    registry: Arc<PackageAdapterRegistry>,
}

impl PackagePathRouter {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        data_root: impl Into<PathBuf>,
        config_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        cache_root: impl Into<PathBuf>,
        registry: Arc<PackageAdapterRegistry>,
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
        scope: PackagePathScope,
        data_class: PackageDataClass,
        type_id: &PackageTypeId,
    ) -> Result<PathBuf, PackageError> {
        let type_root = self.registry.lookup(type_id)?.descriptor().root.clone();
        let root = match (scope, data_class) {
            (PackagePathScope::Workspace, PackageDataClass::Package) => {
                self.workspace_root.join(".agl")
            }
            (PackagePathScope::Workspace, PackageDataClass::Config) => {
                self.workspace_root.join(".agl/config")
            }
            (PackagePathScope::Workspace, PackageDataClass::State) => {
                self.workspace_root.join(".agl/state")
            }
            (PackagePathScope::Workspace, PackageDataClass::Cache) => {
                self.workspace_root.join(".agl/cache")
            }
            (PackagePathScope::Xdg, PackageDataClass::Package) => self.data_root.clone(),
            (PackagePathScope::Xdg, PackageDataClass::Config) => self.config_root.clone(),
            (PackagePathScope::Xdg, PackageDataClass::State) => self.state_root.clone(),
            (PackagePathScope::Xdg, PackageDataClass::Cache) => self.cache_root.clone(),
        };
        Ok(root.join(type_root))
    }

    pub fn workspace_package_root(&self, type_id: &PackageTypeId) -> Result<PathBuf, PackageError> {
        self.root(
            PackagePathScope::Workspace,
            PackageDataClass::Package,
            type_id,
        )
    }

    pub fn xdg_package_root(&self, type_id: &PackageTypeId) -> Result<PathBuf, PackageError> {
        self.root(PackagePathScope::Xdg, PackageDataClass::Package, type_id)
    }

    pub fn workspace_config_root(&self, type_id: &PackageTypeId) -> Result<PathBuf, PackageError> {
        self.root(
            PackagePathScope::Workspace,
            PackageDataClass::Config,
            type_id,
        )
    }

    pub fn xdg_config_root(&self, type_id: &PackageTypeId) -> Result<PathBuf, PackageError> {
        self.root(PackagePathScope::Xdg, PackageDataClass::Config, type_id)
    }

    pub fn workspace_package_path(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
        version: &PackageVersion,
    ) -> Result<PathBuf, PackageError> {
        Ok(append_package_version(
            self.workspace_package_root(type_id)?,
            package_id,
            version,
        ))
    }

    pub fn xdg_package_path(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
        version: &PackageVersion,
    ) -> Result<PathBuf, PackageError> {
        Ok(append_package_version(
            self.xdg_package_root(type_id)?,
            package_id,
            version,
        ))
    }

    pub fn workspace_config_path(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
    ) -> Result<PathBuf, PackageError> {
        Ok(append_package_id(
            self.workspace_config_root(type_id)?,
            package_id,
        ))
    }

    pub fn xdg_config_path(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
    ) -> Result<PathBuf, PackageError> {
        Ok(append_package_id(
            self.xdg_config_root(type_id)?,
            package_id,
        ))
    }

    pub fn config_layers(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
    ) -> Result<Vec<PackageConfigEvidence>, PackageError> {
        let user = self.xdg_config_path(type_id, package_id)?;
        let workspace = self.workspace_config_path(type_id, package_id)?;
        Ok(vec![
            PackageConfigEvidence {
                layer: PackageConfigLayer::PackageDefaults,
                path: None,
                present: true,
            },
            PackageConfigEvidence {
                layer: PackageConfigLayer::User,
                path: Some(user.clone()),
                present: user.exists(),
            },
            PackageConfigEvidence {
                layer: PackageConfigLayer::Workspace,
                path: Some(workspace.clone()),
                present: workspace.exists(),
            },
        ])
    }

    pub fn state_root(&self, type_id: &PackageTypeId) -> Result<PathBuf, PackageError> {
        self.root(PackagePathScope::Xdg, PackageDataClass::State, type_id)
    }

    pub fn cache_root(&self, type_id: &PackageTypeId) -> Result<PathBuf, PackageError> {
        self.root(PackagePathScope::Xdg, PackageDataClass::Cache, type_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PackagePathScope {
    Workspace,
    Xdg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageConfigLayer {
    PackageDefaults,
    User,
    Workspace,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageConfigEvidence {
    pub layer: PackageConfigLayer,
    pub path: Option<PathBuf>,
    pub present: bool,
}

fn append_package_id(mut root: PathBuf, package_id: &PackageId) -> PathBuf {
    for segment in package_id.as_str().split('/') {
        root.push(segment);
    }
    root
}

fn append_package_version(
    root: PathBuf,
    package_id: &PackageId,
    version: &PackageVersion,
) -> PathBuf {
    let mut path = append_package_id(root, package_id);
    path.push(version.to_string());
    path
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageTreeDigest(String);

impl PackageTreeDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        let valid = value.len() == 71
            && value.starts_with("sha256:")
            && value[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if !valid {
            return Err(PackageError::InvalidPackageDigest { value });
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
    type Err = PackageError;

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

pub fn compute_package_digest(view: &dyn PackageView) -> Result<PackageTreeDigest, PackageError> {
    let mut files = view.files()?;
    files.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.package-tree.v1\0");
    let mut previous = None;
    for path in files {
        if previous.as_ref() == Some(&path) {
            return Err(PackageError::DuplicatePackageFile {
                path: path.to_string(),
            });
        }
        if reserved_package_file(&path) {
            return Err(PackageError::ReservedPackageFile {
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

fn reserved_package_file(path: &PackageRelativePath) -> bool {
    let value = path.as_str();
    value.starts_with(".agl/")
        || value == "workspace.toml"
        || value == "package-lock.toml"
        || value.starts_with("config/")
        || value.starts_with("state/")
        || value.starts_with("cache/")
        || value.contains("/source-index")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub type_id: PackageTypeId,
    pub id: PackageId,
    pub version: PackageVersion,
    pub source_tier: PackageSourceTier,
    pub source_kind: PackageSourceKind,
    pub source_id: PackageSourceId,
    pub package_tree_digest: PackageTreeDigest,
    pub envelope_schema: PackageSchemaId,
    pub payload_schema: PackageSchemaId,
    #[serde(default)]
    pub source_revision: Option<String>,
    #[serde(default)]
    pub source_tree: Option<String>,
    pub dependencies: Vec<String>,
}

impl LockedPackage {
    pub fn key(&self) -> String {
        format!("{}:{}@{}", self.type_id, self.id, self.version)
    }

    fn validate(&self, key: &str) -> Result<(), PackageError> {
        if self.key() != key {
            return Err(PackageError::InvalidLockPackageKey {
                key: key.to_owned(),
            });
        }
        let mut sorted = self.dependencies.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != self.dependencies {
            return Err(PackageError::LockDrift {
                key: key.to_owned(),
                field: "dependencies".to_owned(),
                expected: sorted.join(","),
                actual: self.dependencies.join(","),
            });
        }
        if self.source_kind == PackageSourceKind::Git {
            for (field, value) in [
                ("source_revision", self.source_revision.as_deref()),
                ("source_tree", self.source_tree.as_deref()),
            ] {
                if value.is_none_or(str::is_empty) {
                    return Err(PackageError::LockDrift {
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

    pub fn fixture(
        type_id: PackageTypeId,
        id: PackageId,
        version: PackageVersion,
        source_id: PackageSourceId,
        package_tree_digest: PackageTreeDigest,
    ) -> Self {
        Self {
            type_id,
            id,
            version,
            source_tier: PackageSourceTier::Explicit,
            source_kind: PackageSourceKind::Embedded,
            source_id,
            package_tree_digest,
            envelope_schema: PackageSchemaId::common(),
            payload_schema: PackageSchemaId::new("agentlibre.fixture/v1").unwrap(),
            source_revision: None,
            source_tree: None,
            dependencies: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageLock {
    pub version: u32,
    #[serde(default)]
    pub packages: Vec<LockedPackage>,
}

impl PackageLock {
    pub const VERSION: u32 = 1;

    pub fn new(packages: impl IntoIterator<Item = LockedPackage>) -> Result<Self, PackageError> {
        let mut packages = packages.into_iter().collect::<Vec<_>>();
        packages.sort_by_key(LockedPackage::key);
        let lock = Self {
            version: Self::VERSION,
            packages,
        };
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<(), PackageError> {
        if self.version != Self::VERSION {
            return Err(PackageError::UnsupportedLockVersion {
                version: self.version,
            });
        }
        let mut previous = None;
        for package in &self.packages {
            let key = package.key();
            package.validate(&key)?;
            if previous.as_ref().is_some_and(|previous| previous >= &key) {
                return Err(PackageError::InvalidLockPackageKey { key });
            }
            previous = Some(key);
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String, PackageError> {
        self.validate()?;
        toml::to_string(self).map_err(|error| PackageError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })
    }

    pub fn from_toml(value: &str) -> Result<Self, PackageError> {
        let lock: Self = toml::from_str(value).map_err(|error| PackageError::LockIo {
            path: "<memory>".to_owned(),
            reason: error.to_string(),
        })?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn write_atomic(&self, path: impl AsRef<Path>) -> Result<(), PackageError> {
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
            return Err(PackageError::LockIo {
                path: path.display().to_string(),
                reason: error.to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    pub candidate: PackageCandidate,
    pub envelope: PackageEnvelope,
    pub package_tree_digest: PackageTreeDigest,
    pub dependencies: Vec<String>,
}

impl ResolvedPackage {
    pub fn key(&self) -> String {
        format!(
            "{}:{}@{}",
            self.candidate.type_id, self.candidate.package_id, self.candidate.version
        )
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedPackageGraph {
    pub root: String,
    pub nodes: BTreeMap<String, ResolvedPackage>,
}

impl ResolvedPackageGraph {
    pub fn package_lock_entries(&self) -> Result<BTreeMap<String, LockedPackage>, PackageError> {
        let mut packages = BTreeMap::new();
        for node in self.nodes.values() {
            let locked = LockedPackage {
                type_id: node.candidate.type_id.clone(),
                id: node.candidate.package_id.clone(),
                version: node.candidate.version.clone(),
                source_tier: node.candidate.tier,
                source_kind: node.candidate.kind,
                source_id: node.candidate.source_id.clone(),
                package_tree_digest: node.package_tree_digest.clone(),
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

    pub fn lock(&self) -> Result<PackageLock, PackageError> {
        PackageLock::new(self.package_lock_entries()?.into_values())
    }

    pub fn verify_lock(&self, lock: &PackageLock) -> Result<(), PackageError> {
        lock.validate()?;
        for node in self.nodes.values() {
            let key = node.key();
            let Some(locked) = lock.packages.iter().find(|package| package.key() == key) else {
                return Err(PackageError::LockMissingPackage { key });
            };
            compare_lock_field(
                &key,
                "package_tree_digest",
                locked.package_tree_digest.to_string(),
                node.package_tree_digest.to_string(),
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

    pub fn validate_payloads(&self, registry: &PackageAdapterRegistry) -> Result<(), PackageError> {
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
) -> Result<(), PackageError> {
    if expected == actual {
        Ok(())
    } else {
        Err(PackageError::LockDrift {
            key: key.to_owned(),
            field: field.to_owned(),
            expected,
            actual,
        })
    }
}

fn source_tier_name(value: PackageSourceTier) -> &'static str {
    match value {
        PackageSourceTier::Explicit => "explicit",
        PackageSourceTier::Workspace => "workspace",
        PackageSourceTier::User => "user",
        PackageSourceTier::System => "system",
        PackageSourceTier::Builtin => "builtin",
    }
}

fn source_kind_name(value: PackageSourceKind) -> &'static str {
    match value {
        PackageSourceKind::Directory => "directory",
        PackageSourceKind::Git => "git",
        PackageSourceKind::Embedded => "embedded",
    }
}

#[derive(Clone)]
pub struct PackageResolver {
    registry: Arc<PackageAdapterRegistry>,
    sources: Vec<Arc<dyn PackageSource>>,
}

impl fmt::Debug for PackageResolver {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackageResolver")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

impl PackageResolver {
    pub fn new(
        registry: Arc<PackageAdapterRegistry>,
        sources: Vec<Arc<dyn PackageSource>>,
    ) -> Self {
        Self { registry, sources }
    }

    pub fn resolve(&self, root: &PackageRef) -> Result<ResolvedPackageGraph, PackageError> {
        self.resolve_with_selection(root, CandidateSelection::Global)
    }

    pub fn resolve_with_explicit_root(
        &self,
        root: &PackageRef,
        source_id: &PackageSourceId,
    ) -> Result<ResolvedPackageGraph, PackageError> {
        self.resolve_with_selection(root, CandidateSelection::ExplicitRoot(source_id))
    }

    fn resolve_with_selection(
        &self,
        root: &PackageRef,
        selection: CandidateSelection<'_>,
    ) -> Result<ResolvedPackageGraph, PackageError> {
        let mut nodes = BTreeMap::new();
        let mut stack = Vec::new();
        let root_key = self.visit(
            &root.type_id,
            &root.package_id,
            std::slice::from_ref(&root.version_req),
            selection,
            &mut stack,
            &mut nodes,
        )?;
        Ok(ResolvedPackageGraph {
            root: root_key,
            nodes,
        })
    }

    pub fn resolve_locked(
        &self,
        root: &PackageRef,
        lock: &PackageLock,
    ) -> Result<ResolvedPackageGraph, PackageError> {
        let graph = self.resolve(root)?;
        graph.verify_lock(lock)?;
        Ok(graph)
    }

    pub fn resolve_and_validate(
        &self,
        root: &PackageRef,
        lock: Option<&PackageLock>,
    ) -> Result<ResolvedPackageGraph, PackageError> {
        let graph = self.resolve(root)?;
        if let Some(lock) = lock {
            graph.verify_lock(lock)?;
        }
        graph.validate_payloads(&self.registry)?;
        Ok(graph)
    }

    pub fn resolve_and_validate_with_explicit_root(
        &self,
        root: &PackageRef,
        source_id: &PackageSourceId,
        lock: Option<&PackageLock>,
    ) -> Result<ResolvedPackageGraph, PackageError> {
        let graph = self.resolve_with_explicit_root(root, source_id)?;
        if let Some(lock) = lock {
            graph.verify_lock(lock)?;
        }
        graph.validate_payloads(&self.registry)?;
        Ok(graph)
    }

    fn visit(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
        constraints: &[PackageVersionReq],
        selection: CandidateSelection<'_>,
        stack: &mut Vec<String>,
        nodes: &mut BTreeMap<String, ResolvedPackage>,
    ) -> Result<String, PackageError> {
        let identity = format!("{}:{}", type_id, package_id);
        if let Some(position) = stack.iter().position(|entry| entry == &identity) {
            let mut cycle = stack[position..].to_vec();
            cycle.push(identity);
            return Err(PackageError::DependencyCycle { path: cycle });
        }
        if let Some(existing) = nodes.values().find(|node| {
            node.candidate.type_id == *type_id && node.candidate.package_id == *package_id
        }) {
            for constraint in constraints {
                if !constraint.matches(&existing.candidate.version) {
                    return Err(PackageError::ConstraintConflict {
                        key: existing.key(),
                        requirement: constraint.to_string(),
                    });
                }
            }
            return Ok(existing.key());
        }

        let candidate = self
            .select_candidate(type_id, package_id, constraints, selection)?
            .snapshot()?;
        let adapter = self.registry.lookup(type_id)?;
        let envelope = adapter.extract_envelope(candidate.view())?;
        envelope.validate()?;
        if envelope.type_id != *type_id {
            return Err(PackageError::AdapterTypeMismatch {
                type_id: type_id.to_string(),
                actual_type: envelope.type_id.to_string(),
            });
        }
        if envelope.id != *package_id {
            return Err(PackageError::AdapterPackageMismatch {
                type_id: type_id.to_string(),
                actual_id: envelope.id.to_string(),
            });
        }
        if envelope.version != candidate.version {
            return Err(PackageError::CandidateVersionMismatch {
                candidate: candidate.version.to_string(),
                envelope: envelope.version.to_string(),
            });
        }
        let package_tree_digest = compute_package_digest(candidate.view())?;
        let key = format!("{}:{}@{}", type_id, package_id, candidate.version);
        stack.push(identity);
        let mut dependencies = Vec::new();
        for requirement in &envelope.requires {
            let dependency_key = self
                .visit(
                    requirement.type_id(),
                    requirement.package_id(),
                    std::slice::from_ref(&requirement.reference().version_req),
                    selection.for_dependency(),
                    stack,
                    nodes,
                )
                .map_err(|error| match error {
                    PackageError::PackageNotFound { .. } => PackageError::MissingDependency {
                        parent: key.clone(),
                        reference: requirement.to_string(),
                    },
                    other => other,
                })?;
            dependencies.push(dependency_key);
        }
        stack.pop();
        dependencies.sort();
        let node = ResolvedPackage {
            candidate,
            envelope,
            package_tree_digest,
            dependencies,
        };
        nodes.insert(key.clone(), node);
        Ok(key)
    }

    fn select_candidate(
        &self,
        type_id: &PackageTypeId,
        package_id: &PackageId,
        constraints: &[PackageVersionReq],
        selection: CandidateSelection<'_>,
    ) -> Result<PackageCandidate, PackageError> {
        let mut sources = self.sources.clone();
        sources.sort_by_key(|source| (source.tier(), source.id().clone()));
        let tiers = [
            PackageSourceTier::Explicit,
            PackageSourceTier::Workspace,
            PackageSourceTier::User,
            PackageSourceTier::System,
            PackageSourceTier::Builtin,
        ];
        let mut available = BTreeSet::new();
        for tier in tiers {
            match selection {
                CandidateSelection::Global
                    if sources
                        .iter()
                        .any(|source| source.tier() == PackageSourceTier::Explicit)
                        && tier != PackageSourceTier::Explicit =>
                {
                    break;
                }
                CandidateSelection::ExplicitRoot(_) if tier != PackageSourceTier::Explicit => {
                    break;
                }
                CandidateSelection::Ordinary if tier == PackageSourceTier::Explicit => continue,
                CandidateSelection::Global
                | CandidateSelection::ExplicitRoot(_)
                | CandidateSelection::Ordinary => {}
            }
            let mut candidates = Vec::new();
            for source in sources.iter().filter(|source| source.tier() == tier) {
                if let CandidateSelection::ExplicitRoot(source_id) = selection
                    && source.id() != source_id
                {
                    continue;
                }
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
                return Err(PackageError::AmbiguousCandidate {
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
            Err(PackageError::PackageNotFound {
                type_id: type_id.to_string(),
                package_id: package_id.to_string(),
            })
        } else {
            Err(PackageError::IncompatibleVersion {
                type_id: type_id.to_string(),
                package_id: package_id.to_string(),
                requirements: constraints.iter().map(ToString::to_string).collect(),
                available: available.into_iter().collect(),
            })
        }
    }
}

#[derive(Clone, Copy)]
enum CandidateSelection<'a> {
    Global,
    ExplicitRoot(&'a PackageSourceId),
    Ordinary,
}

impl CandidateSelection<'_> {
    fn for_dependency(self) -> Self {
        match self {
            Self::ExplicitRoot(_) => Self::Ordinary,
            other => other,
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
        descriptor: PackageAdapterDescriptor,
    }

    impl PackageAdapter for TestAdapter {
        fn descriptor(&self) -> &PackageAdapterDescriptor {
            &self.descriptor
        }

        fn extract_envelope(
            &self,
            _package: &dyn PackageView,
        ) -> Result<PackageEnvelope, PackageError> {
            Err(PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "test adapter".to_owned(),
            })
        }

        fn validate_payload(
            &self,
            _package: &dyn PackageView,
            _envelope: &PackageEnvelope,
        ) -> Result<ErasedPackagePayload, PackageError> {
            Err(PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "test adapter".to_owned(),
            })
        }
    }

    fn version(value: &str) -> PackageVersion {
        value.parse().unwrap()
    }

    fn requirement(value: &str) -> PackageRequirement {
        value.parse().unwrap()
    }

    #[test]
    fn type_ids_enforce_core_and_custom_grammar() {
        assert!("function".parse::<PackageTypeId>().is_ok());
        assert!("vendor.workflow".parse::<PackageTypeId>().is_ok());
        for invalid in ["", "Vendor.workflow", "vendor", "agentlibre.custom", "a..b"] {
            assert!(invalid.parse::<PackageTypeId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn package_ids_reject_paths_and_delimiters() {
        for valid in ["example", "vendor/workflow.v2", "a-1/b_c"] {
            assert!(valid.parse::<PackageId>().is_ok(), "{valid}");
        }
        for invalid in ["", "/absolute", "a/", "a//b", "a/../b", "a:b", "a@b", "A"] {
            assert!(invalid.parse::<PackageId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn references_are_canonical() {
        let reference: PackageRef = "skill:vendor/workflow@^1.0".parse().unwrap();
        assert_eq!(reference.to_string(), "skill:vendor/workflow@^1.0");
        for invalid in [
            "skill/vendor/workflow@^1",
            "skill:vendor/workflow",
            "skill:vendor@^1@x",
        ] {
            assert!(invalid.parse::<PackageRef>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn compatibility_requires_tested_versions_inside_range() {
        let compatible: PackageVersionReq = ">=1.0.0, <2.0.0".parse().unwrap();
        assert!(AglCompatibility::new(compatible.clone(), [version("1.2.0")]).is_ok());
        assert_eq!(
            AglCompatibility::new(compatible, []).unwrap_err(),
            PackageError::EmptyTestedVersions
        );
        assert!(matches!(
            AglCompatibility::new(">=2.0.0".parse().unwrap(), [version("1.2.0")]),
            Err(PackageError::TestedVersionIncompatible { .. })
        ));
    }

    #[test]
    fn envelope_rejects_duplicate_requirement_targets() {
        let agl = AglCompatibility::new(">=1.0.0".parse().unwrap(), [version("1.0.0")]).unwrap();
        let result = PackageEnvelope::new(
            PackageTypeId::function(),
            "example".parse().unwrap(),
            version("1.0.0"),
            "agentlibre.function/v3".parse().unwrap(),
            agl,
            vec![
                requirement("skill:workflow@^1"),
                requirement("skill:workflow@^2"),
            ],
        );
        assert!(matches!(
            result,
            Err(PackageError::DuplicateRequirement { .. })
        ));
    }

    #[test]
    fn registry_checks_roots_and_unknown_types() {
        let function = PackageAdapterDescriptor::new(
            PackageTypeId::function(),
            FUNCTION_ROOT,
            "FUNCTION.md".parse().unwrap(),
        )
        .unwrap();
        let registry = PackageAdapterRegistry::new([TestAdapter {
            descriptor: function,
        }])
        .unwrap();
        assert_eq!(
            registry
                .lookup(&PackageTypeId::function())
                .unwrap()
                .descriptor()
                .root,
            FUNCTION_ROOT
        );
        assert!(matches!(
            registry.lookup(&PackageTypeId::skill()),
            Err(PackageError::UnsupportedType { .. })
        ));
        let wrong = PackageAdapterDescriptor::new(
            PackageTypeId::skill(),
            FUNCTION_ROOT,
            "SKILL.md".parse().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            PackageAdapterRegistry::new([TestAdapter { descriptor: wrong }]),
            Err(PackageError::CoreRootMismatch { .. })
        ));
    }
}
