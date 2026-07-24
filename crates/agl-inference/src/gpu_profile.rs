//! Reviewed GPU allocation envelopes for exact, immutable model/runtime tuples.
//!
//! This is intentionally not a heuristic estimator. A fixed GPU request is
//! admitted only when its complete runtime shape matches a reviewed profile
//! and the model artifacts match pinned SHA-256 identities. Unknown shapes
//! fail closed before the native worker receives an allocation command.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use agl_config::{
    BackendKind, KvCacheType, ModelDialect, ResolvedInferenceConfig, RuntimeSwitch, ToolCallFormat,
};
use sha2::{Digest as _, Sha256};

use crate::admission::{AllocationEstimate, AllocationReceipt};

const MIB: u64 = 1024 * 1024;

const GEMMA4_E2B_MAIN: ArtifactSpec = ArtifactSpec {
    byte_size: 3_349_514_112,
    sha256: "3646b4c147cd235a44d91df1546d3b7d8e29b547dbe4e1f80856419aa455e6fd",
};

const GEMMA4_E4B_MAIN: ArtifactSpec = ArtifactSpec {
    byte_size: 4_215_693_760,
    sha256: "b3052f962d6449b4eb2075733c068bdec1c51eadb7b237e6c3157bfbb7b1dae0",
};

const GEMMA4_E4B_PROJECTOR: ArtifactSpec = ArtifactSpec {
    byte_size: 990_372_672,
    sha256: "6a255159ee4b01b304f633a57f017dd7d5a69d30fff52abb2614bf0813cef034",
};

const GEMMA4_12B_MAIN: ArtifactSpec = ArtifactSpec {
    byte_size: 6_716_355_328,
    sha256: "cc9ff072e0a8203429ed854e6662c17a6c2bc1e5dca5b475dd4736caaacbc165",
};

const GEMMA4_12B_PROJECTOR: ArtifactSpec = ArtifactSpec {
    byte_size: 175_115_840,
    sha256: "ecc4e93128da8363b7dbf2193eab98cf1142353f52ceaa0c95c0872997aaadd3",
};

const GEMMA4_26B_MAIN: ArtifactSpec = ArtifactSpec {
    byte_size: 14_249_045_120,
    sha256: "dcf179a91153e3a7ece792e48ef872180d9d6ef9b7677f0a0bd3e83cfe624d5e",
};

const GEMMA4_31B_MAIN: ArtifactSpec = ArtifactSpec {
    byte_size: 17_651_001_568,
    sha256: "179cfb99212709597eae5929112cfca677e1bbf566178b479ae1da0c4772874b",
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArtifactSpec {
    byte_size: u64,
    sha256: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProfileArtifacts {
    Gemma4E2b,
    Gemma4E4b,
    Gemma4TwelveB,
    Gemma4TwentySixB,
    Gemma4ThirtyOneB,
}

impl ProfileArtifacts {
    const fn main(self) -> ArtifactSpec {
        match self {
            Self::Gemma4E2b => GEMMA4_E2B_MAIN,
            Self::Gemma4E4b => GEMMA4_E4B_MAIN,
            Self::Gemma4TwelveB => GEMMA4_12B_MAIN,
            Self::Gemma4TwentySixB => GEMMA4_26B_MAIN,
            Self::Gemma4ThirtyOneB => GEMMA4_31B_MAIN,
        }
    }

    const fn projector(self) -> Option<ArtifactSpec> {
        match self {
            Self::Gemma4E4b => Some(GEMMA4_E4B_PROJECTOR),
            Self::Gemma4TwelveB => Some(GEMMA4_12B_PROJECTOR),
            Self::Gemma4E2b | Self::Gemma4TwentySixB | Self::Gemma4ThirtyOneB => None,
        }
    }
}

/// One reviewed allocation profile selected only after exact shape matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownGpuProfile {
    id: &'static str,
    pci_device_id: &'static str,
    pci_subsystem_id: &'static str,
    total_device_bytes: u64,
    estimate: AllocationEstimate,
    receipt: AllocationReceipt,
    reserve_bytes: u64,
    artifacts: ProfileArtifacts,
}

impl KnownGpuProfile {
    pub const fn id(self) -> &'static str {
        self.id
    }

    pub const fn pci_device_id(self) -> &'static str {
        self.pci_device_id
    }

    pub const fn pci_subsystem_id(self) -> &'static str {
        self.pci_subsystem_id
    }

    pub const fn total_device_bytes(self) -> u64 {
        self.total_device_bytes
    }

    pub const fn estimate(self) -> AllocationEstimate {
        self.estimate
    }

    /// Conservative profile receipt used by the native worker. It is a
    /// non-zero reviewed upper-bound decomposition, not a backend free-memory
    /// counter and not an observation supplied by Vulkan.
    pub const fn receipt(self) -> AllocationReceipt {
        self.receipt
    }

    pub const fn reserve_bytes(self) -> u64 {
        self.reserve_bytes
    }
}

