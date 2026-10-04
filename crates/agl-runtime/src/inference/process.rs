//! Daemon-supplied private process launch, independent of an execution service.
use agl_core::execution::{
    ExecutionId, ExecutionOutput, ExecutionOutputStream, ExecutionStartRequest, ExecutionStatus,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivateInference {
    pub artifacts: Vec<String>,
    pub listen_socket: String,
    pub device_paths: Vec<String>,
    pub address_space_limit_bytes: u64,
}
impl PrivateInference {
    pub fn validate(&self, request: &ExecutionStartRequest) -> Result<(), String> {
        let absolute = |value: &str| {
            !value.is_empty()
                && value.len() <= 32768
                && !value.contains('\0')
                && std::path::Path::new(value).is_absolute()
        };
        if !matches!(
            request.owner,
            agl_core::execution::ExecutionOwner::Runtime { .. }
        ) || request.io != agl_core::execution::ExecutionIo::Pipes
            || !matches!(
                request.isolation,
                agl_core::execution::ExecutionIsolation::Standard
            )
            || self.artifacts.is_empty()
            || self.artifacts.len() > 34
            || self.address_space_limit_bytes == 0
            || !absolute(&self.listen_socket)
            || self.artifacts.iter().any(|path| !absolute(path))
            || self.device_paths.len() > 64
            || self.device_paths.iter().any(|path| !absolute(path))
        {
            return Err("invalid private inference isolation request".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub enum ProcessAction {
    Start {
        request: Box<ExecutionStartRequest>,
        isolation: PrivateInference,
    },
    Inspect(ExecutionId),
    Read {
        execution_id: ExecutionId,
        offset: u64,
        max_bytes: u32,
    },
    Stop(ExecutionId),
}
pub enum ProcessResult {
    Status(ExecutionStatus),
    Output(ExecutionOutput),
    Stopped,
}
#[derive(Clone)]
pub struct ProcessHost(Arc<dyn Fn(ProcessAction) -> Result<ProcessResult, String> + Send + Sync>);
impl ProcessHost {
    pub fn new(
        call: impl Fn(ProcessAction) -> Result<ProcessResult, String> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(call))
    }
    pub fn start(
        &self,
        request: ExecutionStartRequest,
        isolation: PrivateInference,
    ) -> Result<ExecutionStatus, String> {
        match (self.0)(ProcessAction::Start {
            request: Box::new(request),
            isolation,
        })? {
            ProcessResult::Status(status) => Ok(status),
            _ => Err("invalid start response".into()),
        }
    }
    pub fn inspect(&self, id: ExecutionId) -> Result<ExecutionStatus, String> {
        match (self.0)(ProcessAction::Inspect(id))? {
            ProcessResult::Status(status) => Ok(status),
            _ => Err("invalid inspect response".into()),
        }
    }
    pub fn read(
        &self,
        execution_id: ExecutionId,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ExecutionOutput, String> {
        match (self.0)(ProcessAction::Read {
            execution_id,
            offset,
            max_bytes,
        })? {
            ProcessResult::Output(output) if output.stream == ExecutionOutputStream::Stderr => {
                Ok(output)
            }
            _ => Err("invalid read response".into()),
        }
    }
    pub fn stop(&self, id: ExecutionId) -> Result<(), String> {
        (self.0)(ProcessAction::Stop(id)).map(|_| ())
    }
}
