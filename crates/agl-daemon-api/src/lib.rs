mod agent;
mod client;
mod progress;

pub use agent::*;
pub use client::{AgentClient, ClientError};
pub use progress::{AgentProgress, AgentProgressStatus};

/// Maximum size of one JSONL protocol frame.
///
/// Operation projections can include the admitted model context and complete
/// tool payloads, so the daemon protocol uses the same 8 MiB bound as the
/// execution protocol rather than truncating authoritative operation data.
pub const MAX_JSONL_FRAME_BYTES: usize = 8 * 1024 * 1024;