/// Process-local artifact verifier. Successful full-file hashes are reused
/// only while the exact inode/size/mtime/ctime fingerprint remains unchanged.
#[derive(Debug, Default)]
pub struct GpuProfileVerifier {
    verified: BTreeMap<ArtifactFingerprint, String>,
}

impl GpuProfileVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(
        &mut self,
        config: &ResolvedInferenceConfig,
    ) -> Result<Option<KnownGpuProfile>, GpuProfileError> {
        let profiles = matching_gpu_profiles(config)?;
        if profiles.is_empty() {
            return Ok(None);
        }

        let accepted_main = profiles
            .iter()
            .map(|profile| profile.artifacts.main())
            .collect::<Vec<_>>();
        let main = self.identify_exact(&config.backend.model, &accepted_main)?;
        let main_profiles = profiles
            .iter()
            .copied()
            .filter(|profile| main.matches(profile.artifacts.main()))
            .collect::<Vec<_>>();
        if main_profiles.is_empty() {
            return Err(GpuProfileError::ArtifactIdentityMismatch {
                path: config.backend.model.clone(),
            });
        }

        let accepted_projectors = main_profiles
            .iter()
            .filter_map(|profile| profile.artifacts.projector())
            .collect::<Vec<_>>();
        let projector = match config.backend.multimodal_projector.as_deref() {
            Some(path) => Some(self.identify_exact(path, &accepted_projectors)?),
            None => None,
        };

        select_verified_profile(&main_profiles, &main, projector.as_ref())
            .map(Some)
            .ok_or_else(|| GpuProfileError::ArtifactIdentityMismatch {
                path: config
                    .backend
                    .multimodal_projector
                    .clone()
                    .unwrap_or_else(|| config.backend.model.clone()),
            })
    }

    fn identify_exact(
        &mut self,
        path: &Path,
        accepted: &[ArtifactSpec],
    ) -> Result<ArtifactIdentity, GpuProfileError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options
            .open(path)
            .map_err(|error| GpuProfileError::ArtifactUnavailable {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        let before = ArtifactFingerprint::from_file(path, &file)?;
        if !accepted
            .iter()
            .any(|expected| expected.byte_size == before.byte_size)
        {
            return Err(GpuProfileError::ArtifactIdentityMismatch {
                path: path.to_path_buf(),
            });
        }

        let digest = match self.verified.get(&before) {
            Some(digest) => digest.clone(),
            None => {
                let digest = sha256_file(file, path)?;
                // Re-open and re-stat the exact path after hashing so a
                // replacement during verification cannot populate the cache.
                let mut options = OpenOptions::new();
                options
                    .read(true)
                    .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
                let after_file =
                    options
                        .open(path)
                        .map_err(|error| GpuProfileError::ArtifactUnavailable {
                            path: path.to_path_buf(),
                            message: error.to_string(),
                        })?;
                let after = ArtifactFingerprint::from_file(path, &after_file)?;
                if after != before {
                    return Err(GpuProfileError::ArtifactChanged {
                        path: path.to_path_buf(),
                    });
                }
                self.verified.insert(before.clone(), digest.clone());
                digest
            }
        };
        if !accepted
            .iter()
            .any(|expected| expected.byte_size == before.byte_size && expected.sha256 == digest)
        {
            return Err(GpuProfileError::ArtifactIdentityMismatch {
                path: path.to_path_buf(),
            });
        }
        Ok(ArtifactIdentity {
            byte_size: before.byte_size,
            sha256: digest,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArtifactIdentity {
    byte_size: u64,
    sha256: String,
}

impl ArtifactIdentity {
    fn matches(&self, expected: ArtifactSpec) -> bool {
        self.byte_size == expected.byte_size && self.sha256 == expected.sha256
    }
}

fn select_verified_profile(
    profiles: &[KnownGpuProfile],
    main: &ArtifactIdentity,
    projector: Option<&ArtifactIdentity>,
) -> Option<KnownGpuProfile> {
    let mut matching = profiles.iter().copied().filter(|profile| {
        main.matches(profile.artifacts.main())
            && match (profile.artifacts.projector(), projector) {
                (Some(expected), Some(actual)) => actual.matches(expected),
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            }
    });
    let profile = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    Some(profile)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ArtifactFingerprint {
    path: PathBuf,
    device: u64,
    inode: u64,
    byte_size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl ArtifactFingerprint {
    fn from_file(path: &Path, file: &File) -> Result<Self, GpuProfileError> {
        let metadata = file
            .metadata()
            .map_err(|error| GpuProfileError::ArtifactUnavailable {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        if !metadata.is_file() {
            return Err(GpuProfileError::ArtifactIdentityMismatch {
                path: path.to_path_buf(),
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }
}

fn sha256_file(file: File, path: &Path) -> Result<String, GpuProfileError> {
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read =
            reader
                .read(&mut buffer)
                .map_err(|error| GpuProfileError::ArtifactUnavailable {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

/// Shape-only lookup for the worker side of an already host-verified job.
///
/// The host must call [`GpuProfileVerifier::resolve`] before dispatch. Multiple
/// artifact families may intentionally share one runtime shape; in that case
/// the returned value is only a shape witness. Host admission must use the
/// artifact-selected profile returned by [`GpuProfileVerifier::resolve`].
pub fn known_gpu_profile_shape(
    config: &ResolvedInferenceConfig,
) -> Result<Option<KnownGpuProfile>, GpuProfileError> {
    Ok(matching_gpu_profiles(config)?.into_iter().next())
}

fn matching_gpu_profiles(
    config: &ResolvedInferenceConfig,
) -> Result<Vec<KnownGpuProfile>, GpuProfileError> {
    let runtime = &config.runtime;
    let draft_gpu_layers = runtime.mtp.gpu_layers.unwrap_or(runtime.gpu_layers);
    let requests_gpu = runtime.gpu_layers > 0 || (runtime.mtp.enabled && draft_gpu_layers > 0);
    if !requests_gpu {
        return Ok(Vec::new());
    }

    let common_shape = config.backend.kind == BackendKind::LlamaCpp
        && runtime.gpu_layers == 999
        && runtime.device.as_deref() == Some("Vulkan0")
        && runtime.threads == 8
        && runtime.flash_attention == Some(RuntimeSwitch::On)
        && !runtime.mtp.enabled
        && config.model.dialect == ModelDialect::Gemma4
        && config.model.tool_call_format == ToolCallFormat::GemmaFunctionCall;
    if !common_shape {
        return Err(GpuProfileError::UnknownFixedConfiguration);
    }

    let has_projector = config.backend.multimodal_projector.is_some();
    let mut profiles = Vec::new();

    if has_projector && exact_runtime_shape(runtime, 65_536, 1024, 1024, None, None) {
        profiles.push(manual_12b_profile(65_536));
    }
    if has_projector && exact_runtime_shape(runtime, 98_304, 1024, 1024, None, None) {
        profiles.push(manual_12b_profile(98_304));
    }

    if has_projector && exact_runtime_shape(runtime, 32_768, 512, 256, Some(true), Some(true)) {
        profiles.push(planner_profile(
            "gemma4-e4b-qat-ud-q4-rx7900xtx-vulkan-q8-32k-auto-20260723",
            AllocationEstimate {
                model_bytes: 3_600 * MIB,
                context_bytes: 384 * MIB,
                transient_bytes: 640 * MIB,
                uncertainty_bytes: 512 * MIB,
            },
            AllocationReceipt {
                model_bytes: 3_438 * MIB,
                context_bytes: 288 * MIB,
                transient_bytes: 517 * MIB,
            },
            ProfileArtifacts::Gemma4E4b,
        ));
    }
    if has_projector && exact_runtime_shape(runtime, 65_536, 512, 256, Some(true), Some(true)) {
        profiles.push(planner_profile(
            "gemma4-12b-qat-ud-q4-rx7900xtx-vulkan-q8-64k-auto-20260723",
            AllocationEstimate {
                model_bytes: 6_700 * MIB,
                context_bytes: 900 * MIB,
                transient_bytes: 384 * MIB,
                uncertainty_bytes: 512 * MIB,
            },
            AllocationReceipt {
                model_bytes: 6_558 * MIB,
                context_bytes: 757 * MIB,
                transient_bytes: 342 * MIB,
            },
            ProfileArtifacts::Gemma4TwelveB,
        ));
    }

    if !has_projector && exact_runtime_shape(runtime, 32_768, 512, 256, Some(true), Some(true)) {
        profiles.extend([
            planner_profile(
                "gemma4-e2b-qat-q4_0-rx7900xtx-vulkan-q8-32k-auto-20260723",
                AllocationEstimate {
                    model_bytes: 1_500 * MIB,
                    context_bytes: 160 * MIB,
                    transient_bytes: 384 * MIB,
                    uncertainty_bytes: 512 * MIB,
                },
                AllocationReceipt {
                    model_bytes: 1_342 * MIB,
                    context_bytes: 107 * MIB,
                    transient_bytes: 284 * MIB,
                },
                ProfileArtifacts::Gemma4E2b,
            ),
            planner_profile(
                "gemma4-26b-a4b-qat-ud-q4-rx7900xtx-vulkan-q8-32k-auto-20260723",
                AllocationEstimate {
                    model_bytes: 13_850 * MIB,
                    context_bytes: 600 * MIB,
                    transient_bytes: 384 * MIB,
                    uncertainty_bytes: 512 * MIB,
                },
                AllocationReceipt {
                    model_bytes: 13_574 * MIB,
                    context_bytes: 473 * MIB,
                    transient_bytes: 263 * MIB,
                },
                ProfileArtifacts::Gemma4TwentySixB,
            ),
            planner_profile(
                "gemma4-31b-qat-q4_0-rx7900xtx-vulkan-q8-32k-auto-20260723",
                AllocationEstimate {
                    model_bytes: 17_050 * MIB,
                    context_bytes: 2_050 * MIB,
                    transient_bytes: 384 * MIB,
                    uncertainty_bytes: 512 * MIB,
                },
                AllocationReceipt {
                    model_bytes: 16_819 * MIB,
                    context_bytes: 1_892 * MIB,
                    transient_bytes: 290 * MIB,
                },
                ProfileArtifacts::Gemma4ThirtyOneB,
            ),
        ]);
    }

    if !has_projector && exact_runtime_shape(runtime, 32_768, 2048, 512, None, None) {
        profiles.push(KnownGpuProfile {
            id: "gemma4-31b-qat-q4_0-rx7900xtx-vulkan-q8-32k-20260722",
            pci_device_id: "1002:744c",
            pci_subsystem_id: "1da2:471e",
            total_device_bytes: 24_560 * MIB,
            estimate: AllocationEstimate {
                model_bytes: 17_050 * MIB,
                context_bytes: 2_100 * MIB,
                transient_bytes: 768 * MIB,
                uncertainty_bytes: 512 * MIB,
            },
            receipt: AllocationReceipt {
                model_bytes: 16_819 * MIB,
                context_bytes: 1_998 * MIB,
                transient_bytes: 591 * MIB,
            },
            reserve_bytes: 1_024 * MIB,
            artifacts: ProfileArtifacts::Gemma4ThirtyOneB,
        });
    }

    if profiles.is_empty() {
        return Err(GpuProfileError::UnknownFixedConfiguration);
    }
    Ok(profiles)
}

fn exact_runtime_shape(
    runtime: &agl_config::InferenceRuntimeConfig,
    context_tokens: u32,
    batch_size: u32,
    ubatch_size: u32,
    mmap: Option<bool>,
    kv_unified: Option<bool>,
) -> bool {
    runtime.context_tokens == context_tokens
        && runtime.batch_size == Some(batch_size)
        && runtime.ubatch_size == Some(ubatch_size)
        && runtime.cache_type_k == Some(KvCacheType::Q8_0)
        && runtime.cache_type_v == Some(KvCacheType::Q8_0)
        && runtime.mmap == mmap
        && runtime.kv_unified == kv_unified
}

fn manual_12b_profile(context_tokens: u32) -> KnownGpuProfile {
    let (context_mib, measured_context_mib) = match context_tokens {
        65_536 => (11_776, 11_424),
        98_304 => (18_788, 18_788),
        _ => unreachable!("manual 12B profile has a fixed context"),
    };
    KnownGpuProfile {
        id: "gemma4-12b-rx7900xtx-vulkan-q8-64k-20260721",
        pci_device_id: "1002:744c",
        pci_subsystem_id: "1da2:471e",
        total_device_bytes: 24_560 * MIB,
        estimate: AllocationEstimate {
            model_bytes: 6_650 * MIB,
            context_bytes: context_mib * MIB,
            transient_bytes: 1_792 * MIB,
            uncertainty_bytes: 1_024 * MIB,
        },
        receipt: AllocationReceipt {
            model_bytes: 6_390 * MIB,
            context_bytes: measured_context_mib * MIB,
            transient_bytes: 1_525 * MIB,
        },
        reserve_bytes: 1_024 * MIB,
        artifacts: ProfileArtifacts::Gemma4TwelveB,
    }
}

fn planner_profile(
    id: &'static str,
    estimate: AllocationEstimate,
    receipt: AllocationReceipt,
    artifacts: ProfileArtifacts,
) -> KnownGpuProfile {
    KnownGpuProfile {
        id,
        pci_device_id: "1002:744c",
        pci_subsystem_id: "1da2:471e",
        total_device_bytes: 24_560 * MIB,
        estimate,
        receipt,
        reserve_bytes: 1_024 * MIB,
        artifacts,
    }
}

#[derive(Debug)]
pub enum GpuProfileError {
    UnknownFixedConfiguration,
    ArtifactUnavailable { path: PathBuf, message: String },
    ArtifactIdentityMismatch { path: PathBuf },
    ArtifactChanged { path: PathBuf },
}

impl GpuProfileError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownFixedConfiguration => "resource_estimate_unknown",
            Self::ArtifactUnavailable { .. } => "resource_profile_artifact_unavailable",
            Self::ArtifactIdentityMismatch { .. } | Self::ArtifactChanged { .. } => {
                "resource_profile_artifact_mismatch"
            }
        }
    }
}

impl fmt::Display for GpuProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFixedConfiguration => formatter
                .write_str("fixed GPU inference configuration has no reviewed allocation profile"),
            Self::ArtifactUnavailable { path, message } => write!(
                formatter,
                "cannot verify GPU profile artifact {}: {message}",
                path.display()
            ),
            Self::ArtifactIdentityMismatch { path } => write!(
                formatter,
                "GPU profile artifact does not match a pinned identity: {}",
                path.display()
            ),
            Self::ArtifactChanged { path } => write!(
                formatter,
                "GPU profile artifact changed while it was being verified: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for GpuProfileError {}

#[cfg(test)]
mod tests {
    use agl_config::{
        InferenceBackendConfig, InferenceRuntimeConfig, ModelConfig, MtpRuntimeConfig, PromptConfig,
    };

    use super::*;
    use crate::admission::{
        AdmissionPolicy, AllocationReceipt as LedgerReceipt, DeviceMemoryEnvelope,
        DeviceMemorySnapshot, ReservationLedger, ReservationRequest, SnapshotPolicy, mib,
        validate_device_snapshot,
    };

    fn config(context_tokens: u32) -> ResolvedInferenceConfig {
        ResolvedInferenceConfig {
            backend: InferenceBackendConfig {
                kind: BackendKind::LlamaCpp,
                model: PathBuf::from("/models/gemma4-12b.gguf"),
                multimodal_projector: Some(PathBuf::from("/models/mmproj.gguf")),
            },
            runtime: InferenceRuntimeConfig {
                gpu_layers: 999,
                context_tokens,
                threads: 8,
                device: Some("Vulkan0".to_string()),
                batch_size: Some(1024),
                ubatch_size: Some(1024),
                flash_attention: Some(RuntimeSwitch::On),
                cache_type_k: Some(KvCacheType::Q8_0),
                cache_type_v: Some(KvCacheType::Q8_0),
                mmap: None,
                kv_unified: None,
                structured_decoding: agl_config::StructuredDecodingMode::Auto,
                repair_malformed_tool_calls: true,
                mtp: MtpRuntimeConfig::default(),
            },
            model: ModelConfig {
                dialect: ModelDialect::Gemma4,
                tool_call_format: ToolCallFormat::GemmaFunctionCall,
            },
            prompt: PromptConfig::default(),
        }
    }

    fn config_31b() -> ResolvedInferenceConfig {
        let mut config = config(32_768);
        config.backend.model = PathBuf::from("/models/gemma4-31b-q4_0-qat.gguf");
        config.backend.multimodal_projector = None;
        config.runtime.batch_size = Some(2048);
        config.runtime.ubatch_size = Some(512);
        config
    }

    fn planner_config(context_tokens: u32, projector: bool) -> ResolvedInferenceConfig {
        let mut config = config(context_tokens);
        config.backend.multimodal_projector =
            projector.then(|| PathBuf::from("/models/mmproj.gguf"));
        config.runtime.batch_size = Some(512);
        config.runtime.ubatch_size = Some(256);
        config.runtime.mmap = Some(true);
        config.runtime.kv_unified = Some(true);
        config
    }

    fn identity(spec: ArtifactSpec) -> ArtifactIdentity {
        ArtifactIdentity {
            byte_size: spec.byte_size,
            sha256: spec.sha256.to_string(),
        }
    }

    fn select(config: &ResolvedInferenceConfig, family: ProfileArtifacts) -> KnownGpuProfile {
        let candidates = matching_gpu_profiles(config).unwrap();
        let projector = family.projector().map(identity);
        select_verified_profile(&candidates, &identity(family.main()), projector.as_ref()).unwrap()
    }

    fn snapshot(available_mib: u64) -> crate::admission::ValidatedDeviceSnapshot {
        let total = mib(24_560).unwrap();
        let envelope = DeviceMemoryEnvelope {
            physical_device_id: "pci:0000:03:00.0".to_string(),
            minimum_total_bytes: total,
            maximum_total_bytes: total,
        };
        validate_device_snapshot(
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:test".to_string(),
                total_bytes: total,
                available_bytes: mib(available_mib).unwrap(),
                observed_at_unix_ms: 10_000,
            },
            &envelope,
            SnapshotPolicy::default(),
            10_000,
        )
        .unwrap()
    }

    #[test]
    fn exact_64k_profile_is_admitted_but_98304_incident_is_rejected() {
        let safe = known_gpu_profile_shape(&config(65_536)).unwrap().unwrap();
        let incident = known_gpu_profile_shape(&config(98_304)).unwrap().unwrap();
        let snapshot = snapshot(23_000);

        let mut safe_ledger = ReservationLedger::new(
            "pci:0000:03:00.0",
            AdmissionPolicy {
                reserve_bytes: safe.reserve_bytes(),
            },
        )
        .unwrap();
        assert!(
            safe_ledger
                .reserve(
                    &snapshot,
                    ReservationRequest {
                        model_key: "model".to_string(),
                        context_key: "context".to_string(),
                        estimate: safe.estimate(),
                    },
                )
                .is_ok()
        );

        let mut incident_ledger = ReservationLedger::new(
            "pci:0000:03:00.0",
            AdmissionPolicy {
                reserve_bytes: incident.reserve_bytes(),
            },
        )
        .unwrap();
        assert!(
            incident_ledger
                .reserve(
                    &snapshot,
                    ReservationRequest {
                        model_key: "model".to_string(),
                        context_key: "context".to_string(),
                        estimate: incident.estimate(),
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn second_64k_context_cannot_spend_the_same_device_budget() {
        let profile = known_gpu_profile_shape(&config(65_536)).unwrap().unwrap();
        let snapshot = snapshot(23_000);
        let mut ledger = ReservationLedger::new(
            "pci:0000:03:00.0",
            AdmissionPolicy {
                reserve_bytes: profile.reserve_bytes(),
            },
        )
        .unwrap();
        let first = ledger
            .reserve(
                &snapshot,
                ReservationRequest {
                    model_key: "model".to_string(),
                    context_key: "context-a".to_string(),
                    estimate: profile.estimate(),
                },
            )
            .unwrap();
        let receipt = profile.receipt();
        ledger
            .commit(
                first.token,
                LedgerReceipt {
                    model_bytes: receipt.model_bytes,
                    context_bytes: receipt.context_bytes,
                    transient_bytes: receipt.transient_bytes,
                },
            )
            .unwrap();
        ledger.finish_active(first.token).unwrap();

        assert!(
            ledger
                .reserve(
                    &snapshot,
                    ReservationRequest {
                        model_key: "model".to_string(),
                        context_key: "context-b".to_string(),
                        estimate: profile.estimate(),
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn unknown_gpu_shapes_never_inherit_profile_support() {
        let mut unknown = config(65_536);
        unknown.runtime.ubatch_size = Some(512);
        assert!(matches!(
            known_gpu_profile_shape(&unknown),
            Err(GpuProfileError::UnknownFixedConfiguration)
        ));

        let mut cpu = config(65_536);
        cpu.runtime.gpu_layers = 0;
        assert_eq!(known_gpu_profile_shape(&cpu).unwrap(), None);
    }

    #[test]
    fn exact_31b_qat_profile_is_recognized_and_variants_fail_closed() {
        let profile = known_gpu_profile_shape(&config_31b()).unwrap().unwrap();
        assert_eq!(
            profile.id(),
            "gemma4-31b-qat-q4_0-rx7900xtx-vulkan-q8-32k-20260722"
        );
        assert_eq!(profile.artifacts, ProfileArtifacts::Gemma4ThirtyOneB);
        assert_eq!(profile.receipt().model_bytes, mib(16_819).unwrap());
        assert_eq!(profile.receipt().context_bytes, mib(1_998).unwrap());

        let mut projector = config_31b();
        projector.backend.multimodal_projector = Some(PathBuf::from("/models/mmproj.gguf"));
        assert!(matches!(
            known_gpu_profile_shape(&projector),
            Err(GpuProfileError::UnknownFixedConfiguration)
        ));

        let mut batch = config_31b();
        batch.runtime.batch_size = Some(1024);
        let mut ubatch = config_31b();
        ubatch.runtime.ubatch_size = Some(256);
        let mut context = config_31b();
        context.runtime.context_tokens = 24_576;
        let mut cache = config_31b();
        cache.runtime.cache_type_v = Some(KvCacheType::Q4_0);
        for config in [batch, ubatch, context, cache] {
            assert!(matches!(
                known_gpu_profile_shape(&config),
                Err(GpuProfileError::UnknownFixedConfiguration)
            ));
        }
    }

    #[test]
    fn planner_shapes_are_disambiguated_by_exact_artifact_identity() {
        let text = planner_config(32_768, false);
        let candidates = matching_gpu_profiles(&text).unwrap();
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates
                .iter()
                .map(|profile| profile.artifacts)
                .collect::<Vec<_>>(),
            [
                ProfileArtifacts::Gemma4E2b,
                ProfileArtifacts::Gemma4TwentySixB,
                ProfileArtifacts::Gemma4ThirtyOneB,
            ]
        );

        let expected = [
            (
                ProfileArtifacts::Gemma4E2b,
                "gemma4-e2b-qat-q4_0-rx7900xtx-vulkan-q8-32k-auto-20260723",
                (1_342, 107, 284),
                (1_500, 160, 384, 512),
            ),
            (
                ProfileArtifacts::Gemma4TwentySixB,
                "gemma4-26b-a4b-qat-ud-q4-rx7900xtx-vulkan-q8-32k-auto-20260723",
                (13_574, 473, 263),
                (13_850, 600, 384, 512),
            ),
            (
                ProfileArtifacts::Gemma4ThirtyOneB,
                "gemma4-31b-qat-q4_0-rx7900xtx-vulkan-q8-32k-auto-20260723",
                (16_819, 1_892, 290),
                (17_050, 2_050, 384, 512),
            ),
        ];
        for (family, id, receipt, estimate) in expected {
            let selected = select(&text, family);
            assert_eq!(selected.id(), id);
            assert_eq!(selected.artifacts, family);
            assert_profile_bytes(selected, receipt, estimate);
        }

        let unknown = ArtifactIdentity {
            byte_size: GEMMA4_E2B_MAIN.byte_size,
            sha256: "0".repeat(64),
        };
        assert!(select_verified_profile(&candidates, &unknown, None).is_none());
    }

    #[test]
    fn projector_profiles_require_the_exact_family_pair() {
        let e4b = planner_config(32_768, true);
        let e4b_candidates = matching_gpu_profiles(&e4b).unwrap();
        assert_eq!(e4b_candidates.len(), 1);
        let e4b_profile = select(&e4b, ProfileArtifacts::Gemma4E4b);
        assert_profile_bytes(e4b_profile, (3_438, 288, 517), (3_600, 384, 640, 512));

        let twelve = planner_config(65_536, true);
        let twelve_candidates = matching_gpu_profiles(&twelve).unwrap();
        assert_eq!(twelve_candidates.len(), 1);
        let twelve_profile = select(&twelve, ProfileArtifacts::Gemma4TwelveB);
        assert_profile_bytes(twelve_profile, (6_558, 757, 342), (6_700, 900, 384, 512));

        assert!(
            select_verified_profile(
                &e4b_candidates,
                &identity(GEMMA4_E4B_MAIN),
                Some(&identity(GEMMA4_12B_PROJECTOR)),
            )
            .is_none()
        );
        assert!(
            select_verified_profile(
                &twelve_candidates,
                &identity(GEMMA4_12B_MAIN),
                Some(&identity(GEMMA4_E4B_PROJECTOR)),
            )
            .is_none()
        );
        assert!(
            select_verified_profile(&e4b_candidates, &identity(GEMMA4_E4B_MAIN), None,).is_none()
        );
    }

    #[test]
    fn planner_profiles_fail_closed_on_every_runtime_dimension() {
        let base = planner_config(32_768, false);
        let mut variants = Vec::new();

        let mut context = base.clone();
        context.runtime.context_tokens = 32_767;
        variants.push(context);
        let mut batch = base.clone();
        batch.runtime.batch_size = Some(256);
        variants.push(batch);
        let mut ubatch = base.clone();
        ubatch.runtime.ubatch_size = Some(128);
        variants.push(ubatch);
        let mut cache_k = base.clone();
        cache_k.runtime.cache_type_k = Some(KvCacheType::Q4_0);
        variants.push(cache_k);
        let mut cache_v = base.clone();
        cache_v.runtime.cache_type_v = Some(KvCacheType::Q4_0);
        variants.push(cache_v);
        let mut mmap = base.clone();
        mmap.runtime.mmap = None;
        variants.push(mmap);
        let mut kv_unified = base.clone();
        kv_unified.runtime.kv_unified = Some(false);
        variants.push(kv_unified);
        let mut layers = base.clone();
        layers.runtime.gpu_layers = 61;
        variants.push(layers);
        let mut device = base.clone();
        device.runtime.device = Some("Vulkan1".to_string());
        variants.push(device);
        let mut threads = base.clone();
        threads.runtime.threads = 7;
        variants.push(threads);
        let mut flash = base.clone();
        flash.runtime.flash_attention = Some(RuntimeSwitch::Off);
        variants.push(flash);
        let mut mtp = base;
        mtp.runtime.mtp.enabled = true;
        variants.push(mtp);

        for variant in variants {
            assert!(matches!(
                matching_gpu_profiles(&variant),
                Err(GpuProfileError::UnknownFixedConfiguration)
            ));
        }
    }

    #[test]
    fn every_profile_uses_the_uniform_reserve_and_exact_admission_boundary() {
        let incident = known_gpu_profile_shape(&config(98_304)).unwrap().unwrap();
        assert_eq!(incident.reserve_bytes(), mib(1_024).unwrap());

        let profiles = [
            known_gpu_profile_shape(&config(65_536)).unwrap().unwrap(),
            known_gpu_profile_shape(&config_31b()).unwrap().unwrap(),
            select(&planner_config(32_768, false), ProfileArtifacts::Gemma4E2b),
            select(&planner_config(32_768, true), ProfileArtifacts::Gemma4E4b),
            select(
                &planner_config(65_536, true),
                ProfileArtifacts::Gemma4TwelveB,
            ),
            select(
                &planner_config(32_768, false),
                ProfileArtifacts::Gemma4TwentySixB,
            ),
            select(
                &planner_config(32_768, false),
                ProfileArtifacts::Gemma4ThirtyOneB,
            ),
        ];

        for profile in profiles {
            assert_eq!(profile.reserve_bytes(), mib(1_024).unwrap());
            profile
                .receipt()
                .validate_against(profile.estimate())
                .unwrap();

            let required_mib =
                (profile.estimate().envelope_bytes().unwrap() + profile.reserve_bytes()) / MIB;
            let reserve = AdmissionPolicy {
                reserve_bytes: profile.reserve_bytes(),
            };
            let request = || ReservationRequest {
                model_key: "model".to_string(),
                context_key: profile.id().to_string(),
                estimate: profile.estimate(),
            };
            let mut admitted = ReservationLedger::new("pci:0000:03:00.0", reserve).unwrap();
            assert!(admitted.reserve(&snapshot(required_mib), request()).is_ok());

            let mut denied = ReservationLedger::new("pci:0000:03:00.0", reserve).unwrap();
            assert!(
                denied
                    .reserve(&snapshot(required_mib - 1), request())
                    .is_err()
            );
        }
    }

    fn assert_profile_bytes(
        profile: KnownGpuProfile,
        receipt: (u64, u64, u64),
        estimate: (u64, u64, u64, u64),
    ) {
        assert_eq!(profile.receipt().model_bytes, mib(receipt.0).unwrap());
        assert_eq!(profile.receipt().context_bytes, mib(receipt.1).unwrap());
        assert_eq!(profile.receipt().transient_bytes, mib(receipt.2).unwrap());
        assert_eq!(profile.estimate().model_bytes, mib(estimate.0).unwrap());
        assert_eq!(profile.estimate().context_bytes, mib(estimate.1).unwrap());
        assert_eq!(profile.estimate().transient_bytes, mib(estimate.2).unwrap());
        assert_eq!(
            profile.estimate().uncertainty_bytes,
            mib(estimate.3).unwrap()
        );
    }
}
