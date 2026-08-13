use std::path::PathBuf;

use crate::{
    ExecutionExit, ExecutionId, ExecutionIo, ExecutionOwner, ExecutionProfile, ExecutionState,
    OpaqueOwnerId, TerminalSize,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStatus {
    pub execution_id: ExecutionId,
    pub owner: ExecutionOwner,
    pub state: ExecutionState,
    pub profile: ExecutionProfile,
    pub io: ExecutionIo,
    pub cwd: PathBuf,
    pub terminal_size: Option<TerminalSize>,
    pub exit: Option<ExecutionExit>,
    pub first_retained_sequence: Option<u64>,
    pub last_sequence: u64,
    pub retained_bytes: u64,
    pub discarded_output_bytes: u64,
    pub output_truncated: bool,
    pub output_expired: bool,
    pub started_at_unix_ms: Option<i64>,
    pub finished_at_unix_ms: Option<i64>,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionListFilter {
    pub owner: Option<ExecutionOwner>,
    pub authority_scope: Option<OpaqueOwnerId>,
    pub include_finished: bool,
}
