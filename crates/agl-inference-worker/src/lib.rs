pub mod native;
mod native_bundle;
pub mod sandbox;
pub mod service;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

pub use service::{WorkerServiceRuntime, serve_with_runtime};

use std::path::Path;

use agl_inference::worker_protocol::{
    DeviceKind, DeviceSnapshot, DeviceSnapshotEntry, Result, WorkerControlChannel,
    WorkerProtocolError, WorkerProtocolErrorCode,
};
use agl_inference::{InferenceDeviceInfo, InferenceDeviceKind};
use native::{LlamaCppModelRuntime, llama_cpp_inference_device_inventory};

pub fn run_from_inherited_channel() -> Result<()> {
    let channel = WorkerControlChannel::from_inherited_env()?;
    channel.validate_inherited_process_hardening()?;
    let native_bundle = native_bundle::validate_for_current_executable().map_err(|error| {
        WorkerProtocolError::new(
            WorkerProtocolErrorCode::WorkerUntrusted,
            format!("exact native inference bundle validation failed: {error}"),
        )
    })?;
    let native_library_dir = native_bundle.directory().to_path_buf();
    let sandbox_native_library_dir = native_library_dir.clone();
    let inventory_native_library_dir = native_library_dir.clone();
    serve_with_runtime(
        channel,
        move || {
            Ok(LlamaCppModelRuntime::with_native_library_dir(
                native_library_dir,
            ))
        },
        move || production_device_snapshot(&inventory_native_library_dir),
        move |configuration, control_fd| {
            if !configuration
                .runtime_roots()
                .iter()
                .any(|root| Path::new(root) == sandbox_native_library_dir)
            {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::WorkerUntrusted,
                    "sandbox configuration omitted the exact native bundle directory",
                ));
            }
            sandbox::enter(configuration, control_fd)
                .map(|_| ())
                .map_err(|error| {
                    WorkerProtocolError::new(
                        WorkerProtocolErrorCode::WorkerUntrusted,
                        format!("inference worker sandbox admission failed: {error}"),
                    )
                })
        },
    )
}

fn production_device_snapshot(native_library_dir: &std::path::Path) -> Result<DeviceSnapshot> {
    let inventory =
        llama_cpp_inference_device_inventory(native_library_dir).map_err(|failure| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUnavailable,
                format!("native device inventory failed: {}", failure.message()),
            )
        })?;
    device_snapshot_from_inventory(inventory)
}

fn device_snapshot_from_inventory(inventory: Vec<InferenceDeviceInfo>) -> Result<DeviceSnapshot> {
    let devices = inventory
        .into_iter()
        .map(|device| {
            DeviceSnapshotEntry::new(
                device.physical_device_id,
                device.driver_build_id,
                device.backend_name,
                device.description,
                match device.kind {
                    InferenceDeviceKind::Cpu => DeviceKind::Cpu,
                    InferenceDeviceKind::DiscreteGpu => DeviceKind::DiscreteGpu,
                    InferenceDeviceKind::IntegratedGpu => DeviceKind::IntegratedGpu,
                    InferenceDeviceKind::Accelerator => DeviceKind::Accelerator,
                    InferenceDeviceKind::Metadata => DeviceKind::Metadata,
                    InferenceDeviceKind::Unknown => DeviceKind::Unknown,
                },
                device.free_memory_bytes,
                device.total_memory_bytes,
                device.usable,
                device.supports_gpu_offload,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    DeviceSnapshot::new(devices)
}

#[cfg(test)]
mod tests {
    use agl_inference::worker_protocol::WORKER_BUILD_ID;

    use super::*;

    #[test]
    fn production_snapshot_keeps_backend_name_out_of_authority_identity() {
        let snapshot = device_snapshot_from_inventory(vec![InferenceDeviceInfo {
            physical_device_id: "pci:0000:03:00.0".to_string(),
            pci_device_id: None,
            pci_subsystem_id: None,
            driver_build_id: WORKER_BUILD_ID.to_string(),
            backend_name: "Vulkan0".to_string(),
            description: "display-only GPU description".to_string(),
            kind: InferenceDeviceKind::DiscreteGpu,
            free_memory_bytes: 0,
            total_memory_bytes: 0,
            usable: true,
            supports_gpu_offload: true,
        }])
        .expect("build worker snapshot");

        let device = &snapshot.devices()[0];
        assert_eq!(device.device_id(), "pci:0000:03:00.0");
        assert_eq!(device.driver_build_id(), WORKER_BUILD_ID);
        assert_eq!(device.backend_name(), "Vulkan0");
        assert!(device.usable());
        assert!(device.supports_gpu_offload());
        assert_eq!(device.free_memory_bytes(), 0);
        assert_eq!(device.total_memory_bytes(), 0);
    }
}
