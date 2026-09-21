//! Typed client and wire contract for the agentLIBRE execution service.

mod client;
mod protocol;

pub use client::{BlockingExecutionClient, ExecutionClient, ExecutionClientError};
pub use protocol::*;
