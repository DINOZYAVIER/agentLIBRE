//! agentLIBRE core (AGL-067, D2): the agent, the Functions, the backends,
//! the forge and the memory, the engine's life and the plane a Function runs
//! on — apart from any terminal. A surface reads its input, draws what comes
//! out and takes its turns through a `Run`.

pub mod about;
pub mod action;
pub mod agent;
pub mod attach;
pub mod daemon;
pub mod backend;
pub mod client;
pub mod download;
pub mod engine;
pub mod error;
pub mod forge;
pub mod format;
pub mod function;
pub mod grammar;
pub mod i18n;
pub mod machine;
pub mod media;
pub mod memory;
pub mod phase;
pub mod plane;
pub mod record;
pub mod records;
pub mod search;
pub mod session;
pub mod status;
pub mod stop;
pub mod strata;
#[cfg(test)]
mod test_server;
pub mod tools;
pub mod weights;
pub mod whisper;
pub mod wsl;
