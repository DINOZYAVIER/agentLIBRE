use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use super::{EngineExecutable, EngineInventory, InferenceHostConfig, InferenceHostStartError};

impl InferenceHostConfig {
    pub fn development_default(
        authority_root: impl Into<PathBuf>,
        evidence_root: impl Into<PathBuf>,
        context_idle_duration: Duration,
        model_idle_duration: Duration,
    ) -> Result<Self, InferenceHostStartError> {
        let path =
            development_engine_path(std::env::var_os("AGL_LLAMA_CPP_BUILD_DIR").map(PathBuf::from));
        let path = std::fs::canonicalize(&path).map_err(|error| {
            InferenceHostStartError::EngineStart {
                reason: format!(
                    "development llama-server is unavailable at {}: {error}; run scripts/build-llama-cpp.sh",
                    path.display()
                ),
            }
        })?;
        let sha256 = sha256_path(&path)?;
        Ok(Self {
            executable: EngineExecutable { path, sha256 },
            queue_capacity: 32,
            external_host_reserve_bytes: 1 << 30,
            authority_root: authority_root.into(),
            context_idle_duration,
            model_idle_duration,
            evidence_root: evidence_root.into(),
        })
    }
}

fn development_engine_path(build_dir: Option<PathBuf>) -> PathBuf {
    build_dir
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/llama-cpp/build")
        })
        .join("bin/llama-server")
}

pub(super) fn validate_idle_duration(
    name: &str,
    duration: Duration,
) -> Result<(), InferenceHostStartError> {
    if duration < Duration::from_secs(1)
        || duration > Duration::from_secs(24 * 60 * 60)
        || duration.subsec_nanos() != 0
    {
        return Err(InferenceHostStartError::InvalidEngineInventory {
            reason: format!("{name} must be whole seconds in 1..=86400"),
        });
    }
    Ok(())
}

pub(super) fn prepare_authority_root(
    root: &std::path::Path,
) -> Result<(), InferenceHostStartError> {
    use std::os::unix::fs::PermissionsExt as _;

    if !root.is_absolute() {
        return Err(InferenceHostStartError::LeaseUnavailable {
            reason: "inference authority root must be absolute".to_owned(),
        });
    }
    std::fs::create_dir_all(root).map_err(|error| InferenceHostStartError::LeaseUnavailable {
        reason: error.to_string(),
    })?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        InferenceHostStartError::LeaseUnavailable {
            reason: error.to_string(),
        }
    })
}

pub(super) fn prepare_evidence_root(root: &std::path::Path) -> Result<(), InferenceHostStartError> {
    use std::os::unix::fs::PermissionsExt as _;

    if !root.is_absolute() {
        return Err(InferenceHostStartError::EngineStart {
            reason: "inference evidence root must be absolute".to_owned(),
        });
    }
    std::fs::create_dir_all(root).map_err(|error| InferenceHostStartError::EngineStart {
        reason: format!("failed to create inference evidence root: {error}"),
    })?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        InferenceHostStartError::EngineStart {
            reason: format!("failed to secure inference evidence root: {error}"),
        }
    })
}

pub(super) fn acquire_authority_lease(
    root: &std::path::Path,
    identity: &str,
) -> Result<crate::DeviceAuthorityLease, InferenceHostStartError> {
    crate::DeviceAuthorityLease::acquire(root, identity).map_err(|error| {
        InferenceHostStartError::LeaseUnavailable {
            reason: format!("{identity}: {error}"),
        }
    })
}

pub(super) fn sha256_path(path: &std::path::Path) -> Result<String, InferenceHostStartError> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;

    let mut file = File::open(path).map_err(|error| InferenceHostStartError::EngineStart {
        reason: error.to_string(),
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read =
            file.read(&mut buffer)
                .map_err(|error| InferenceHostStartError::EngineStart {
                    reason: error.to_string(),
                })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(output)
}

pub(super) fn validate_inventory(
    inventory: &EngineInventory,
) -> Result<(), InferenceHostStartError> {
    if inventory.physical_host_bytes == 0
        || inventory.physical_cpu_cores == 0
        || inventory.logical_cpu_cores < inventory.physical_cpu_cores
    {
        return Err(InferenceHostStartError::InvalidEngineInventory {
            reason: "host capacity or CPU topology is impossible".to_owned(),
        });
    }
    let mut identities = std::collections::BTreeSet::new();
    for device in &inventory.devices {
        if device.identity.is_empty()
            || !identities.insert(device.identity.as_str())
            || (device.usable && device.physical_pool_bytes == 0)
        {
            return Err(InferenceHostStartError::InvalidEngineInventory {
                reason: "device identity or capacity is invalid".to_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::development_engine_path;

    #[test]
    fn selected_llama_cpp_build_directory_resolves_the_development_engine() {
        let build_dir = PathBuf::from("/tmp/agl-llama-cpp-ci-build");

        assert_eq!(
            development_engine_path(Some(build_dir)),
            Path::new("/tmp/agl-llama-cpp-ci-build/bin/llama-server")
        );
    }
}
