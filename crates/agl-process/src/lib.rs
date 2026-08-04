//! Staged agentLIBRE composition facade.
//!
//! Runtime ownership lives in `agl-terminald`; Step 04 removes this facade
//! when application consumers switch to `agl-terminal-client`.

pub use agl_terminald::*;
