use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Host-safe accelerator classification without native backend handles.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceDeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

/// Product inventory data returned through the inference runtime seam.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceDeviceInfo {
    pub physical_device_id: String,
    pub pci_device_id: Option<String>,
    pub pci_subsystem_id: Option<String>,
    pub driver_build_id: String,
    pub backend_name: String,
    pub description: String,
    pub kind: InferenceDeviceKind,
    pub free_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub usable: bool,
    pub supports_gpu_offload: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HostCapabilityProjectionError {
    #[error("host memory or CPU topology is unavailable")]
    MissingHostCapacity,
    #[error("inference device `{identity}` has an invalid physical pool")]
    InvalidDevicePool { identity: String },
}

/// The sole product-inventory to static-planner conversion boundary.
pub fn project_host_capabilities(
    devices: impl IntoIterator<Item = InferenceDeviceInfo>,
) -> Result<agl_model::HostCapabilities, HostCapabilityProjectionError> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.refresh_cpu_all();
    let physical_host_bytes = system.total_memory();
    let physical_cpu_cores = sysinfo::System::physical_core_count().unwrap_or(0);
    let logical_cpu_cores = system.cpus().len();
    if physical_host_bytes == 0 || physical_cpu_cores == 0 || logical_cpu_cores < physical_cpu_cores
    {
        return Err(HostCapabilityProjectionError::MissingHostCapacity);
    }
    let devices = devices
        .into_iter()
        .map(|device| {
            if device.total_memory_bytes == 0 {
                return Err(HostCapabilityProjectionError::InvalidDevicePool {
                    identity: device.physical_device_id,
                });
            }
            Ok(agl_model::HostCapabilityDevice {
                identity: device.physical_device_id,
                kind: match device.kind {
                    InferenceDeviceKind::Cpu => agl_model::HostCapabilityDeviceKind::Cpu,
                    InferenceDeviceKind::DiscreteGpu => {
                        agl_model::HostCapabilityDeviceKind::DiscreteGpu
                    }
                    InferenceDeviceKind::IntegratedGpu => {
                        agl_model::HostCapabilityDeviceKind::IntegratedGpu
                    }
                    InferenceDeviceKind::Accelerator => {
                        agl_model::HostCapabilityDeviceKind::Accelerator
                    }
                    InferenceDeviceKind::Metadata => agl_model::HostCapabilityDeviceKind::Metadata,
                    InferenceDeviceKind::Unknown => agl_model::HostCapabilityDeviceKind::Unknown,
                },
                pci_device_id: device.pci_device_id,
                pci_subsystem_id: device.pci_subsystem_id,
                physical_pool_bytes: device.total_memory_bytes,
                usable: device.usable,
                supports_gpu_offload: device.supports_gpu_offload,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(agl_model::HostCapabilities {
        physical_host_bytes,
        physical_cpu_cores,
        logical_cpu_cores,
        devices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_inventory_round_trips_and_rejects_unknown_fields() {
        let info = InferenceDeviceInfo {
            physical_device_id: "0000:03:00.0".to_string(),
            pci_device_id: Some("1002:744c".to_string()),
            pci_subsystem_id: Some("1da2:471e".to_string()),
            driver_build_id: "sha256:driver".to_string(),
            backend_name: "Vulkan0".to_string(),
            description: "GPU".to_string(),
            kind: InferenceDeviceKind::DiscreteGpu,
            free_memory_bytes: 7,
            total_memory_bytes: 11,
            usable: true,
            supports_gpu_offload: true,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(
            serde_json::from_value::<InferenceDeviceInfo>(value.clone()).unwrap(),
            info
        );

        let mut unknown = value;
        unknown["native_handle"] = serde_json::json!(1);
        assert!(serde_json::from_value::<InferenceDeviceInfo>(unknown).is_err());
    }
}
