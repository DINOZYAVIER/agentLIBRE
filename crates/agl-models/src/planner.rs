use std::path::{Path, PathBuf};

use agl_config::{
    BoundInferencePreset, InferenceBackendConfig, InferencePresetRuntimeConfig,
    InferenceRuntimeConfig, MIN_AUTO_CONTEXT_TOKENS, MtpRuntimeConfig, ResolvedInferenceConfig,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};

use crate::{CatalogRuntimeProfile, ModelPackage, ProfileDevice};

pub const RECOMMENDED_MEMORY_FLOOR_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuResources {
    pub physical_cores: usize,
    pub logical_cores: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskResources {
    pub path: PathBuf,
    pub mount_point: PathBuf,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlamaDeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlamaDeviceInfo {
    pub name: String,
    pub description: String,
    pub kind: LlamaDeviceKind,
    pub free_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub usable: bool,
    pub supports_gpu_offload: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostResources {
    pub detected_total_memory_bytes: u64,
    pub nominal_memory_class_bytes: u64,
    pub available_memory_bytes: u64,
    pub cpu: CpuResources,
    pub disk: DiskResources,
    pub devices: Vec<LlamaDeviceInfo>,
}

impl HostResources {
    pub fn inspect(cache_path: impl AsRef<Path>, devices: Vec<LlamaDeviceInfo>) -> Result<Self> {
        validate_device_snapshots(&devices)?;
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_memory(MemoryRefreshKind::everything())
                .with_cpu(CpuRefreshKind::everything()),
        );
        let total = system.total_memory();
        let logical = system.cpus().len().max(1);
        let physical = System::physical_core_count().unwrap_or(logical).max(1);
        Ok(Self {
            detected_total_memory_bytes: total,
            nominal_memory_class_bytes: nominal_memory_class(total),
            available_memory_bytes: system.available_memory(),
            cpu: CpuResources {
                physical_cores: physical,
                logical_cores: logical,
            },
            disk: inspect_disk(cache_path.as_ref())?,
            devices,
        })
    }

    pub fn below_recommended_floor(&self) -> bool {
        self.nominal_memory_class_bytes < RECOMMENDED_MEMORY_FLOOR_BYTES
    }
}

fn validate_device_snapshots(devices: &[LlamaDeviceInfo]) -> Result<()> {
    for device in devices.iter().filter(|device| {
        device.usable
            && device.supports_gpu_offload
            && matches!(
                device.kind,
                LlamaDeviceKind::DiscreteGpu | LlamaDeviceKind::IntegratedGpu
            )
    }) {
        ensure!(
            !device.name.is_empty()
                && device.name.len() <= 256
                && !device.name.chars().any(char::is_control),
            "GPU device identity is invalid"
        );
        ensure!(
            device.total_memory_bytes > 0,
            "GPU device `{}` reports zero total memory",
            device.name
        );
        ensure!(
            device.free_memory_bytes <= device.total_memory_bytes,
            "GPU device `{}` reports {} free bytes above its {} total bytes",
            device.name,
            device.free_memory_bytes,
            device.total_memory_bytes
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFitKind {
    Recommended,
    Fits,
    Slow,
    InsufficientMemory,
    InsufficientDisk,
    UnsupportedBackend,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFit {
    pub kind: ModelFitKind,
    pub reason: String,
    pub profile_id: Option<String>,
    pub selected_device: Option<String>,
    pub bytes_to_download: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlan {
    pub profile_id: String,
    pub selected_device: Option<String>,
    pub runtime: InferenceRuntimeConfig,
    pub smoke_timeout_seconds: u64,
    pub expected_speed: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlanSet {
    pub selected: RuntimePlan,
    pub cpu_fallback: Option<RuntimePlan>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuFallbackOffer {
    pub gpu_failure: String,
    pub cpu_plan: RuntimePlan,
    pub memory_fit: String,
    pub context_tokens: u32,
    pub expected_speed: String,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimePlanner;

impl RuntimePlanner {
    pub fn plan_set(
        &self,
        package: &ModelPackage,
        host: &HostResources,
        policy: &agl_config::AutoRuntimePolicy,
        allow_low_memory: bool,
    ) -> Result<RuntimePlanSet> {
        let selected = self.plan(package, host, policy, allow_low_memory)?;
        let cpu_fallback = if selected.runtime.gpu_layers > 0 {
            self.cpu_plan(package, host, policy, allow_low_memory).ok()
        } else {
            None
        };
        Ok(RuntimePlanSet {
            selected,
            cpu_fallback,
        })
    }

    pub fn cpu_plan(
        &self,
        package: &ModelPackage,
        host: &HostResources,
        policy: &agl_config::AutoRuntimePolicy,
        allow_low_memory: bool,
    ) -> Result<RuntimePlan> {
        policy.validate()?;
        let profile = select_cpu_profile(package, host, allow_low_memory).with_context(|| {
            format!(
                "model `{}` has no benchmarked CPU profile that fits current memory",
                package.id
            )
        })?;
        runtime_plan_from_profile(profile, None, host, policy)
    }

    pub fn cpu_fallback_offer(
        &self,
        package: &ModelPackage,
        host: &HostResources,
        policy: &agl_config::AutoRuntimePolicy,
        allow_low_memory: bool,
        gpu_failure: impl Into<String>,
    ) -> Result<CpuFallbackOffer> {
        let cpu_plan = self.cpu_plan(package, host, policy, allow_low_memory)?;
        let cpu_profile = package
            .profiles
            .iter()
            .find(|profile| profile.id == cpu_plan.profile_id)
            .context("CPU fallback profile disappeared from catalog")?;
        let measured_fit = host.detected_total_memory_bytes >= cpu_profile.required_total_ram_bytes
            && host.available_memory_bytes >= cpu_profile.required_available_ram_bytes;
        Ok(CpuFallbackOffer {
            gpu_failure: gpu_failure.into(),
            memory_fit: if measured_fit {
                "benchmarked CPU profile fits current system memory".to_string()
            } else {
                "best-effort low-memory override: current RAM is below the benchmarked CPU gate"
                    .to_string()
            },
            context_tokens: cpu_plan.runtime.context_tokens,
            expected_speed: cpu_plan.expected_speed.clone(),
            cpu_plan,
        })
    }

    pub fn fit(
        &self,
        package: &ModelPackage,
        host: &HostResources,
        bytes_to_download: u64,
        allow_low_memory: bool,
    ) -> ModelFit {
        if bytes_to_download > host.disk.available_bytes {
            return ModelFit {
                kind: ModelFitKind::InsufficientDisk,
                reason: format!(
                    "download needs {bytes_to_download} bytes but only {} bytes are available",
                    host.disk.available_bytes
                ),
                profile_id: None,
                selected_device: None,
                bytes_to_download,
            };
        }
        if host.below_recommended_floor() && !allow_low_memory {
            return ModelFit {
                kind: ModelFitKind::InsufficientMemory,
                reason: "detected memory is below the recommended 8 GB minimum".to_string(),
                profile_id: None,
                selected_device: None,
                bytes_to_download,
            };
        }
        if package.profiles.is_empty() {
            return ModelFit {
                kind: ModelFitKind::UnsupportedBackend,
                reason: "package has no benchmarked runtime profile".to_string(),
                profile_id: None,
                selected_device: None,
                bytes_to_download,
            };
        }

        if let Some((profile, device)) = select_gpu_profile(package, host, allow_low_memory) {
            return ModelFit {
                kind: if package.default {
                    ModelFitKind::Recommended
                } else {
                    ModelFitKind::Fits
                },
                reason: if allow_low_memory
                    && (host.detected_total_memory_bytes < profile.required_total_ram_bytes
                        || host.available_memory_bytes < profile.required_available_ram_bytes)
                {
                    format!(
                        "best-effort low-memory override selects benchmarked GPU profile for {}",
                        device.name
                    )
                } else {
                    format!("benchmarked GPU profile fits {}", device.name)
                },
                profile_id: Some(profile.id.clone()),
                selected_device: Some(device.name.clone()),
                bytes_to_download,
            };
        }

        if let Some(profile) = select_cpu_profile(package, host, allow_low_memory) {
            let kind = if package.default {
                ModelFitKind::Recommended
            } else if profile.expected_speed == "slow" {
                ModelFitKind::Slow
            } else {
                ModelFitKind::Fits
            };
            return ModelFit {
                kind,
                reason: if allow_low_memory
                    && (host.detected_total_memory_bytes < profile.required_total_ram_bytes
                        || host.available_memory_bytes < profile.required_available_ram_bytes)
                {
                    "best-effort low-memory override selects a benchmarked CPU profile despite RAM gates"
                        .to_string()
                } else {
                    "benchmarked CPU profile fits available system memory".to_string()
                },
                profile_id: Some(profile.id.clone()),
                selected_device: None,
                bytes_to_download,
            };
        }

        ModelFit {
            kind: ModelFitKind::InsufficientMemory,
            reason: "no benchmarked profile fits current RAM or VRAM availability".to_string(),
            profile_id: None,
            selected_device: None,
            bytes_to_download,
        }
    }

    pub fn plan(
        &self,
        package: &ModelPackage,
        host: &HostResources,
        policy: &agl_config::AutoRuntimePolicy,
        allow_low_memory: bool,
    ) -> Result<RuntimePlan> {
        policy.validate()?;
        let fit = self.fit(package, host, 0, allow_low_memory);
        ensure!(
            matches!(
                fit.kind,
                ModelFitKind::Recommended | ModelFitKind::Fits | ModelFitKind::Slow
            ),
            "model `{}` does not fit: {}",
            package.id,
            fit.reason
        );
        let profile = package
            .profiles
            .iter()
            .find(|profile| Some(profile.id.as_str()) == fit.profile_id.as_deref())
            .context("selected runtime profile disappeared from catalog")?;
        runtime_plan_from_profile(profile, fit.selected_device, host, policy)
    }

    pub fn resolve_bound(
        &self,
        bound: BoundInferencePreset,
        package: &ModelPackage,
        host: &HostResources,
        allow_low_memory: bool,
    ) -> Result<(ResolvedInferenceConfig, RuntimePlan)> {
        let InferencePresetRuntimeConfig::Auto(policy) = &bound.runtime else {
            bail!("resolve_bound requires an automatic runtime preset");
        };
        ensure!(
            package.artifact(&bound.backend.model_id).is_some(),
            "model `{}` is not part of catalog package `{}`",
            bound.backend.model_id,
            package.id
        );
        let plan = self.plan(package, host, policy, allow_low_memory)?;
        let resolved = ResolvedInferenceConfig {
            backend: InferenceBackendConfig {
                kind: bound.backend.kind,
                model: bound.backend.model,
                multimodal_projector: bound.backend.multimodal_projector,
            },
            runtime: plan.runtime.clone(),
            model: bound.model,
            prompt: bound.prompt,
        };
        resolved.validate()?;
        Ok((resolved, plan))
    }

    pub fn resolve_bound_with_plan(
        &self,
        bound: BoundInferencePreset,
        package: &ModelPackage,
        host: &HostResources,
        allow_low_memory: bool,
        plan: &RuntimePlan,
    ) -> Result<ResolvedInferenceConfig> {
        let InferencePresetRuntimeConfig::Auto(policy) = &bound.runtime else {
            bail!("resolve_bound_with_plan requires an automatic runtime preset");
        };
        let selected = self.plan(package, host, policy, allow_low_memory)?;
        let cpu = self.cpu_plan(package, host, policy, allow_low_memory).ok();
        ensure!(
            plan == &selected || cpu.as_ref() == Some(plan),
            "explicit runtime plan is not a current benchmarked plan for `{}`",
            package.id
        );
        let resolved = ResolvedInferenceConfig {
            backend: InferenceBackendConfig {
                kind: bound.backend.kind,
                model: bound.backend.model,
                multimodal_projector: bound.backend.multimodal_projector,
            },
            runtime: plan.runtime.clone(),
            model: bound.model,
            prompt: bound.prompt,
        };
        resolved.validate()?;
        Ok(resolved)
    }
}

fn runtime_plan_from_profile(
    profile: &CatalogRuntimeProfile,
    selected_device: Option<String>,
    host: &HostResources,
    policy: &agl_config::AutoRuntimePolicy,
) -> Result<RuntimePlan> {
    ensure!(
        profile.context_tokens >= MIN_AUTO_CONTEXT_TOKENS,
        "runtime profile `{}` context {} is below the supported automatic floor {}",
        profile.id,
        profile.context_tokens,
        MIN_AUTO_CONTEXT_TOKENS
    );
    ensure!(
        policy.max_context_tokens >= profile.context_tokens,
        "automatic context ceiling {} is below benchmarked profile `{}` context {}",
        policy.max_context_tokens,
        profile.id,
        profile.context_tokens
    );
    let runtime = InferenceRuntimeConfig {
        gpu_layers: profile.gpu_layers,
        context_tokens: profile.context_tokens,
        threads: profile
            .threads
            .min(u32::try_from(host.cpu.physical_cores).unwrap_or(u32::MAX))
            .max(1),
        device: selected_device.clone(),
        batch_size: Some(profile.batch_size.min(policy.max_batch_size)),
        ubatch_size: Some(profile.ubatch_size.min(policy.max_ubatch_size)),
        flash_attention: Some(policy.flash_attention),
        cache_type_k: Some(policy.cache_type_k),
        cache_type_v: Some(policy.cache_type_v),
        mmap: Some(true),
        kv_unified: Some(true),
        mtp: MtpRuntimeConfig::default(),
    };
    runtime.validate()?;
    Ok(RuntimePlan {
        profile_id: profile.id.clone(),
        selected_device,
        runtime,
        smoke_timeout_seconds: profile.smoke_timeout_seconds,
        expected_speed: profile.expected_speed.clone(),
    })
}

fn select_gpu_profile<'a>(
    package: &'a ModelPackage,
    host: &'a HostResources,
    allow_low_memory: bool,
) -> Option<(&'a CatalogRuntimeProfile, &'a LlamaDeviceInfo)> {
    for profile in package
        .profiles
        .iter()
        .filter(|profile| profile.device == ProfileDevice::Gpu)
    {
        for device in host.devices.iter().filter(|device| {
            device.usable
                && device.supports_gpu_offload
                && matches!(
                    device.kind,
                    LlamaDeviceKind::DiscreteGpu | LlamaDeviceKind::IntegratedGpu
                )
        }) {
            let vram_fits = device.free_memory_bytes >= profile.required_vram_bytes;
            let ram_fits = host.detected_total_memory_bytes >= profile.required_total_ram_bytes
                && host.available_memory_bytes >= profile.required_available_ram_bytes;
            let unified_safe = device.kind != LlamaDeviceKind::IntegratedGpu
                || host.available_memory_bytes
                    >= profile
                        .required_available_ram_bytes
                        .saturating_add(profile.required_vram_bytes);
            if vram_fits && (allow_low_memory || ram_fits) && unified_safe {
                return Some((profile, device));
            }
        }
    }
    None
}

fn select_cpu_profile<'a>(
    package: &'a ModelPackage,
    host: &HostResources,
    allow_low_memory: bool,
) -> Option<&'a CatalogRuntimeProfile> {
    package
        .profiles
        .iter()
        .filter(|profile| profile.device == ProfileDevice::Cpu)
        .find(|profile| {
            allow_low_memory
                || (host.detected_total_memory_bytes >= profile.required_total_ram_bytes
                    && host.available_memory_bytes >= profile.required_available_ram_bytes)
        })
}

fn nominal_memory_class(total: u64) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    total.saturating_add(GIB - 1) / GIB * GIB
}

fn inspect_disk(path: &Path) -> Result<DiskResources> {
    let probe = nearest_existing_ancestor(path)?;
    let disks = Disks::new_with_refreshed_list();
    let disk = disks
        .list()
        .iter()
        .filter(|disk| probe.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .with_context(|| format!("failed to find filesystem for {}", probe.display()))?;
    Ok(DiskResources {
        path: path.to_path_buf(),
        mount_point: disk.mount_point().to_path_buf(),
        total_bytes: disk.total_space(),
        available_bytes: disk.available_space(),
    })
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    while !current.exists() {
        current = current
            .parent()
            .context("disk probe path has no existing ancestor")?
            .to_path_buf();
    }
    std::fs::canonicalize(&current)
        .with_context(|| format!("failed to canonicalize disk probe {}", current.display()))
}

#[cfg(test)]
mod tests {
    use agl_config::{KvCacheType, RuntimeSwitch};

    use super::*;
    use crate::{CatalogCapability, ModelArtifact, ModelArtifactRole, ModelPackageId};

    fn package() -> ModelPackage {
        ModelPackage {
            id: ModelPackageId::new("gemma4-e4b").unwrap(),
            display_name: "Gemma 4 E4B".to_string(),
            function_id: "gemma4-e4b".to_string(),
            default: true,
            capabilities: vec![CatalogCapability::Text],
            license: "apache-2.0".to_string(),
            license_url: "https://example.com/license".to_string(),
            repository: "owner/repo".to_string(),
            revision: "a".repeat(40),
            artifacts: vec![ModelArtifact {
                role: ModelArtifactRole::Main,
                model_id: agl_config::ModelId::new("gemma4-e4b").unwrap(),
                filename: "model.gguf".to_string(),
                byte_size: 4_000_000_000,
                sha256: "b".repeat(64),
                required: true,
            }],
            profiles: vec![CatalogRuntimeProfile {
                id: "cpu-8gb".to_string(),
                device: ProfileDevice::Cpu,
                benchmark_evidence: "fixture".to_string(),
                required_total_ram_bytes: 7_000_000_000,
                required_available_ram_bytes: 4_000_000_000,
                required_vram_bytes: 0,
                gpu_layers: 0,
                context_tokens: 32768,
                batch_size: 256,
                ubatch_size: 128,
                threads: 4,
                smoke_timeout_seconds: 300,
                expected_speed: "slow".to_string(),
            }],
        }
    }

    fn package_with_gpu_profile() -> ModelPackage {
        let mut package = package();
        package.profiles.insert(
            0,
            CatalogRuntimeProfile {
                id: "vulkan-5gb".to_string(),
                device: ProfileDevice::Gpu,
                benchmark_evidence: "fixture".to_string(),
                required_total_ram_bytes: 8_000_000_000,
                required_available_ram_bytes: 5_000_000_000,
                required_vram_bytes: 4_000_000_000,
                gpu_layers: 43,
                context_tokens: 32768,
                batch_size: 256,
                ubatch_size: 128,
                threads: 4,
                smoke_timeout_seconds: 120,
                expected_speed: "fast".to_string(),
            },
        );
        package
    }

    fn host(total: u64, available: u64, disk: u64) -> HostResources {
        HostResources {
            detected_total_memory_bytes: total,
            nominal_memory_class_bytes: nominal_memory_class(total),
            available_memory_bytes: available,
            cpu: CpuResources {
                physical_cores: 4,
                logical_cores: 8,
            },
            disk: DiskResources {
                path: PathBuf::from("/cache"),
                mount_point: PathBuf::from("/"),
                total_bytes: disk,
                available_bytes: disk,
            },
            devices: Vec::new(),
        }
    }

    fn policy() -> agl_config::AutoRuntimePolicy {
        agl_config::AutoRuntimePolicy {
            max_context_tokens: 32768,
            max_batch_size: 128,
            max_ubatch_size: 64,
            flash_attention: RuntimeSwitch::On,
            cache_type_k: KvCacheType::Q8_0,
            cache_type_v: KvCacheType::Q8_0,
        }
    }

    #[test]
    fn nominal_eight_gb_class_accepts_reserved_memory() {
        let host = host(7_700_000_000, 5_000_000_000, 20_000_000_000);
        assert_eq!(host.nominal_memory_class_bytes, 8 * 1024 * 1024 * 1024);
        assert!(!host.below_recommended_floor());
        assert_eq!(
            RuntimePlanner
                .fit(&package(), &host, 5_000_000_000, false)
                .kind,
            ModelFitKind::Recommended
        );
    }

    #[test]
    fn true_six_gb_and_low_disk_are_rejected() {
        let low_ram = host(6_200_000_000, 5_000_000_000, 20_000_000_000);
        assert_eq!(
            RuntimePlanner
                .fit(&package(), &low_ram, 5_000_000_000, false)
                .kind,
            ModelFitKind::InsufficientMemory
        );
        let low_disk = host(16_000_000_000, 12_000_000_000, 1_000_000_000);
        assert_eq!(
            RuntimePlanner
                .fit(&package(), &low_disk, 5_000_000_000, false)
                .kind,
            ModelFitKind::InsufficientDisk
        );
    }

    #[test]
    fn low_memory_override_bypasses_total_and_availability_ram_gates_only() {
        let constrained = host(6_200_000_000, 2_000_000_000, 20_000_000_000);
        let fit = RuntimePlanner.fit(&package(), &constrained, 0, true);
        assert_eq!(fit.kind, ModelFitKind::Recommended);
        assert!(fit.reason.contains("best-effort low-memory override"));

        let policy = agl_config::AutoRuntimePolicy {
            max_context_tokens: 32768,
            max_batch_size: 128,
            max_ubatch_size: 64,
            flash_attention: RuntimeSwitch::On,
            cache_type_k: KvCacheType::Q8_0,
            cache_type_v: KvCacheType::Q8_0,
        };
        let plan = RuntimePlanner
            .plan(&package(), &constrained, &policy, true)
            .unwrap();
        assert_eq!(plan.runtime.gpu_layers, 0);

        let low_disk = host(6_200_000_000, 2_000_000_000, 1_000_000_000);
        assert_eq!(
            RuntimePlanner
                .fit(&package(), &low_disk, 5_000_000_000, true)
                .kind,
            ModelFitKind::InsufficientDisk
        );
    }

    #[test]
    fn cpu_plan_is_numeric_and_bounded_by_auto_policy() {
        let host = host(16_000_000_000, 12_000_000_000, 20_000_000_000);
        let policy = agl_config::AutoRuntimePolicy {
            max_context_tokens: 32768,
            max_batch_size: 128,
            max_ubatch_size: 64,
            flash_attention: RuntimeSwitch::On,
            cache_type_k: KvCacheType::Q8_0,
            cache_type_v: KvCacheType::Q8_0,
        };
        let plan = RuntimePlanner
            .plan(&package(), &host, &policy, false)
            .unwrap();
        assert_eq!(plan.runtime.gpu_layers, 0);
        assert_eq!(plan.runtime.context_tokens, 32768);
        assert_eq!(plan.runtime.batch_size, Some(128));
        assert_eq!(plan.runtime.ubatch_size, Some(64));
    }

    #[test]
    fn discrete_gpu_plan_includes_an_explicit_cpu_fallback() {
        let mut host = host(16_000_000_000, 12_000_000_000, 20_000_000_000);
        host.devices.push(LlamaDeviceInfo {
            name: "Vulkan0".to_string(),
            description: "discrete fixture".to_string(),
            kind: LlamaDeviceKind::DiscreteGpu,
            free_memory_bytes: 6_000_000_000,
            total_memory_bytes: 8_000_000_000,
            usable: true,
            supports_gpu_offload: true,
        });
        let policy = policy();

        let plans = RuntimePlanner
            .plan_set(&package_with_gpu_profile(), &host, &policy, false)
            .unwrap();

        assert_eq!(plans.selected.profile_id, "vulkan-5gb");
        assert_eq!(plans.selected.selected_device.as_deref(), Some("Vulkan0"));
        assert_eq!(plans.selected.runtime.gpu_layers, 43);
        assert_eq!(plans.cpu_fallback.as_ref().unwrap().profile_id, "cpu-8gb");
        assert_eq!(plans.cpu_fallback.as_ref().unwrap().runtime.gpu_layers, 0);
    }

    #[test]
    fn impossible_gpu_memory_snapshots_fail_before_planning() {
        let device = |free_memory_bytes, total_memory_bytes| LlamaDeviceInfo {
            name: "Vulkan0".to_string(),
            description: "invalid driver fixture".to_string(),
            kind: LlamaDeviceKind::DiscreteGpu,
            free_memory_bytes,
            total_memory_bytes,
            usable: true,
            supports_gpu_offload: true,
        };

        let zero_total = validate_device_snapshots(&[device(0, 0)]).unwrap_err();
        assert!(zero_total.to_string().contains("zero total memory"));

        let free_above_total = validate_device_snapshots(&[device(18_788, 6_390)]).unwrap_err();
        assert!(
            free_above_total
                .to_string()
                .contains("above its 6390 total")
        );
    }

    #[test]
    fn integrated_gpu_cannot_double_count_unified_memory() {
        let mut host = host(16_000_000_000, 8_000_000_000, 20_000_000_000);
        host.devices.push(LlamaDeviceInfo {
            name: "Vulkan0".to_string(),
            description: "integrated fixture".to_string(),
            kind: LlamaDeviceKind::IntegratedGpu,
            free_memory_bytes: 6_000_000_000,
            total_memory_bytes: 8_000_000_000,
            usable: true,
            supports_gpu_offload: true,
        });

        for allow_low_memory in [false, true] {
            let plan = RuntimePlanner
                .plan(
                    &package_with_gpu_profile(),
                    &host,
                    &policy(),
                    allow_low_memory,
                )
                .unwrap();
            assert_eq!(plan.profile_id, "cpu-8gb");
            assert_eq!(plan.runtime.gpu_layers, 0);
            assert_eq!(plan.selected_device, None);
        }
    }

    #[test]
    fn unusable_gpu_is_ignored_for_cpu_safe_startup() {
        let mut host = host(16_000_000_000, 12_000_000_000, 20_000_000_000);
        host.devices.push(LlamaDeviceInfo {
            name: "Vulkan0".to_string(),
            description: "unavailable fixture".to_string(),
            kind: LlamaDeviceKind::DiscreteGpu,
            free_memory_bytes: 8_000_000_000,
            total_memory_bytes: 8_000_000_000,
            usable: false,
            supports_gpu_offload: true,
        });

        let plan = RuntimePlanner
            .plan(&package_with_gpu_profile(), &host, &policy(), false)
            .unwrap();

        assert_eq!(plan.profile_id, "cpu-8gb");
        assert_eq!(plan.runtime.gpu_layers, 0);
        assert_eq!(plan.selected_device, None);
    }
}
