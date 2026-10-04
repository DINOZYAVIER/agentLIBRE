//! Native Tool handler and binding interfaces, independent of inference.

#![forbid(unsafe_code)]

mod forge_file;
pub mod image;
pub use forge_file::{ForgeFile, VerificationStatus};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{ffi::OsString, path::PathBuf, time::Duration};

use agl_core::agent::{
    AgentOperationKey, AuthorityGrantSet, ToolDefinitionDigest, ToolFailure, ToolResult,
    WorkspaceScope,
};
use agl_core::package::{PackageTreeDigest, PackageVersion};
use agl_core::{AuthorityGrant, ConversationId, ExtensionDefinition, ToolId};

pub type ToolFuture =
    Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + 'static>>;

#[derive(Clone, Default)]
pub struct ToolCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ToolCancellation {
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

impl std::fmt::Debug for ToolCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub operation: AgentOperationKey,
    pub conversation_id: Option<ConversationId>,
    pub workspace: WorkspaceScope,
    pub authority: AuthorityGrantSet,
    pub deadline_at_ms: i64,
    pub result_bytes: u64,
    /// True only for the planner operation. First-party handlers use this to
    /// enforce the read-only workspace boundary at execution time.
    pub read_only_workspace: bool,
    /// Host-declared build-output trees omitted from planner execution views.
    pub output_directories: Vec<String>,
    cancellation: ToolCancellation,
}

impl ToolContext {
    pub fn new(
        operation: AgentOperationKey,
        conversation_id: Option<ConversationId>,
        workspace: WorkspaceScope,
        authority: AuthorityGrantSet,
        deadline_at_ms: i64,
        result_bytes: u64,
        cancellation: ToolCancellation,
    ) -> Self {
        Self {
            operation,
            conversation_id,
            workspace,
            authority,
            deadline_at_ms,
            result_bytes,
            read_only_workspace: false,
            output_directories: Vec::new(),
            cancellation,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn cancellation(&self) -> ToolCancellation {
        self.cancellation.clone()
    }
}

pub type ExecutionFuture =
    Pin<Box<dyn Future<Output = Result<agl_core::execution::ExecutionResult, ToolFailure>> + Send>>;

/// The daemon supplies this callable after admission. No socket client or
/// process lifecycle implementation belongs to the native Tool SDK.
#[derive(Clone)]
pub struct ExecutionHost(
    Arc<dyn Fn(ToolContext, agl_core::execution::ExecutionAction) -> ExecutionFuture + Send + Sync>,
);

impl ExecutionHost {
    pub fn new<F>(call: F) -> Self
    where
        F: Fn(ToolContext, agl_core::execution::ExecutionAction) -> ExecutionFuture
            + Send
            + Sync
            + 'static,
    {
        Self(Arc::new(call))
    }

    pub fn call(
        &self,
        context: ToolContext,
        action: agl_core::execution::ExecutionAction,
    ) -> ExecutionFuture {
        (self.0)(context, action)
    }
}

pub trait ToolHandler: Send + Sync {
    fn call(&self, context: ToolContext, input: serde_json::Value) -> ToolFuture;
}

#[derive(Clone)]
pub struct ToolBinding {
    pub tool_id: ToolId,
    pub definition_digest: ToolDefinitionDigest,
    pub handler: Arc<dyn ToolHandler>,
}

#[derive(Clone)]
pub struct ExtensionBindings {
    pub version: PackageVersion,
    pub content_digest: PackageTreeDigest,
    pub definition: ExtensionDefinition,
    pub tools: Vec<ToolBinding>,
    pub allows_authority: Arc<dyn Fn(&AuthorityGrant) -> bool + Send + Sync>,
}

/// One bounded `git` invocation requested by a first-party Tool handler.
///
/// The host runs `git` with exactly `args`, `cwd`, an otherwise empty
/// environment containing only `env`, and `stdin`. It enforces its own memory
/// bound for the whole cgroup. Cancellation or the earlier of `timeout` and
/// the absolute Tool deadline starts SIGTERM, then SIGKILL after two seconds.
/// Each stream keeps the host-configured number of leading bytes; excess
/// output is drained without stopping the process.
#[derive(Clone, Debug)]
pub struct ServiceGitRequest {
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    pub deadline_at_ms: i64,
    pub cancellation: ToolCancellation,
}

/// Completion state of a started `git` process. `exit_code` is `None` when the
/// process was killed by a signal (including the host deadline kill) or could
/// not be waited for; `timed_out` is true when the host killed it at the
/// deadline.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceGitOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// `git` was provably never started, so the request had no effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceGitNotStarted;

/// Host callback that runs one blocking `git` invocation.
pub type ServiceGit =
    Arc<dyn Fn(ServiceGitRequest) -> Result<ServiceGitOutput, ServiceGitNotStarted> + Send + Sync>;
