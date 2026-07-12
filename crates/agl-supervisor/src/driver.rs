use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{RunId, StepId};
use agl_store::{DurableRunRecord, EffectDeliveryClass, RunState, RunUsage};
use serde::{Deserialize, Serialize};

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

impl std::fmt::Debug for RunCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorEffect {
    pub sequence: u64,
    pub kind: String,
    pub delivery_class: EffectDeliveryClass,
    pub request: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorTerminal {
    pub state: RunState,
    pub result: Option<serde_json::Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DriverSnapshot {
    pub checkpoint: serde_json::Value,
    pub pending_effect: Option<SupervisorEffect>,
    pub events: Vec<SafeRuntimeEventEnvelope>,
    pub terminal: Option<SupervisorTerminal>,
    pub usage: RunUsage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverEffectError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_limit_exempt: bool,
}

#[derive(Clone, Debug)]
pub struct EffectExecutionContext {
    pub run_id: RunId,
    pub step_id: StepId,
    pub attempt: u32,
    pub cancellation: RunCancellation,
}

impl DriverEffectError {
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

    fn execute_pending_effect(
        &mut self,
        context: &EffectExecutionContext,
    ) -> std::result::Result<serde_json::Value, DriverEffectError>;
}

pub trait DurableRunDriverFactory: Send + Sync + 'static {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> Result<Box<dyn DurableRunDriver>>;
}
