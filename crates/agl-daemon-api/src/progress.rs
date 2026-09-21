use agl_core::Content;
use agl_core::agent::{AgentOperationKey, AgentOperationTerminalStatus};
use serde::{Deserialize, Serialize};

/// Bounded process-local progress. Durable consumers resynchronize through
/// `AgentRunView` and `AgentEventPage`; this value is never persisted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentProgress {
    ModelOutputDelta {
        operation: AgentOperationKey,
        content: Content,
    },
    OperationStatus {
        operation: AgentOperationKey,
        status: AgentProgressStatus,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "terminal", rename_all = "snake_case")]
pub enum AgentProgressStatus {
    Running,
    Terminal(AgentOperationTerminalStatus),
}
