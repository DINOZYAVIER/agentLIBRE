use std::sync::Arc;
use std::time::Instant;

use agl_ids::SessionId;
use agl_inference::{
    ArtifactFileHandle, InferenceCancellation, InferenceDeviceInfo, InferenceHost,
    InferenceOutputSink, InferencePlanRejectionEvidence, InferenceRequest, InferenceResponse,
    ModelManagerError, ModelManagerStatus, ModelManagerStatusDetail, ModelUnloadResult,
    ModelUnloadTarget, ResolvedMediaAttachment, VolatileHandles,
};
use agl_model::{HostCapabilities, ModelContextKey, ModelExecutionPlan};
use anyhow::{Result, ensure};

#[derive(Clone)]
pub struct ChatInferenceJob {
    pub plan: ModelExecutionPlan,
    pub artifacts: Vec<ArtifactFileHandle>,
    pub media: Vec<ResolvedMediaAttachment>,
    pub session_id: SessionId,
    pub request: InferenceRequest,
    pub cancellation: InferenceCancellation,
    pub deadline: Option<Instant>,
    pub output_sink: Arc<dyn InferenceOutputSink>,
    pub evidence_root: Option<std::path::PathBuf>,
    pub product_resolution: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct ChatPlanRejection {
    pub request: InferenceRequest,
    pub rejection: InferencePlanRejectionEvidence,
    pub evidence_root: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for ChatInferenceJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatInferenceJob")
            .field("plan", &self.plan.digest())
            .field("session_id", &self.session_id)
            .field("media_parts", &self.media.len())
            .field("request", &self.request)
            .field("cancellation", &self.cancellation)
            .field("deadline", &self.deadline)
            .field("output_sink", &"<volatile>")
            .field("evidence_root", &self.evidence_root)
            .finish()
    }
}

pub trait InferenceClient: Send + Sync + 'static {
    #[cfg(not(test))]
    fn static_capabilities(&self) -> Result<HostCapabilities> {
        anyhow::bail!("inference client has no canonical HostCapabilities")
    }

    #[cfg(test)]
    fn static_capabilities(&self) -> Result<HostCapabilities> {
        Ok(test_host_capabilities())
    }

    #[cfg(not(test))]
    fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>>;

    #[cfg(test)]
    fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
        Ok(Vec::new())
    }

    fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse>;

    fn record_plan_rejection(&self, _rejection: ChatPlanRejection) -> Result<()> {
        anyhow::bail!("inference client cannot durably record a Model plan rejection")
    }

    fn clear_context(&self, context: &ModelContextKey) -> Result<()>;

    fn release_context(&self, context: &ModelContextKey) -> Result<()>;

    fn status(&self) -> Result<ModelManagerStatus>;

    fn status_with_detail(&self, detail: ModelManagerStatusDetail) -> Result<ModelManagerStatus> {
        if detail != ModelManagerStatusDetail::Aggregate {
            anyhow::bail!("detailed inference status is unavailable");
        }
        self.status()
    }

    fn unload(&self, _target: ModelUnloadTarget) -> Result<ModelUnloadResult> {
        anyhow::bail!("model unload is unavailable")
    }
}

#[derive(Clone)]
pub struct InferenceClientHandle {
    inner: Arc<dyn InferenceClient>,
}

impl InferenceClientHandle {
    pub fn new(client: impl InferenceClient) -> Self {
        Self {
            inner: Arc::new(client),
        }
    }

    pub fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse> {
        self.inner.generate(job)
    }

    pub fn record_plan_rejection(&self, rejection: ChatPlanRejection) -> Result<()> {
        self.inner.record_plan_rejection(rejection)
    }

    pub fn static_capabilities(&self) -> Result<HostCapabilities> {
        self.inner.static_capabilities()
    }

    pub fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
        self.inner.device_inventory()
    }

    pub fn clear_context(&self, context: &ModelContextKey) -> Result<()> {
        self.inner.clear_context(context)
    }

    pub fn release_context(&self, context: &ModelContextKey) -> Result<()> {
        self.inner.release_context(context)
    }

    pub fn status(&self) -> Result<ModelManagerStatus> {
        self.inner.status()
    }

    pub fn status_with_detail(
        &self,
        detail: ModelManagerStatusDetail,
    ) -> Result<ModelManagerStatus> {
        self.inner.status_with_detail(detail)
    }

    pub fn unload(&self, target: ModelUnloadTarget) -> Result<ModelUnloadResult> {
        self.inner.unload(target)
    }
}

