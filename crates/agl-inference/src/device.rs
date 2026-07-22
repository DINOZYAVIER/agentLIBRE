use serde::{Deserialize, Serialize};

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
    pub driver_build_id: String,
    pub backend_name: String,
    pub description: String,
    pub kind: InferenceDeviceKind,
    pub free_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub usable: bool,
    pub supports_gpu_offload: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_inventory_round_trips_and_rejects_unknown_fields() {
        let info = InferenceDeviceInfo {
            physical_device_id: "0000:03:00.0".to_string(),
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
