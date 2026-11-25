//! agentLIBRE core (AGL-067, D2; AGL-131): the agent's library and nothing
//! above it: the agent, the Functions, the backends, the forge's and the
//! memory's use, the engine's life and the plane a Function runs on. It
//! knows no socket, no daemon and no surface: the daemon (`agentlibre-daemon`)
//! holds its runs and serves them in the protocol, and a surface is a client
//! of that.

pub mod about;
pub mod action;
pub mod agent;
pub mod attach;
pub mod backend;
pub mod download;
pub mod engine;
pub mod error;

pub mod format;
pub mod function;
pub mod grammar;
pub mod i18n;
pub mod machine;
pub mod media;
pub mod phase;
pub mod plane;
pub mod search;
pub mod session;
pub mod stop;
pub mod strata;
#[cfg(test)]
mod test_server;
pub mod tools;
pub mod weights;
pub mod whisper;
pub mod wsl;
