use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use agl_model::{HostCapabilityDevice, HostCapabilityDeviceKind};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sysinfo::System;

use crate::host::{
    EngineDeviceRuntimeIdentity, EngineExecutable, EngineInventory, InferenceHostStartError,
    ResourcePools,
};

const INVENTORY_FD: RawFd = 191;
const MAX_INVENTORY_BYTES: u64 = 256 * 1024;

#[derive(Debug)]
pub(crate) struct DiscoveredInventory {
    pub inventory: EngineInventory,
    pub available: ResourcePools,
    pub executable: File,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeInventory {
    schema: String,
    llama_cpp_commit: String,
    devices: Vec<NativeDevice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeDevice {
    identity: String,
    description: String,
    native_device_id: String,
    kind: NativeDeviceKind,
    available_pool_bytes: u64,
    physical_pool_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeDeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

impl NativeDeviceKind {
    fn is_usable(self) -> bool {
        !matches!(self, Self::Metadata | Self::Unknown)
    }
}

pub(crate) fn discover(
    configured: &EngineExecutable,
) -> Result<DiscoveredInventory, InferenceHostStartError> {
    let mut executable = open_executable(&configured.path)?;
    let actual_sha256 = hash_file(&mut executable)?;
    if actual_sha256 != configured.sha256 {
        return Err(InferenceHostStartError::ExecutableIdentityMismatch {
            expected_sha256: configured.sha256.clone(),
            actual_sha256,
        });
    }

    let (reader, writer) = UnixStream::pair().map_err(start_io)?;
    let writer_fd = writer.as_raw_fd();
    let executable_fd = executable.as_raw_fd();
    let executable_path = format!("/proc/self/fd/{executable_fd}");
    let mut command = Command::new(executable_path);
    command
        .env_clear()
        .env("AGL_LLAMA_SERVER_INVENTORY_FD", INVENTORY_FD.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: `pre_exec` invokes only async-signal-safe fcntl/dup2 calls and
    // reports failure through `io::Error`; no allocation occurs in the child.
    unsafe {
        command.pre_exec(move || {
            clear_cloexec(executable_fd)?;
            if libc::dup2(writer_fd, INVENTORY_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            clear_cloexec(INVENTORY_FD)
        });
    }
    let mut child = command.spawn().map_err(start_io)?;
    drop(writer);

    let mut ready = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let polled = unsafe { libc::poll(&mut ready, 1, 10_000) };
    if polled <= 0 {
        let _ = child.kill();
        let output = child.wait_with_output().map_err(start_io)?;
        return Err(invalid(&format!(
            "native inventory did not answer within 10 seconds; stderr: {}",
            bounded_stderr(&output.stderr)
        )));
    }
    let mut bytes = Vec::new();
    reader
        .take(MAX_INVENTORY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(start_io)?;
    if bytes.len() as u64 > MAX_INVENTORY_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err(invalid("native inventory exceeds 256 KiB"));
    }
    let output = child.wait_with_output().map_err(start_io)?;
    if !output.status.success() {
        return Err(invalid(&format!(
            "native inventory exited with {}; stderr: {}",
            output.status,
            bounded_stderr(&output.stderr)
        )));
    }
    let native: NativeInventory = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(&format!("invalid native inventory JSON: {error}")))?;
    convert(native, configured.clone(), executable)
}

fn convert(
    native: NativeInventory,
    executable_identity: EngineExecutable,
    executable: File,
) -> Result<DiscoveredInventory, InferenceHostStartError> {
    if native.schema != "agentlibre.llama-inventory/v1" {
        return Err(invalid("unsupported native inventory schema"));
    }
    if native.llama_cpp_commit.is_empty()
        || native.llama_cpp_commit.len() > 64
        || !native
            .llama_cpp_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("invalid llama.cpp commit identity"));
    }
    if native.devices.is_empty() || native.devices.len() > 32 {
        return Err(invalid("native inventory device count is invalid"));
    }

    let mut devices = Vec::with_capacity(native.devices.len());
    let mut runtime_devices = Vec::with_capacity(native.devices.len());
    let mut available_device_bytes = 0_u64;
    let mut available_shared_bytes = 0_u64;
    for device in native.devices {
        if device.identity.is_empty()
            || device.identity.len() > 256
            || device.description.len() > 1024
            || device.native_device_id.len() > 256
            || (device.kind.is_usable() && device.physical_pool_bytes == 0)
            || device.available_pool_bytes > device.physical_pool_bytes
        {
            return Err(invalid("native device fields are invalid"));
        }
        let kind = match device.kind {
            NativeDeviceKind::Cpu => HostCapabilityDeviceKind::Cpu,
            NativeDeviceKind::DiscreteGpu => HostCapabilityDeviceKind::DiscreteGpu,
            NativeDeviceKind::IntegratedGpu => HostCapabilityDeviceKind::IntegratedGpu,
            NativeDeviceKind::Accelerator => HostCapabilityDeviceKind::Accelerator,
            NativeDeviceKind::Metadata => HostCapabilityDeviceKind::Metadata,
            NativeDeviceKind::Unknown => HostCapabilityDeviceKind::Unknown,
        };
        match kind {
            HostCapabilityDeviceKind::DiscreteGpu | HostCapabilityDeviceKind::Accelerator => {
                available_device_bytes = available_device_bytes.max(device.available_pool_bytes);
            }
            HostCapabilityDeviceKind::IntegratedGpu => {
                available_shared_bytes = available_shared_bytes.max(device.available_pool_bytes);
            }
            _ => {}
        }
        let (pci_device_id, pci_subsystem_id) = pci_identities(&device.native_device_id);
        runtime_devices.push(EngineDeviceRuntimeIdentity {
            identity: device.identity.clone(),
            description: device.description.clone(),
            native_device_id: device.native_device_id.clone(),
            driver_build_id: driver_build_id(&device.native_device_id, &device.description),
        });
        devices.push(HostCapabilityDevice {
            identity: device.identity,
            kind,
            pci_device_id,
            pci_subsystem_id,
            physical_pool_bytes: device.physical_pool_bytes,
            usable: device.kind.is_usable(),
            supports_gpu_offload: matches!(
                kind,
                HostCapabilityDeviceKind::DiscreteGpu
                    | HostCapabilityDeviceKind::IntegratedGpu
                    | HostCapabilityDeviceKind::Accelerator
            ),
        });
    }

    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu_all();
    let physical_host_bytes = system.total_memory();
    let available_host_bytes = system.available_memory();
    let logical_cpu_cores = system.cpus().len();
    let physical_cpu_cores = System::physical_core_count().unwrap_or(logical_cpu_cores);
    if physical_host_bytes == 0 || physical_cpu_cores == 0 || logical_cpu_cores == 0 {
        return Err(invalid("host memory or CPU topology is unavailable"));
    }
    Ok(DiscoveredInventory {
        inventory: EngineInventory {
            physical_host_bytes,
            physical_cpu_cores,
            logical_cpu_cores,
            devices,
            runtime_devices,
            llama_cpp_commit: native.llama_cpp_commit,
            executable: executable_identity,
        },
        available: ResourcePools {
            host_bytes: available_host_bytes,
            device_bytes: available_device_bytes,
            shared_bytes: available_shared_bytes,
        },
        executable,
    })
}

fn driver_build_id(native_device_id: &str, description: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.driver-build/v1\0");
    digest.update(native_device_id.as_bytes());
    digest.update(b"\0");
    digest.update(description.as_bytes());
    if let Some(kernel) = System::kernel_version() {
        digest.update(b"\0kernel\0");
        digest.update(kernel.as_bytes());
    }
    let normalized = native_device_id
        .strip_prefix("pci:")
        .unwrap_or(native_device_id);
    let device_root = Path::new("/sys/bus/pci/devices").join(normalized);
    if let Ok(module) = std::fs::read_link(device_root.join("driver/module")) {
        digest.update(b"\0module\0");
        digest.update(module.as_os_str().as_encoded_bytes());
        for name in ["version", "srcversion"] {
            if let Ok(value) = std::fs::read_to_string(module.join(name)) {
                digest.update(b"\0");
                digest.update(name.as_bytes());
                digest.update(b"\0");
                digest.update(value.trim().as_bytes());
            }
        }
    }
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing a SHA-256 digest cannot fail");
    }
    output
}

fn open_executable(path: &Path) -> Result<File, InferenceHostStartError> {
    if !path.is_absolute() {
        return Err(invalid("llama-server path must be absolute"));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path).map_err(start_io)?;
    let metadata = file.metadata().map_err(start_io)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(invalid("llama-server must be an executable regular file"));
    }
    Ok(file)
}

