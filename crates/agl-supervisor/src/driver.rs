use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{RunId, StepId};
use agl_kernel::{
    CancellationSignal, DurableRunRecord, RunRequest, RunRequestResult, RunTerminalOutcome,
    RunUsage,
};

use crate::Result;

#[derive(Clone, Default)]
pub struct RunCancellation {
    cancelled: Arc<AtomicBool>,
}

impl RunCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl CancellationSignal for RunCancellation {
    fn is_cancelled(&self) -> bool {
        RunCancellation::is_cancelled(self)
    }
}

impl std::fmt::Debug for RunCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DriverSnapshot {
    pub checkpoint: serde_json::Value,
    pub pending_request: Option<RunRequest>,
    pub events: Vec<SafeRuntimeEventEnvelope>,
    pub terminal: Option<RunTerminalOutcome>,
    pub usage: RunUsage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequestError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_limit_exempt: bool,
}

#[derive(Clone, Debug)]
pub struct RunRequestContext {
    pub run_id: RunId,
    pub step_id: StepId,
    pub attempt: u32,
    pub cancellation: RunCancellation,
}

impl RunRequestError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            retry_limit_exempt: false,
        }
    }

    pub fn durable_wait(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: true,
            retry_limit_exempt: true,
        }
    }
}

pub trait DurableRunDriver: Send {
    fn snapshot(&mut self) -> Result<DriverSnapshot>;

    fn execute_pending_request(
        &mut self,
        context: &RunRequestContext,
    ) -> std::result::Result<RunRequestResult, RunRequestError>;
}

pub trait DurableRunDriverFactory: Send + Sync + 'static {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> Result<Box<dyn DurableRunDriver>>;
}