impl InferenceClient for InferenceHost {
    fn static_capabilities(&self) -> Result<HostCapabilities> {
        Ok(InferenceHost::static_capabilities(self).clone())
    }

    fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
        Ok(host_device_inventory(self))
    }

    fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse> {
        ensure_managed_session(&job.session_id, job.request.session_id.as_ref())?;
        if job.cancellation.is_cancelled() {
            return Err(ModelManagerError::Cancelled.into());
        }
        if job
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(ModelManagerError::DeadlineExceeded.into());
        }
        self.submit_blocking(
            job.plan,
            job.request,
            job.artifacts,
            VolatileHandles {
                media: job.media,
                cancellation: job.cancellation,
                deadline: job.deadline,
                output_sink: job.output_sink,
                evidence_root: job.evidence_root,
                product_resolution: job.product_resolution,
            },
        )
        .map_err(host_failure)
        .map_err(Into::into)
    }

    fn record_plan_rejection(&self, rejection: ChatPlanRejection) -> Result<()> {
        InferenceHost::record_plan_rejection(
            self,
            &rejection.request,
            rejection.rejection,
            rejection.evidence_root.as_deref(),
        )
        .map_err(host_failure)?;
        Ok(())
    }

    fn clear_context(&self, context: &ModelContextKey) -> Result<()> {
        InferenceHost::clear_context(self, context).map_err(host_failure)?;
        Ok(())
    }

    fn release_context(&self, context: &ModelContextKey) -> Result<()> {
        InferenceHost::release_context(self, context).map_err(host_failure)?;
        Ok(())
    }

    fn status(&self) -> Result<ModelManagerStatus> {
        let status = InferenceHost::status(self);
        Ok(ModelManagerStatus {
            queue_depth: status.pending,
            resident_models: status.resident_models,
            resident_contexts: status.resident_contexts,
            resident_model_digests: status.resident_model_digest.into_iter().collect(),
            ..ModelManagerStatus::default()
        })
    }

    fn unload(&self, target: ModelUnloadTarget) -> Result<ModelUnloadResult> {
        let released = match target {
            ModelUnloadTarget::All => self.unload_all().map_err(host_failure)?,
            ModelUnloadTarget::Digest(digest) => {
                self.unload_model_digest(&digest).map_err(host_failure)?
            }
        };
        Ok(if released {
            ModelUnloadResult {
                matched_models: 1,
                released_models: 1,
                released_contexts: 1,
                outcome: agl_inference::ModelUnloadOutcome::Released,
            }
        } else {
            ModelUnloadResult::not_resident()
        })
    }
}

fn host_failure(error: agl_inference::InferenceFailure) -> ModelManagerError {
    match error {
        agl_inference::InferenceFailure::Cancelled => ModelManagerError::Cancelled,
        agl_inference::InferenceFailure::DeadlineExceeded => ModelManagerError::DeadlineExceeded,
        agl_inference::InferenceFailure::Queue(agl_inference::InferenceQueueRejection::Full {
            capacity,
            ..
        }) => ModelManagerError::QueueFull { capacity },
        agl_inference::InferenceFailure::Queue(
            agl_inference::InferenceQueueRejection::ShuttingDown,
        ) => ModelManagerError::ManagerUnavailable,
        error => ModelManagerError::GenerationFailed {
            message: error.to_string(),
        },
    }
}

fn ensure_managed_session(
    managed_session_id: &SessionId,
    request_session_id: Option<&SessionId>,
) -> Result<()> {
    ensure!(
        request_session_id == Some(managed_session_id),
        "inference request session does not match its managed context"
    );
    Ok(())
}

impl From<InferenceHost> for InferenceClientHandle {
    fn from(host: InferenceHost) -> Self {
        Self::new(host)
    }
}

