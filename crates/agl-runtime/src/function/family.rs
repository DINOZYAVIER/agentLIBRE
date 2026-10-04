//! Function Family declarations: named lists of ready profiles, each an
//! ordinary Function.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::requirement::{FunctionRequirement, parse_function_requirement};

pub const FUNCTION_FAMILY_FILE_NAME: &str = "FAMILY.toml";
pub const FUNCTION_FAMILY_SCHEMA: &str = "agentlibre.function-family/v1";
/// Forge artifact kind under which Function Families are declared.
pub const FUNCTION_FAMILY_KIND: &str = "function-family";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionFamily {
    pub schema: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Id of the profile recommended when the user does not choose one.
    pub default: String,
    pub profiles: Vec<FamilyProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyProfile {
    pub id: String,
    pub name: String,
    /// Typed requirement, written to `chat.default_function` on selection.
    pub function: String,
    #[serde(default)]
    pub min_ram_gb: Option<u64>,
    #[serde(default)]
    pub min_vram_gb: Option<u64>,
}

impl FunctionFamily {
    pub fn parse(document: &str) -> Result<Self> {
        let family: Self = toml::from_str(document).context("invalid FAMILY.toml")?;
        family.validate()?;
        Ok(family)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == FUNCTION_FAMILY_SCHEMA,
            "unsupported Function Family schema {:?}",
            self.schema
        );
        ensure!(
            !self.id.trim().is_empty(),
            "Function Family id cannot be empty"
        );
        ensure!(
            !self.name.trim().is_empty(),
            "Function Family name cannot be empty"
        );
        if let Some(description) = &self.description {
            ensure!(
                !description.trim().is_empty(),
                "Function Family description cannot be empty when present"
            );
        }
        ensure!(
            !self.profiles.is_empty(),
            "Function Family {} declares no profiles",
            self.id
        );
        let mut ids = BTreeSet::new();
        for profile in &self.profiles {
            ensure!(
                !profile.id.trim().is_empty(),
                "profile id cannot be empty in Function Family {}",
                self.id
            );
            ensure!(
                !profile.name.trim().is_empty(),
                "profile {} name cannot be empty",
                profile.id
            );
            ensure!(
                ids.insert(profile.id.as_str()),
                "duplicate profile id {:?} in Function Family {}",
                profile.id,
                self.id
            );
            profile
                .requirement()
                .with_context(|| format!("profile {} has an invalid function", profile.id))?;
            for (field, value) in [
                ("min_ram_gb", profile.min_ram_gb),
                ("min_vram_gb", profile.min_vram_gb),
            ] {
                ensure!(
                    value != Some(0),
                    "profile {} {field} must be a positive integer",
                    profile.id
                );
            }
        }
        ensure!(
            ids.contains(self.default.as_str()),
            "default profile {:?} is not declared in Function Family {}",
            self.default,
            self.id
        );
        Ok(())
    }

    pub fn profile(&self, id: &str) -> Option<&FamilyProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }
}

impl FamilyProfile {
    pub fn requirement(&self) -> Result<FunctionRequirement> {
        parse_function_requirement(&self.function)
    }
}

/// Read every Function Family declared in the configured catalog manifest.
pub fn discover_function_families(catalog_root: impl AsRef<Path>) -> Result<Vec<FunctionFamily>> {
    let catalog_root = catalog_root.as_ref();
    ensure!(
        catalog_root.is_absolute(),
        "Forge catalog root must be absolute"
    );
    let root = catalog_root
        .canonicalize()
        .with_context(|| format!("Forge catalog {} is unavailable", catalog_root.display()))?;
    let artifacts = crate::resolve_workspace_forge(&root)?;
    let mut families = Vec::new();
    for artifact in artifacts
        .declarations
        .iter()
        .filter(|artifact| artifact.kind == FUNCTION_FAMILY_KIND)
    {
        ensure!(
            artifact.schema == FUNCTION_FAMILY_SCHEMA,
            "artifact {} has schema {}; expected {FUNCTION_FAMILY_SCHEMA}",
            artifact.id,
            artifact.schema
        );
        let family = read_family(&artifact.materialized_path)
            .with_context(|| format!("invalid Function Family artifact {}", artifact.id))?;
        ensure!(
            family.id == artifact.id,
            "Function Family artifact {} declares id {}",
            artifact.id,
            family.id
        );
        families.push(family);
    }
    Ok(families)
}

fn read_family(directory: &Path) -> Result<FunctionFamily> {
    let path = directory.join(FUNCTION_FAMILY_FILE_NAME);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "{} must be a regular file",
        path.display()
    );
    let document = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    FunctionFamily::parse(&document)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema = "agentlibre.function-family/v1"
id = "agentlibre.general"
name = "General"
description = "General-purpose profiles."
default = "cpu-small"
[[profiles]]
id = "cpu-small"
name = "CPU Small"
function = "function:agentlibre.cpu-small@^1.0"
min_ram_gb = 16
[[profiles]]
id = "gpu-small"
name = "GPU Small"
function = "function:agentlibre.gpu-small@^1.0"
min_vram_gb = 12
"#;

    #[test]
    fn parses_valid_family() {
        let family = FunctionFamily::parse(VALID).unwrap();
        assert_eq!(family.profiles.len(), 2);
        assert_eq!(family.profile("gpu-small").unwrap().min_vram_gb, Some(12));
        assert_eq!(family.profile("cpu-small").unwrap().min_vram_gb, None);
    }

    fn rejects(document: &str, needle: &str) {
        let error = format!("{:#}", FunctionFamily::parse(document).unwrap_err());
        assert!(error.contains(needle), "{error}");
    }

    #[test]
    fn rejects_unknown_fields() {
        rejects(&format!("extra = 1\n{VALID}"), "unknown field");
        rejects(
            &VALID.replace("min_ram_gb = 16", "min_ram_gb = 16\nmin_gpu = 1"),
            "unknown field",
        );
    }

    #[test]
    fn rejects_wrong_schema() {
        rejects(
            &VALID.replace("function-family/v1", "function-family/v2"),
            "schema",
        );
    }

    #[test]
    fn rejects_missing_default_profile() {
        rejects(
            &VALID.replace("default = \"cpu-small\"", "default = \"nope\""),
            "default profile",
        );
    }

    #[test]
    fn rejects_empty_and_duplicate_profiles() {
        rejects(
            "schema = \"agentlibre.function-family/v1\"\nid = \"a\"\nname = \"A\"\ndefault = \"x\"\nprofiles = []\n",
            "no profiles",
        );
        rejects(
            &VALID.replace("id = \"gpu-small\"", "id = \"cpu-small\""),
            "duplicate profile id",
        );
    }

    #[test]
    fn rejects_bad_requirement() {
        for bad in [
            "agentlibre.cpu-small@^1.0",
            "function:agentlibre.cpu-small",
            "function:agentlibre.cpu-small@",
            "function:agentlibre.cpu-small@not-a-version",
        ] {
            rejects(
                &VALID.replace("function:agentlibre.cpu-small@^1.0", bad),
                "invalid function",
            );
        }
    }

    #[test]
    fn rejects_non_positive_minimums() {
        rejects(
            &VALID.replace("min_ram_gb = 16", "min_ram_gb = 0"),
            "positive",
        );
        rejects(
            &VALID.replace("min_vram_gb = 12", "min_vram_gb = 0"),
            "positive",
        );
        assert!(
            FunctionFamily::parse(&VALID.replace("min_ram_gb = 16", "min_ram_gb = -1")).is_err()
        );
    }
}
