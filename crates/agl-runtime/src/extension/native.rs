use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::package::{PackageTreeDigest, PackageVersion};
use agl_core::agent::{
    AgentOperationKey, AuthorityGrantSet, ToolDefinitionDigest, ToolFailure, ToolResult,
    WorkspaceScope,
};
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
            cancellation,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
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
