//! Durable process and PTY service for agentLIBRE.

#[cfg(target_os = "linux")]
mod sandbox;
mod server;
mod store;
mod supervisor;

pub use server::{ExecutionServer, ListenerSource, default_socket_path};
pub use supervisor::ExecutionService;

pub const DEFAULT_SOCKET_FILE: &str = "execd.sock";
