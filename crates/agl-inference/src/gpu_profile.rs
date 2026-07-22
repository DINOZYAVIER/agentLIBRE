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

const GEMMA4_12B_MAIN: ArtifactSpec = ArtifactSpec {
    byte_size: 6_716_355_328,
    sha256: "cc9ff072e0a8203429ed854e6662c17a6c2bc1e5dca5b475dd4736caaacbc165",
};

// The builtin catalog artifact and the exact BF16 projector used by the
// designated 2026-07-21 64K live test are byte-distinct, reviewed variants of
// the same Gemma 4 12B projector allocation shape.
const GEMMA4_12B_PROJECTORS: [ArtifactSpec; 2] = [
    ArtifactSpec {
        byte_size: 175_115_840,
        sha256: "ecc4e93128da8363b7dbf2193eab98cf1142353f52ceaa0c95c0872997aaadd3",
    },
    ArtifactSpec {
        byte_size: 175_115_328,
        sha256: "922168af5824a5df33cfeb0afa7ccac7e47355b4d268693a2b2bab517ac1d066",
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArtifactSpec {
    byte_size: u64,
    sha256: &'static str,
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
        let Some(profile) = known_gpu_profile_shape(config)? else {
            return Ok(None);
        };
        self.verify_exact(&config.backend.model, &[GEMMA4_12B_MAIN])?;
        let projector = config
            .backend
            .multimodal_projector
            .as_deref()
            .ok_or(GpuProfileError::UnknownFixedConfiguration)?;
        self.verify_exact(projector, &GEMMA4_12B_PROJECTORS)?;
        Ok(Some(profile))
    }

    fn verify_exact(
        &mut self,
        path: &Path,
        accepted: &[ArtifactSpec],
    ) -> Result<(), GpuProfileError> {
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
        Ok(())
    }
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
/// The host must call [`GpuProfileVerifier::resolve`] before dispatch. The
/// worker repeats the exact config match so it cannot manufacture a receipt
/// for an arbitrary fixed profile.
pub fn known_gpu_profile_shape(
    config: &ResolvedInferenceConfig,
) -> Result<Option<KnownGpuProfile>, GpuProfileError> {
    let runtime = &config.runtime;
    let draft_gpu_layers = runtime.mtp.gpu_layers.unwrap_or(runtime.gpu_layers);
    let requests_gpu = runtime.gpu_layers > 0 || (runtime.mtp.enabled && draft_gpu_layers > 0);
    if !requests_gpu {
        return Ok(None);
    }

    let exact_shape = config.backend.kind == BackendKind::LlamaCpp
        && config.backend.multimodal_projector.is_some()
        && runtime.gpu_layers == 999
        && runtime.device.as_deref() == Some("Vulkan0")
        && runtime.threads == 8
        && runtime.batch_size == Some(1024)
        && runtime.ubatch_size == Some(1024)
        && runtime.flash_attention == Some(RuntimeSwitch::On)
        && runtime.cache_type_k == Some(KvCacheType::Q8_0)
        && runtime.cache_type_v == Some(KvCacheType::Q8_0)
        && runtime.mmap.is_none()
        && runtime.kv_unified.is_none()
        && !runtime.mtp.enabled
        && config.model.dialect == ModelDialect::Gemma4
        && config.model.tool_call_format == ToolCallFormat::GemmaFunctionCall;
    if !exact_shape {
        return Err(GpuProfileError::UnknownFixedConfiguration);
    }

    let (context_mib, measured_context_mib) = match runtime.context_tokens {
        // The isolated 64K worker measured 11,424 MiB of device context
        // buffers. Keep a 352-MiB component margin in addition to the global
        // uncertainty and reserve below. The 98K incident remains recorded at
        // its measured size and is rejected by capacity admission.
        65_536 => (11_776, 11_424),
        98_304 => (18_788, 18_788),
        _ => return Err(GpuProfileError::UnknownFixedConfiguration),
    };
    Ok(Some(KnownGpuProfile {
        id: "gemma4-12b-rx7900xtx-vulkan-q8-64k-20260721",
        pci_device_id: "1002:744c",
        pci_subsystem_id: "1da2:471e",
        total_device_bytes: 24_560 * MIB,
        estimate: AllocationEstimate {
            // 6,390 MiB measured weights plus a bounded projector/backend
            // allowance retained for the complete worker generation.
            model_bytes: 6_650 * MIB,
            context_bytes: context_mib * MIB,
            // The isolated 64K worker measured 1,524.32 MiB of Vulkan compute
            // buffers. Round up and retain a further component margin.
            transient_bytes: 1_792 * MIB,
            uncertainty_bytes: 1_024 * MIB,
        },
        receipt: AllocationReceipt {
            model_bytes: 6_390 * MIB,
            context_bytes: measured_context_mib * MIB,
            transient_bytes: 1_525 * MIB,
        },
        reserve_bytes: 1_024 * MIB,
    }))
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
                mtp: MtpRuntimeConfig::default(),
            },
            model: ModelConfig {
                dialect: ModelDialect::Gemma4,
                tool_call_format: ToolCallFormat::GemmaFunctionCall,
            },
            prompt: PromptConfig::default(),
        }
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
}
