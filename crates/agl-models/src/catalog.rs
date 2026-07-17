use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use agl_config::{InferencePresetRuntimeConfig, ModelId};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ModelPackageId(String);

impl ModelPackageId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure!(!value.is_empty(), "model package id cannot be empty");
        ensure!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "model package id `{value}` must use lowercase ASCII, digits, or hyphens"
        );
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelPackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ModelPackageId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ModelPackageId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelArtifactRole {
    Main,
    Projector,
    Draft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogCapability {
    Text,
    Tools,
    Vision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileDevice {
    Cpu,
    Gpu,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifact {
    pub role: ModelArtifactRole,
    pub model_id: ModelId,
    pub filename: String,
    pub byte_size: u64,
    pub sha256: String,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRuntimeProfile {
    pub id: String,
    pub device: ProfileDevice,
    pub benchmark_evidence: String,
    pub required_total_ram_bytes: u64,
    pub required_available_ram_bytes: u64,
    pub required_vram_bytes: u64,
    pub gpu_layers: u32,
    pub context_tokens: u32,
    pub batch_size: u32,
    pub ubatch_size: u32,
    pub threads: u32,
    pub smoke_timeout_seconds: u64,
    pub expected_speed: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPackage {
    pub id: ModelPackageId,
    pub display_name: String,
    pub function_id: String,
    pub default: bool,
    pub capabilities: Vec<CatalogCapability>,
    pub license: String,
    pub license_url: String,
    pub repository: String,
    pub revision: String,
    pub artifacts: Vec<ModelArtifact>,
    #[serde(default)]
    pub profiles: Vec<CatalogRuntimeProfile>,
}

impl ModelPackage {
    pub fn required_artifacts(&self) -> impl Iterator<Item = &ModelArtifact> {
        self.artifacts.iter().filter(|artifact| artifact.required)
    }

    pub fn artifact(&self, model_id: &ModelId) -> Option<&ModelArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| &artifact.model_id == model_id)
    }

    pub fn total_required_bytes(&self) -> u64 {
        self.required_artifacts()
            .map(|artifact| artifact.byte_size)
            .sum()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub version: u32,
    pub packages: Vec<ModelPackage>,
}

impl ModelCatalog {
    pub fn builtin() -> Result<Self> {
        Self::from_toml(agl_assets::model_catalog_text())
            .context("embedded model catalog is invalid")
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let catalog: Self = toml::from_str(text).context("failed to parse model catalog")?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported model catalog version {}",
            self.version
        );
        ensure!(!self.packages.is_empty(), "model catalog has no packages");
        ensure!(
            self.packages
                .iter()
                .filter(|package| package.default)
                .count()
                == 1,
            "model catalog must contain exactly one default package"
        );

        let mut package_ids = BTreeSet::new();
        let mut function_ids = BTreeSet::new();
        let mut model_ids = BTreeSet::new();
        for package in &self.packages {
            ensure!(
                package_ids.insert(package.id.clone()),
                "duplicate model package id `{}`",
                package.id
            );
            ensure!(
                function_ids.insert(package.function_id.as_str()),
                "duplicate model package function id `{}`",
                package.function_id
            );
            validate_package(package)?;
            for artifact in &package.artifacts {
                ensure!(
                    model_ids.insert(artifact.model_id.clone()),
                    "duplicate logical model id `{}` in catalog",
                    artifact.model_id
                );
            }
        }
        Ok(())
    }

    pub fn default_package(&self) -> &ModelPackage {
        self.packages
            .iter()
            .find(|package| package.default)
            .expect("validated model catalog has a default")
    }

    pub fn package(&self, id: &ModelPackageId) -> Option<&ModelPackage> {
        self.packages.iter().find(|package| &package.id == id)
    }

    pub fn package_for_model(&self, id: &ModelId) -> Option<&ModelPackage> {
        self.packages
            .iter()
            .find(|package| package.artifact(id).is_some())
    }
}

fn validate_package(package: &ModelPackage) -> Result<()> {
    ensure!(
        !package.display_name.trim().is_empty(),
        "package `{}` display_name cannot be empty",
        package.id
    );
    ensure!(
        package.function_id == package.id.as_str(),
        "package `{}` must map to matching function id",
        package.id
    );
    let Some((owner, repo)) = package.repository.split_once('/') else {
        bail!("package `{}` repository must be OWNER/REPO", package.id);
    };
    ensure!(
        !owner.is_empty()
            && !repo.is_empty()
            && [owner, repo].iter().all(|part| {
                *part != "."
                    && *part != ".."
                    && part.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }),
        "package `{}` repository must be OWNER/REPO",
        package.id
    );
    ensure!(
        package.revision.len() == 40
            && package
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "package `{}` revision must be a full lowercase commit SHA",
        package.id
    );
    ensure!(
        package.license_url.starts_with("https://"),
        "package `{}` license_url must use HTTPS",
        package.id
    );
    ensure!(
        !package.capabilities.is_empty(),
        "package `{}` must declare capabilities",
        package.id
    );
    ensure!(
        package.capabilities.iter().collect::<BTreeSet<_>>().len() == package.capabilities.len(),
        "package `{}` contains duplicate capabilities",
        package.id
    );
    ensure!(
        package
            .artifacts
            .iter()
            .filter(|artifact| artifact.role == ModelArtifactRole::Main)
            .count()
            == 1,
        "package `{}` must contain exactly one main artifact",
        package.id
    );
    ensure!(
        package
            .artifacts
            .iter()
            .any(|artifact| artifact.role == ModelArtifactRole::Main && artifact.required),
        "package `{}` main artifact must be required",
        package.id
    );
    for role in [ModelArtifactRole::Projector, ModelArtifactRole::Draft] {
        ensure!(
            package
                .artifacts
                .iter()
                .filter(|artifact| artifact.role == role)
                .count()
                <= 1,
            "package `{}` contains more than one {role:?} artifact",
            package.id
        );
    }
    if package.capabilities.contains(&CatalogCapability::Vision) {
        ensure!(
            package.artifacts.iter().any(|artifact| {
                artifact.role == ModelArtifactRole::Projector && artifact.required
            }),
            "vision package `{}` must contain a required projector",
            package.id
        );
    }
    let mut filenames = BTreeSet::new();
    for artifact in &package.artifacts {
        ensure!(
            filenames.insert(artifact.filename.as_str()),
            "package `{}` contains duplicate artifact filename `{}`",
            package.id,
            artifact.filename
        );
        ensure!(
            !artifact.filename.is_empty()
                && Path::new(&artifact.filename)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                && artifact.filename.to_ascii_lowercase().ends_with(".gguf"),
            "package `{}` artifact filename `{}` is not a safe GGUF path",
            package.id,
            artifact.filename
        );
        ensure!(
            artifact.byte_size > 4,
            "package `{}` artifact `{}` has invalid byte size",
            package.id,
            artifact.filename
        );
        ensure!(
            artifact.sha256.len() == 64
                && artifact
                    .sha256
                    .bytes()
                    .all(|byte| { byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() }),
            "package `{}` artifact `{}` has invalid SHA-256",
            package.id,
            artifact.filename
        );
    }
    validate_profiles(package)?;
    validate_function_contract(package)
}

fn validate_profiles(package: &ModelPackage) -> Result<()> {
    ensure!(
        !package.profiles.is_empty(),
        "package `{}` has no benchmarked runtime profiles",
        package.id
    );
    ensure!(
        package
            .profiles
            .iter()
            .any(|profile| profile.device == ProfileDevice::Cpu),
        "package `{}` has no benchmarked CPU profile",
        package.id
    );
    let mut ids = BTreeSet::new();
    for profile in &package.profiles {
        ensure!(
            ids.insert(profile.id.as_str()),
            "package `{}` contains duplicate runtime profile `{}`",
            package.id,
            profile.id
        );
        ensure!(
            !profile.benchmark_evidence.trim().is_empty(),
            "package `{}` profile `{}` has no benchmark evidence",
            package.id,
            profile.id
        );
        ensure!(
            agl_assets::model_benchmark_evidence(&profile.benchmark_evidence).is_some(),
            "package `{}` profile `{}` references missing embedded benchmark evidence `{}`",
            package.id,
            profile.id,
            profile.benchmark_evidence
        );
        ensure!(
            profile.required_total_ram_bytes > 0
                && profile.required_available_ram_bytes > 0
                && profile.batch_size > 0
                && profile.ubatch_size > 0
                && profile.threads > 0
                && profile.smoke_timeout_seconds > 0,
            "package `{}` profile `{}` contains zero limits",
            package.id,
            profile.id
        );
        ensure!(
            profile.context_tokens >= agl_config::MIN_AUTO_CONTEXT_TOKENS,
            "package `{}` profile `{}` context {} is below the supported automatic floor {}",
            package.id,
            profile.id,
            profile.context_tokens,
            agl_config::MIN_AUTO_CONTEXT_TOKENS
        );
        ensure!(
            profile.ubatch_size <= profile.batch_size,
            "package `{}` profile `{}` ubatch exceeds batch",
            package.id,
            profile.id
        );
        match profile.device {
            ProfileDevice::Cpu => ensure!(
                profile.gpu_layers == 0 && profile.required_vram_bytes == 0,
                "CPU profile `{}` must not request GPU resources",
                profile.id
            ),
            ProfileDevice::Gpu => ensure!(
                profile.gpu_layers > 0 && profile.required_vram_bytes > 0,
                "GPU profile `{}` must request GPU resources",
                profile.id
            ),
        }
    }
    Ok(())
}

fn validate_function_contract(package: &ModelPackage) -> Result<()> {
    let function = agl_assets::builtin_function(&package.function_id).with_context(|| {
        format!(
            "package `{}` references missing builtin function `{}`",
            package.id, package.function_id
        )
    })?;
    let inference = function
        .inference_config
        .text()
        .context("builtin function inference config is not UTF-8")?;
    let preset = agl_config::load_inference_preset_from_str(&package.function_id, inference)
        .with_context(|| {
            format!(
                "package `{}` function inference preset is invalid",
                package.id
            )
        })?;
    ensure!(
        matches!(preset.runtime, InferencePresetRuntimeConfig::Auto(_)),
        "package `{}` function must use automatic runtime planning",
        package.id
    );
    let main = package
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ModelArtifactRole::Main)
        .expect("package main artifact was validated above");
    ensure!(
        preset.backend.model_id == main.model_id,
        "package `{}` main model id does not match its builtin function",
        package.id
    );
    let projector = package
        .required_artifacts()
        .find(|artifact| artifact.role == ModelArtifactRole::Projector)
        .map(|artifact| &artifact.model_id);
    ensure!(
        preset.backend.multimodal_projector_id.as_ref() == projector,
        "package `{}` projector id does not match its builtin function",
        package.id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_contains_pinned_default_and_required_projector() {
        let catalog = ModelCatalog::builtin().unwrap();
        let package = catalog.default_package();
        assert_eq!(package.id.as_str(), "gemma4-e4b");
        assert_eq!(package.revision.len(), 40);
        assert_eq!(package.required_artifacts().count(), 2);
        assert!(package.artifacts.iter().any(|artifact| {
            artifact.model_id.as_str() == "gemma4-e4b-mmproj"
                && artifact.role == ModelArtifactRole::Projector
                && artifact.required
        }));
    }

    #[test]
    fn package_and_model_ids_are_distinct_types() {
        let package = ModelPackageId::new("gemma4-e4b").unwrap();
        let model = ModelId::new("gemma4-e4b").unwrap();
        assert_eq!(package.as_str(), model.as_str());
    }

    #[test]
    fn catalog_rejects_function_model_mismatch_and_missing_profiles() {
        let mut catalog = ModelCatalog::builtin().unwrap();
        catalog.packages[0].artifacts[0].model_id = ModelId::new("wrong-model").unwrap();
        assert!(catalog.validate().is_err());

        let mut catalog = ModelCatalog::builtin().unwrap();
        catalog.packages[0].profiles.clear();
        assert!(catalog.validate().is_err());
    }

    #[test]
    fn catalog_rejects_automatic_profiles_below_32k() {
        let mut catalog = ModelCatalog::builtin().unwrap();
        catalog.packages[0].profiles[0].context_tokens = agl_config::MIN_AUTO_CONTEXT_TOKENS - 1;
        let error = catalog.validate().unwrap_err();
        assert!(format!("{error:#}").contains("below the supported automatic floor 32768"));
    }

    #[test]
    fn builtin_large_model_profiles_match_live_32k_cpu_admission() {
        let catalog = ModelCatalog::builtin().unwrap();
        let expected = [
            (
                "gemma4-12b",
                "cpu-16gb-32768",
                17_179_869_184,
                14_000_000_000,
                "model-benchmark:20260716-gemma4-12b-32k-cpu",
            ),
            (
                "gemma4-26b",
                "cpu-20gb-32768",
                21_474_836_480,
                20_000_000_000,
                "model-benchmark:20260716-gemma4-26b-32k-cpu",
            ),
            (
                "gemma4-31b",
                "cpu-40gb-32768",
                42_949_672_960,
                40_000_000_000,
                "model-benchmark:20260716-gemma4-31b-32k-cpu",
            ),
        ];

        for (package_id, profile_id, total_ram, available_ram, evidence) in expected {
            let package = catalog
                .package(&ModelPackageId::new(package_id).unwrap())
                .unwrap();
            assert_eq!(package.profiles.len(), 1);
            let profile = &package.profiles[0];
            assert_eq!(profile.id, profile_id);
            assert_eq!(profile.device, ProfileDevice::Cpu);
            assert_eq!(profile.required_total_ram_bytes, total_ram);
            assert_eq!(profile.required_available_ram_bytes, available_ram);
            assert_eq!(profile.context_tokens, 32_768);
            assert_eq!(profile.benchmark_evidence, evidence);
        }
        assert!(catalog.packages.iter().all(|package| {
            package
                .profiles
                .iter()
                .all(|profile| profile.device == ProfileDevice::Cpu)
        }));
    }
}
