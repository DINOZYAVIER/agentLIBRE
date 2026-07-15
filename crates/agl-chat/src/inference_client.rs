use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use agl_config::ResolvedInferenceConfig;
use agl_ids::SessionId;
use agl_inference::evidence::InferenceArtifactRoot;
use agl_inference::{
    ContextKey, InferenceCancellation, InferenceJob, InferenceRequest, InferenceResponse,
    ModelManagerHandle, ModelManagerStatus,
};
use anyhow::{Result, ensure};

#[derive(Clone, Debug)]
pub struct ChatInferenceJob {
    pub config: ResolvedInferenceConfig,
    pub artifact_root: InferenceArtifactRoot,
    pub content_store_root: PathBuf,
    pub max_output_tokens: u32,
    pub session_id: SessionId,
    pub request: InferenceRequest,
    pub cancellation: InferenceCancellation,
    pub deadline: Option<Instant>,
}

pub trait InferenceClient: Send + Sync + 'static {
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
}
