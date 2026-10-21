//! The command-line surfaces: arguments, the stdin line protocol, `-p`
//! progress output, the server command, the forge store (AGL-007) and
//! `init` (AGL-010).

pub mod args;
pub mod forge;
pub mod init;
pub mod lines;
pub mod report;
pub mod serve;
pub mod whisper;