fn host_device_inventory(host: &InferenceHost) -> Vec<InferenceDeviceInfo> {
    host.engine_inventory()
        .devices
        .iter()
        .map(|device| InferenceDeviceInfo {
            physical_device_id: device.identity.clone(),
            pci_device_id: device.pci_device_id.clone(),
            pci_subsystem_id: device.pci_subsystem_id.clone(),
            driver_build_id: host
                .engine_inventory()
                .runtime_devices
                .iter()
                .find(|runtime| runtime.identity == device.identity)
                .map(|runtime| runtime.driver_build_id.clone())
                .unwrap_or_default(),
            backend_name: device.identity.clone(),
            description: device.identity.clone(),
            kind: match device.kind {
                agl_model::HostCapabilityDeviceKind::Cpu => agl_inference::InferenceDeviceKind::Cpu,
                agl_model::HostCapabilityDeviceKind::DiscreteGpu => {
                    agl_inference::InferenceDeviceKind::DiscreteGpu
                }
                agl_model::HostCapabilityDeviceKind::IntegratedGpu => {
                    agl_inference::InferenceDeviceKind::IntegratedGpu
                }
                agl_model::HostCapabilityDeviceKind::Accelerator => {
                    agl_inference::InferenceDeviceKind::Accelerator
                }
                agl_model::HostCapabilityDeviceKind::Metadata => {
                    agl_inference::InferenceDeviceKind::Metadata
                }
                agl_model::HostCapabilityDeviceKind::Unknown => {
                    agl_inference::InferenceDeviceKind::Unknown
                }
            },
            free_memory_bytes: device.physical_pool_bytes,
            total_memory_bytes: device.physical_pool_bytes,
            usable: device.usable,
            supports_gpu_offload: device.supports_gpu_offload,
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn test_inference_client() -> InferenceClientHandle {
    struct TestInferenceClient;

    impl InferenceClient for TestInferenceClient {
        fn static_capabilities(&self) -> Result<HostCapabilities> {
            Ok(test_host_capabilities())
        }
        fn generate(&self, _job: ChatInferenceJob) -> Result<InferenceResponse> {
            anyhow::bail!("test inference client has no scripted response")
        }

        fn clear_context(&self, _context: &ModelContextKey) -> Result<()> {
            Ok(())
        }

        fn release_context(&self, _context: &ModelContextKey) -> Result<()> {
            Ok(())
        }

        fn status(&self) -> Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }
    }

    InferenceClientHandle::new(TestInferenceClient)
}

#[cfg(test)]
fn test_host_capabilities() -> HostCapabilities {
    HostCapabilities {
        physical_host_bytes: u64::MAX / 2,
        physical_cpu_cores: 4,
        logical_cpu_cores: 4,
        devices: vec![agl_model::HostCapabilityDevice {
            identity: "CPU".to_owned(),
            kind: agl_model::HostCapabilityDeviceKind::Cpu,
            pci_device_id: None,
            pci_subsystem_id: None,
            physical_pool_bytes: u64::MAX / 2,
            usable: true,
            supports_gpu_offload: false,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_context_rejects_a_different_request_session() {
        let managed = SessionId::generate();
        let other = SessionId::generate();

        assert!(ensure_managed_session(&managed, Some(&managed)).is_ok());
        assert!(ensure_managed_session(&managed, Some(&other)).is_err());
        assert!(ensure_managed_session(&managed, None).is_err());
    }

    #[test]
    fn handle_forwards_host_safe_device_inventory() {
        struct InventoryClient;

        impl InferenceClient for InventoryClient {
            fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
                Ok(vec![InferenceDeviceInfo {
                    physical_device_id: "physical-test-0".to_string(),
                    pci_device_id: None,
                    pci_subsystem_id: None,
                    driver_build_id: "sha256:test-driver".to_string(),
                    backend_name: "fake0".to_string(),
                    description: "Fake accelerator".to_string(),
                    kind: agl_inference::InferenceDeviceKind::Accelerator,
                    free_memory_bytes: 10,
                    total_memory_bytes: 20,
                    usable: true,
                    supports_gpu_offload: true,
                }])
            }

            fn generate(&self, _job: ChatInferenceJob) -> Result<InferenceResponse> {
                anyhow::bail!("inventory forwarding test does not generate")
            }

            fn clear_context(&self, _context: &ModelContextKey) -> Result<()> {
                Ok(())
            }

            fn release_context(&self, _context: &ModelContextKey) -> Result<()> {
                Ok(())
            }

            fn status(&self) -> Result<ModelManagerStatus> {
                Ok(ModelManagerStatus::default())
            }
        }

        let devices = InferenceClientHandle::new(InventoryClient)
            .device_inventory()
            .unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].physical_device_id, "physical-test-0");
    }
}
