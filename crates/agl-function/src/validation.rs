use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};
pub fn validate_function_id(label: &str, value: &str) -> Result<()> {
    ensure!(
        is_valid_function_id(value),
        "{label} must use lowercase ASCII letters, digits, hyphens, underscores, or dots: {value}"
    );
    Ok(())
}

pub(crate) fn sha256_text(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}

pub(crate) fn sha256_bytes(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut value = String::with_capacity("sha256:".len() + digest.len() * 2);
    value.push_str("sha256:");
    for byte in digest {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

pub(crate) fn validate_relative_function_file_path(label: &str, value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{label} cannot be empty");
    ensure!(!value.contains('\0'), "{label} cannot contain NUL");
    let path = Path::new(value);
    ensure!(!path.is_absolute(), "{label} cannot be absolute: {value}");
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                ensure!(segment != ".git", "{label} cannot enter .git: {value}");
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("{label} cannot contain parent traversal: {value}");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("{label} cannot be absolute: {value}");
            }
        }
    }
    Ok(())
}

pub fn is_valid_function_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

pub(crate) fn default_identity_fields() -> Vec<String> {
    ["function", "skills", "subagents"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub(crate) fn is_valid_identity_field(field: &str) -> bool {
    matches!(field, "function" | "skills" | "subagents" | "model_profile")
}

pub(crate) fn validate_extensions(
    label: &str,
    extensions: &BTreeMap<String, serde_yaml::Value>,
) -> Result<()> {
    for key in extensions.keys() {
        ensure!(
            key.starts_with("x-"),
            "unknown {label} front matter field `{key}`"
        );
    }
    Ok(())
}

pub(crate) fn validate_unique_non_empty(field: &str, values: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        ensure!(
            !value.trim().is_empty(),
            "{field} cannot contain empty values"
        );
        ensure!(
            seen.insert(value),
            "{field} contains duplicate value `{value}`"
        );
    }
    Ok(())
}

pub(crate) fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
