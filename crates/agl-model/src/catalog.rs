use std::collections::BTreeSet;
use std::path::{Component, Path};

use agl_config::ModelId;
use agl_package::{
    PackageId, PackageRef, PackageSourceId, PackageSourceKind, PackageSourceTier, PackageTreeDigest,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub type ModelPackageId = PackageId;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
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
pub struct ModelArtifactFile {
    pub filename: String,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifact {
    pub role: ModelArtifactRole,
    pub model_id: ModelId,
    pub files: Vec<ModelArtifactFile>,
    pub required: bool,
}

impl ModelArtifact {
    pub fn primary_file(&self) -> &ModelArtifactFile {
        &self.files[0]
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.byte_size).sum()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRuntimeProfile {
    pub id: String,
    pub device: ProfileDevice,
    pub pci_device_id: Option<String>,
    pub pci_subsystem_id: Option<String>,
    pub benchmark_evidence: String,
    pub required_total_ram_bytes: u64,
    pub host_private_bytes: u64,
    pub device_private_bytes: u64,
    pub shared_bytes: u64,
    pub decoder_scratch_bytes: u64,
    pub gpu_layers: u32,
    pub context_tokens: u32,
    pub batch_size: u32,
    pub ubatch_size: u32,
    pub threads: u32,
    pub flash_attention: bool,
    pub cache_type_k: agl_config::KvCacheType,
    pub cache_type_v: agl_config::KvCacheType,
    pub mmap: bool,
    pub unified_kv: bool,
    pub slot_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtp: Option<CatalogMtpProfile>,
    pub smoke_timeout_seconds: u64,
    pub expected_speed: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogMtpProfile {
    pub max_draft_tokens: u32,
    pub min_draft_tokens: u32,
    pub p_min_millionths: u32,
    pub gpu_layers: u32,
    pub cache_type_k: agl_config::KvCacheType,
    pub cache_type_v: agl_config::KvCacheType,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPackage {
    pub id: ModelPackageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ModelPackageProvenance>,
    pub display_name: String,
    pub capabilities: Vec<CatalogCapability>,
    pub license: String,
    pub license_url: String,
    pub repository: String,
    pub revision: String,
    pub artifacts: Vec<ModelArtifact>,
    #[serde(default)]
    pub profiles: Vec<CatalogRuntimeProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPackageProvenance {
    pub reference: PackageRef,
    pub source_id: PackageSourceId,
    pub source_tier: PackageSourceTier,
    pub source_kind: PackageSourceKind,
    pub package_tree_digest: PackageTreeDigest,
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
            .map(ModelArtifact::total_bytes)
            .sum()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub packages: Vec<ModelPackage>,
}

impl ModelCatalog {
    pub fn from_builtin_resolved() -> Result<Self> {
        let packages = crate::adapter::resolved_builtin_model_packages()?;
        let catalog = Self { packages };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.packages.is_empty(), "model catalog has no packages");

        let mut package_ids = BTreeSet::new();
        let mut model_ids = BTreeSet::new();
        for package in &self.packages {
            ensure!(
                package_ids.insert(package.id.clone()),
                "duplicate model package id `{}`",
                package.id
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
    let draft = package
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ModelArtifactRole::Draft && artifact.required);
    for profile in &package.profiles {
        ensure!(
            draft.is_some() == profile.mtp.is_some(),
            "package `{}` profile `{}` must bind Draft artifact and MTP shape together",
            package.id,
            profile.id
        );
        if let Some(mtp) = &profile.mtp {
            ensure!(
                !package.capabilities.contains(&CatalogCapability::Vision),
                "package `{}` profile `{}` cannot combine vision and MTP",
                package.id,
                profile.id
            );
            ensure!(
                (1..=64).contains(&mtp.max_draft_tokens)
                    && mtp.min_draft_tokens <= mtp.max_draft_tokens
                    && mtp.p_min_millionths <= 1_000_000,
                "package `{}` profile `{}` has an invalid MTP shape",
                package.id,
                profile.id
            );
        }
    }
    let mut filenames = BTreeSet::new();
    for artifact in &package.artifacts {
        ensure!(
            !artifact.files.is_empty(),
            "package `{}` artifact role `{:?}` has no files",
            package.id,
            artifact.role
        );
        for file in &artifact.files {
            ensure!(
                filenames.insert(file.filename.as_str()),
                "package `{}` contains duplicate artifact filename `{}`",
                package.id,
                file.filename
            );
            ensure!(
                !file.filename.is_empty()
                    && Path::new(&file.filename).components().count() == 1
                    && Path::new(&file.filename)
                        .components()
                        .all(|component| matches!(component, Component::Normal(_)))
                    && file.filename.to_ascii_lowercase().ends_with(".gguf"),
                "package `{}` artifact filename `{}` is not a safe GGUF basename",
                package.id,
                file.filename
            );
            ensure!(
                file.byte_size > 4,
                "package `{}` artifact `{}` has invalid byte size",
                package.id,
                file.filename
            );
            ensure!(
                file.sha256.len() == 64
                    && file
                        .sha256
                        .bytes()
                        .all(|byte| { byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() }),
                "package `{}` artifact `{}` has invalid SHA-256",
                package.id,
                file.filename
            );
        }
    }
    validate_profiles(package)
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
            profile.required_total_ram_bytes > 0
                && profile.host_private_bytes > 0
                && profile.batch_size > 0
                && profile.ubatch_size > 0
                && profile.threads > 0
                && profile.slot_count == 1
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
            ProfileDevice::Cpu => {
                ensure!(
                    profile.gpu_layers == 0 && profile.device_private_bytes == 0,
                    "CPU profile `{}` must not request GPU resources",
                    profile.id
                );
                ensure!(
                    profile.pci_device_id.is_none() && profile.pci_subsystem_id.is_none(),
                    "CPU profile `{}` must not declare PCI GPU identity",
                    profile.id
                );
            }
            ProfileDevice::Gpu => {
                ensure!(
                    profile.gpu_layers > 0 && profile.device_private_bytes > 0,
                    "GPU profile `{}` must request GPU resources",
                    profile.id
                );
                let pci_device_id = profile.pci_device_id.as_deref().with_context(|| {
                    format!(
                        "GPU profile `{}` has no exact PCI device identity",
                        profile.id
                    )
                })?;
                let pci_subsystem_id = profile.pci_subsystem_id.as_deref().with_context(|| {
                    format!(
                        "GPU profile `{}` has no exact PCI subsystem identity",
                        profile.id
                    )
                })?;
                ensure!(
                    is_canonical_pci_id(pci_device_id) && is_canonical_pci_id(pci_subsystem_id),
                    "GPU profile `{}` has malformed PCI identity",
                    profile.id
                );
            }
        }
    }
    Ok(())
}

fn is_canonical_pci_id(value: &str) -> bool {
    value.len() == 9
        && value.as_bytes()[4] == b':'
        && value.bytes().enumerate().all(|(index, byte)| {
            index == 4 || byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_contains_pinned_default_and_required_projector() {
        let catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let package = catalog
            .package(&ModelPackageId::new("gemma4-e4b").unwrap())
            .unwrap();
        let provenance = package
            .provenance
            .as_ref()
            .expect("resolved catalog package carries common provenance");
        assert_eq!(provenance.reference.to_string(), "model:gemma4-e4b@=1.2.0");
        assert_eq!(provenance.source_id.as_str(), "builtin");
        assert!(
            provenance
                .package_tree_digest
                .as_str()
                .starts_with("sha256:")
        );
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
    fn catalog_rejects_missing_profiles() {
        let mut catalog = ModelCatalog::from_builtin_resolved().unwrap();
        catalog.packages[0].profiles.clear();
        assert!(catalog.validate().is_err());
    }

    #[test]
    fn catalog_rejects_automatic_profiles_below_32k() {
        let mut catalog = ModelCatalog::from_builtin_resolved().unwrap();
        catalog.packages[0].profiles[0].context_tokens = agl_config::MIN_AUTO_CONTEXT_TOKENS - 1;
        let error = catalog.validate().unwrap_err();
        assert!(format!("{error:#}").contains("below the supported automatic floor 32768"));
    }

    #[test]
    fn catalog_rejects_profiles_below_the_runtime_floor() {
        let mut catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let e2b = catalog
            .packages
            .iter_mut()
            .find(|package| package.id.as_str() == "gemma4-e2b")
            .unwrap();
        e2b.profiles[0].context_tokens = agl_config::MIN_AUTO_CONTEXT_TOKENS - 1;
        let error = catalog.validate().unwrap_err();
        assert!(format!("{error:#}").contains("below the supported automatic floor 32768"));
    }

    #[test]
    fn catalog_rejects_gpu_profiles_without_exact_pci_identity() {
        let mut catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let gpu = catalog.packages[0]
            .profiles
            .iter_mut()
            .find(|profile| profile.device == ProfileDevice::Gpu)
            .unwrap();
        gpu.pci_subsystem_id = None;

        let error = catalog.validate().unwrap_err();
        assert!(format!("{error:#}").contains("has no exact PCI subsystem identity"));
    }

    #[test]
    fn builtin_catalog_contains_five_independent_packages() {
        let catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let package_ids = catalog
            .packages
            .iter()
            .map(|package| package.id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            package_ids,
            BTreeSet::from([
                "gemma4-12b",
                "gemma4-26b",
                "gemma4-31b",
                "gemma4-e2b",
                "gemma4-e4b",
            ])
        );
        assert_eq!(catalog.packages.len(), 5);
        assert!(catalog.packages.iter().all(|package| {
            package
                .artifacts
                .iter()
                .any(|artifact| artifact.model_id.as_str() == package.id.as_str())
        }));
    }

    #[test]
    fn official_e2b_and_31b_artifact_pins_are_exact() {
        let catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let expected = [
            (
                "gemma4-e2b",
                "google/gemma-4-E2B-it-qat-q4_0-gguf",
                "675cff42a74c774d6cb76f76d8eacb49b48c9b93",
                "gemma-4-E2B_q4_0-it.gguf",
                3_349_514_112,
                "3646b4c147cd235a44d91df1546d3b7d8e29b547dbe4e1f80856419aa455e6fd",
            ),
            (
                "gemma4-31b",
                "google/gemma-4-31B-it-qat-q4_0-gguf",
                "59dde24573e7e61570dba08b18a2e1fe246955ed",
                "gemma-4-31B_q4_0-it.gguf",
                17_651_001_568,
                "179cfb99212709597eae5929112cfca677e1bbf566178b479ae1da0c4772874b",
            ),
        ];

        for (package_id, repository, revision, filename, byte_size, sha256) in expected {
            let package = catalog
                .package(&ModelPackageId::new(package_id).unwrap())
                .unwrap();
            assert_eq!(package.repository, repository);
            assert_eq!(package.revision, revision);
            assert_eq!(package.artifacts.len(), 1);
            let main = &package.artifacts[0];
            let main_file = main.primary_file();
            assert_eq!(main.role, ModelArtifactRole::Main);
            assert_eq!(main.model_id.as_str(), package_id);
            assert_eq!(main_file.filename, filename);
            assert_eq!(main_file.byte_size, byte_size);
            assert_eq!(main_file.sha256, sha256);
            assert!(main.required);
            assert_eq!(
                package.capabilities,
                vec![CatalogCapability::Text, CatalogCapability::Tools]
            );
        }
    }

    #[test]
    fn official_e2b_and_31b_cannot_be_cross_bound() {
        let mut catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let e2b_index = catalog
            .packages
            .iter()
            .position(|package| package.id.as_str() == "gemma4-e2b")
            .unwrap();
        let thirty_one_index = catalog
            .packages
            .iter()
            .position(|package| package.id.as_str() == "gemma4-31b")
            .unwrap();
        let e2b_model = catalog.packages[e2b_index].artifacts[0].model_id.clone();
        catalog.packages[thirty_one_index].artifacts[0].model_id = e2b_model;

        let error = catalog.validate().unwrap_err();
        assert!(format!("{error:#}").contains("duplicate logical model id"));
    }

    #[test]
    fn builtin_profiles_match_five_function_cpu_and_gpu_matrix() {
        let catalog = ModelCatalog::from_builtin_resolved().unwrap();
        let expected = [
            (
                "gemma4-e2b",
                "cpu-8gb-32768",
                32_768,
                "gpu-rx7900xtx-32768",
                32_768,
                3_753_902_080,
                "evidence/20260723-five-gemma4-rx7900xtx.md",
            ),
            (
                "gemma4-e4b",
                "cpu-8gb-32768",
                32_768,
                "gpu-rx7900xtx-32768",
                32_768,
                6_459_228_160,
                "evidence/20260723-five-gemma4-rx7900xtx.md",
            ),
            (
                "gemma4-12b",
                "cpu-16gb-65536",
                65_536,
                "gpu-rx7900xtx-65536",
                65_536,
                9_982_443_520,
                "evidence/20260723-five-gemma4-rx7900xtx.md",
            ),
            (
                "gemma4-26b",
                "cpu-20gb-32768",
                32_768,
                "gpu-rx7900xtx-32768",
                32_768,
                17_165_189_120,
                "evidence/20260723-five-gemma4-rx7900xtx.md",
            ),
        ];

        for (
            package_id,
            cpu_profile_id,
            cpu_context,
            gpu_profile_id,
            gpu_context,
            required_vram,
            benchmark_evidence,
        ) in expected
        {
            let package = catalog
                .package(&ModelPackageId::new(package_id).unwrap())
                .unwrap();
            assert_eq!(package.profiles.len(), 2);
            let cpu = package
                .profiles
                .iter()
                .find(|profile| profile.device == ProfileDevice::Cpu)
                .unwrap();
            assert_eq!(cpu.id, cpu_profile_id);
            assert_eq!(cpu.context_tokens, cpu_context);
            assert_eq!(cpu.device_private_bytes, 0);
            assert_eq!(cpu.gpu_layers, 0);

            let gpu = package
                .profiles
                .iter()
                .find(|profile| profile.device == ProfileDevice::Gpu)
                .unwrap();
            assert_eq!(gpu.id, gpu_profile_id);
            assert_eq!(gpu.context_tokens, gpu_context);
            assert_eq!(gpu.device_private_bytes, required_vram);
            assert_eq!(gpu.gpu_layers, 999);
            assert_eq!(gpu.pci_device_id.as_deref(), Some("1002:744c"));
            assert_eq!(gpu.pci_subsystem_id.as_deref(), Some("1da2:471e"));
            assert_eq!(gpu.batch_size, 512);
            assert_eq!(gpu.ubatch_size, 256);
            assert_eq!(gpu.threads, 8);
            assert_eq!(gpu.benchmark_evidence, benchmark_evidence);
        }

        let thirty_one_b = catalog
            .package(&ModelPackageId::new("gemma4-31b").unwrap())
            .unwrap();
        assert_eq!(thirty_one_b.profiles.len(), 3);
        let cpu = &thirty_one_b.profiles[0];
        assert_eq!(cpu.id, "cpu-40gb-32768");
        assert_eq!(cpu.device, ProfileDevice::Cpu);
        assert_eq!(cpu.context_tokens, 32_768);
        let gpu_profiles = thirty_one_b
            .profiles
            .iter()
            .filter(|profile| profile.device == ProfileDevice::Gpu)
            .collect::<Vec<_>>();
        assert_eq!(gpu_profiles.len(), 2);
        for (profile, id, context, required_vram, evidence) in [
            (
                gpu_profiles[0],
                "gpu-rx7900xtx-32768",
                32_768,
                22_041_067_520,
                "evidence/20260723-five-gemma4-rx7900xtx.md",
            ),
            (
                gpu_profiles[1],
                "gpu-rx7900xtx-65536",
                65_536,
                23_488_102_400,
                "evidence/20260727-gemma4-31b-64k-rx7900xtx.md",
            ),
        ] {
            assert_eq!(profile.id, id);
            assert_eq!(profile.context_tokens, context);
            assert_eq!(profile.device_private_bytes, required_vram);
            assert_eq!(profile.gpu_layers, 999);
            assert_eq!(profile.pci_device_id.as_deref(), Some("1002:744c"));
            assert_eq!(profile.pci_subsystem_id.as_deref(), Some("1da2:471e"));
            assert_eq!(profile.batch_size, 512);
            assert_eq!(profile.ubatch_size, 256);
            assert_eq!(profile.benchmark_evidence, evidence);
        }
    }
}