fn hash_file(file: &mut File) -> Result<String, InferenceHostStartError> {
    let before = file.metadata().map_err(start_io)?;
    file.seek(SeekFrom::Start(0)).map_err(start_io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(start_io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata().map_err(start_io)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(invalid("llama-server changed while it was verified"));
    }
    file.seek(SeekFrom::Start(0)).map_err(start_io)?;
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(output)
}

fn pci_identities(native_id: &str) -> (Option<String>, Option<String>) {
    let normalized = native_id.strip_prefix("pci:").unwrap_or(native_id);
    if !normalized.contains(':') || !normalized.contains('.') {
        return (None, None);
    }
    let device_root = Path::new("/sys/bus/pci/devices").join(normalized);
    let device = join_pci_words(
        read_sysfs_pci_word(&device_root.join("vendor")),
        read_sysfs_pci_word(&device_root.join("device")),
    );
    let subsystem = join_pci_words(
        read_sysfs_pci_word(&device_root.join("subsystem_vendor")),
        read_sysfs_pci_word(&device_root.join("subsystem_device")),
    );
    (device, subsystem)
}

fn read_sysfs_pci_word(path: &Path) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn join_pci_words(vendor: Option<String>, device: Option<String>) -> Option<String> {
    Some(format!("{}:{}", vendor?, device?))
}

fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn bounded_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned()
}

fn invalid(reason: &str) -> InferenceHostStartError {
    InferenceHostStartError::InvalidEngineInventory {
        reason: reason.to_owned(),
    }
}

fn start_io(error: std::io::Error) -> InferenceHostStartError {
    InferenceHostStartError::EngineStart {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::join_pci_words;

    #[test]
    fn pci_profile_identity_contains_vendor_and_device_words() {
        assert_eq!(
            join_pci_words(Some("1002".to_owned()), Some("744c".to_owned())).as_deref(),
            Some("1002:744c")
        );
        assert_eq!(
            join_pci_words(Some("1da2".to_owned()), Some("471e".to_owned())).as_deref(),
            Some("1da2:471e")
        );
        assert_eq!(join_pci_words(None, Some("744c".to_owned())), None);
    }
}
