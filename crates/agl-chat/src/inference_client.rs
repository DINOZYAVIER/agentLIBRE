use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use agl_config::ResolvedInferenceConfig;
use agl_ids::SessionId;
use agl_inference::evidence::InferenceArtifactRoot;
use agl_inference::{
    ContextKey, InferenceCancellation, InferenceDeviceInfo, InferenceJob, InferenceOutputSink,
    InferenceRequest, InferenceResponse, ModelManagerHandle, ModelManagerStatus,
};
use anyhow::{Result, ensure};

#[derive(Clone)]
pub struct ChatInferenceJob {
    pub config: ResolvedInferenceConfig,
    pub artifact_root: InferenceArtifactRoot,
    pub content_store_root: PathBuf,
    pub max_output_tokens: u32,
    pub session_id: SessionId,
    pub request: InferenceRequest,
    pub cancellation: InferenceCancellation,
    pub deadline: Option<Instant>,
    pub output_sink: Arc<dyn InferenceOutputSink>,
}

impl std::fmt::Debug for ChatInferenceJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatInferenceJob")
            .field("config", &self.config)
            .field("artifact_root", &self.artifact_root)
            .field("content_store_root", &self.content_store_root)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("session_id", &self.session_id)
            .field("request", &self.request)
            .field("cancellation", &self.cancellation)
            .field("deadline", &self.deadline)
            .field("output_sink", &"<volatile>")
            .finish()
    }
}

pub trait InferenceClient: Send + Sync + 'static {
    #[cfg(not(test))]
    fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>>;

    #[cfg(test)]
    fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
        Ok(Vec::new())
    }

    fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse>;

    fn clear_context(&self, config: &ResolvedInferenceConfig, session_id: &SessionId)
    -> Result<()>;

    fn release_context(
        &self,
        config: &ResolvedInferenceConfig,
        session_id: &SessionId,
    ) -> Result<()>;

    fn status(&self) -> Result<ModelManagerStatus>;
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

    pub fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
        self.inner.device_inventory()
    }

    pub fn clear_context(
        &self,
        config: &ResolvedInferenceConfig,
        session_id: &SessionId,
    ) -> Result<()> {
        self.inner.clear_context(config, session_id)
    }

    pub fn release_context(
        &self,
        config: &ResolvedInferenceConfig,
        session_id: &SessionId,
    ) -> Result<()> {
        self.inner.release_context(config, session_id)
    }

    pub fn status(&self) -> Result<ModelManagerStatus> {
        self.inner.status()
    }
}

impl InferenceClient for ModelManagerHandle {
    fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>> {
        Ok(ModelManagerHandle::device_inventory(self)?)
    }

    fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse> {
        ensure_managed_session(&job.session_id, job.request.session_id.as_ref())?;
        let context_key = ContextKey::for_conversation(&job.config, job.session_id.as_str())?;
        let mut inference_job = InferenceJob::new(
            job.config,
            job.request,
            context_key,
            job.artifact_root,
            job.content_store_root,
            job.max_output_tokens,
            job.output_sink,
        )?
        .with_cancellation(job.cancellation);
        if let Some(deadline) = job.deadline {
            inference_job = inference_job.with_deadline(deadline);
        }
        Ok(ModelManagerHandle::generate(self, inference_job)?)
    }

    fn clear_context(
        &self,
        config: &ResolvedInferenceConfig,
        session_id: &SessionId,
    ) -> Result<()> {
        let context_key = ContextKey::for_conversation(config, session_id.as_str())?;
        Ok(ModelManagerHandle::clear_context(self, &context_key)?)
    }

    fn release_context(
        &self,
        config: &ResolvedInferenceConfig,
        session_id: &SessionId,
    ) -> Result<()> {
        let context_key = ContextKey::for_conversation(config, session_id.as_str())?;
        Ok(ModelManagerHandle::release_context(self, &context_key)?)
    }

    fn status(&self) -> Result<ModelManagerStatus> {
        Ok(ModelManagerHandle::status(self)?)
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

impl From<ModelManagerHandle> for InferenceClientHandle {
    fn from(handle: ModelManagerHandle) -> Self {
        Self::new(handle)
    }
}

#[cfg(test)]
pub(crate) fn test_inference_client() -> InferenceClientHandle {
    struct TestInferenceClient;

    impl InferenceClient for TestInferenceClient {
        fn generate(&self, _job: ChatInferenceJob) -> Result<InferenceResponse> {
            anyhow::bail!("test inference client has no scripted response")
        }

        fn clear_context(
            &self,
            _config: &ResolvedInferenceConfig,
            _session_id: &SessionId,
        ) -> Result<()> {
            Ok(())
        }

        fn release_context(
            &self,
            _config: &ResolvedInferenceConfig,
            _session_id: &SessionId,
        ) -> Result<()> {
            Ok(())
        }

        fn status(&self) -> Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }
    }

    InferenceClientHandle::new(TestInferenceClient)
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

            fn clear_context(
                &self,
                _config: &ResolvedInferenceConfig,
                _session_id: &SessionId,
            ) -> Result<()> {
                Ok(())
            }

            fn release_context(
                &self,
                _config: &ResolvedInferenceConfig,
                _session_id: &SessionId,
            ) -> Result<()> {
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
