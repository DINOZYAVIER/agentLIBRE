//! Immutable, exact package identities and bounded package trees.
//!
//! This crate intentionally has no dependency solver, lock refresh workflow,
//! runtime installation state, adapter registry, or `Any` payload surface.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use semver::Version;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum PackageError {
    #[error("invalid {field}: {value}")]
    InvalidIdentifier { field: &'static str, value: String },
    #[error("invalid package version: {0}")]
    InvalidVersion(String),
    #[error("invalid relative package path: {0}")]
    InvalidRelativePath(String),
    #[error("package path is a symlink: {0}")]
    Symlink(String),
    #[error("package path is not a regular file or directory: {0}")]
    NotRegular(String),
    #[error("package file is missing: {0}")]
    MissingFile(String),
    #[error("duplicate package file: {0}")]
    DuplicateFile(String),
    #[error("reserved package file: {0}")]
    ReservedFile(String),
    #[error("package tree exceeds its bound")]
    TreeTooLarge,
    #[error("invalid package digest: {0}")]
    InvalidDigest(String),
    #[error("package I/O at {path}: {reason}")]
    Io { path: String, reason: String },
}

macro_rules! identifier {
    ($name:ident, $field:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
                let value = value.into();
                if value.is_empty()
                    || value.len() > 128
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'-' | b'_' | b'.')
                    })
                    || !value.as_bytes()[0].is_ascii_lowercase()
                {
                    return Err(PackageError::InvalidIdentifier {
                        field: $field,
                        value,
                    });
                }
                Ok(Self(value))
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
            type Err = PackageError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
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
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier!(PackageId, "package ID");

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageVersion(Version);

impl PackageVersion {
    pub fn new(value: impl AsRef<str>) -> Result<Self, PackageError> {
        Version::parse(value.as_ref())
            .map(Self)
            .map_err(|_| PackageError::InvalidVersion(value.as_ref().to_owned()))
    }
}

impl fmt::Display for PackageVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
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
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for PackageVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackageRelativePath(String);

impl PackageRelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        if value.is_empty()
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\\')
            || value.contains(':')
            || value.chars().any(char::is_control)
            || value
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(PackageError::InvalidRelativePath(value));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for PackageRelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl FromStr for PackageRelativePath {
    type Err = PackageError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

pub trait PackageView: Sized + Send + Sync {
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
        let mut output = Self::default();
        for (path, bytes) in files {
            if output.files.insert(path.clone(), bytes).is_some() {
                return Err(PackageError::DuplicateFile(path.to_string()));
            }
        }
        Ok(output)
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
            .ok_or_else(|| PackageError::MissingFile(path.to_string()))
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageTreeDigest(String);
impl PackageTreeDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, PackageError> {
        let value = value.into();
        if value.len() != 71
            || !value.starts_with("sha256:")
            || !value[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PackageError::InvalidDigest(value));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for PackageTreeDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub fn compute_package_digest(view: &impl PackageView) -> Result<PackageTreeDigest, PackageError> {
    let mut files = view.files()?;
    files.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.package-tree.v1\0");
    let mut total = 0_u64;
    let mut previous = None;
    for path in files {
        if previous.as_ref() == Some(&path) {
            return Err(PackageError::DuplicateFile(path.to_string()));
        }
        if reserved(&path) {
            return Err(PackageError::ReservedFile(path.to_string()));
        }
        let bytes = view.read_file(&path)?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or(PackageError::TreeTooLarge)?;
        if total > MAX_PACKAGE_BYTES {
            return Err(PackageError::TreeTooLarge);
        }
        hasher.update((path.as_str().len() as u64).to_be_bytes());
        hasher.update(path.as_str().as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
        previous = Some(path);
    }
    let mut value = String::from("sha256:");
    for byte in hasher.finalize() {
        use fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    PackageTreeDigest::new(value)
}

fn reserved(path: &PackageRelativePath) -> bool {
    let value = path.as_str();
    value.split('/').any(|component| component == ".git")
        || matches!(value, "workspace.toml" | "package-lock.toml")
        || value.starts_with("config/")
        || value.starts_with("state/")
        || value.starts_with("cache/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_contained() {
        assert!(PackageRelativePath::new("references/api.md").is_ok());
        assert!(PackageRelativePath::new("../secret").is_err());
    }

    #[test]
    fn package_digest_is_order_independent() {
        let left = InMemoryPackageView::new([
            (PackageRelativePath::new("b").unwrap(), b"2".to_vec()),
            (PackageRelativePath::new("a").unwrap(), b"1".to_vec()),
        ])
        .unwrap();
        let right = InMemoryPackageView::new([
            (PackageRelativePath::new("a").unwrap(), b"1".to_vec()),
            (PackageRelativePath::new("b").unwrap(), b"2".to_vec()),
        ])
        .unwrap();
        assert_eq!(
            compute_package_digest(&left).unwrap(),
            compute_package_digest(&right).unwrap()
        );
    }

    #[test]
    fn dot_agl_is_an_ordinary_package_path() {
        let package = InMemoryPackageView::new([(
            PackageRelativePath::new(".agl/content.txt").unwrap(),
            b"ordinary content".to_vec(),
        )])
        .unwrap();
        assert!(compute_package_digest(&package).is_ok());
    }
}
