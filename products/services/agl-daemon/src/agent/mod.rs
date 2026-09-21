use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(test)]
use agl_core::JsonSchema;
use agl_core::agent::{
    AgentFsm, AgentFsmInput, AgentFsmState, AgentOperation, AgentOperationFailure,
    AgentOperationFailureKind, AgentOperationFsm, AgentOperationFsmInput, AgentOperationRequest,
    AgentOperationResult, AgentRun, AgentRunSpec, AgentRunStatus, ExtensionDefinitionDigest,
    ToolDefinitionDigest, ToolFailureKind, ToolResult,
};
use agl_core::{
    AgentRunId, CanonicalJson, Content, EffectId, ExtensionId, Fsm, MessageId, ToolDefinition,
    ToolId,
};
use agl_daemon_api::{AgentProgress, AgentProgressStatus};
use agl_runtime::extension::{
    ExtensionBindings, ToolBinding, ToolCancellation, ToolContext, ToolFuture,
};
use agl_runtime::inference::{
    InferenceCancellation, InferenceGenerateRequest, InferenceHealthSink, InferenceProgressSink,
    InferenceRoute, InferenceServiceError,
};
use sha2::{Digest as _, Sha256};

use crate::store::{AgentRunAdmission, StoreHandle};

mod common;
mod compaction;
mod memory_io;
mod operation_driver;
mod progress;
mod run_driver;
mod scheduler;
mod service;

pub(crate) use common::AgentServiceError;
pub(crate) use common::{map_inference_failure, now_ms};
#[cfg(test)]
use operation_driver::validate_tool_result;
pub(crate) use operation_driver::{OperationExecution, operation_message_id};
pub(crate) use progress::{
    AgentProgressSubscription, ProgressSubscriber, ProgressSubscribers, SubscriptionSignal,
    emit_durable, emit_progress,
};
pub(crate) use run_driver::{admit, drive, validate_snapshot};
pub(crate) use scheduler::{
    Command, load_recoverable_runs, map_command_send, prepare_tools, worker_loop,
};
#[cfg(test)]
use service::PROGRESS_CAPACITY;
pub(crate) use service::{AgentDependencies, AgentHandle, AgentService};
pub(crate) use service::{
    MAX_ACTIVE_RUNS, PreparedTool, RECOVERY_PAGE_CAPACITY, RunCancellation,
    SCHEDULER_QUEUE_CAPACITY, sha256,
};

include!("tests.rs");
