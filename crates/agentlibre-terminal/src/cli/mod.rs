//! The command-line surfaces: arguments, the server command, the forge
//! store (AGL-007) and
//! `init` (AGL-010), the Functions (AGL-036), the lines on stdout
//! (AGL-038), the search's login (AGL-043 F20), the memory (AGL-060),
//! the help (AGL-061), the desktop's notifications (AGL-074), the planes
//! (AGL-077), the bridge of the iPhone and the watch (AGL-083), the sending
//! into a session (AGL-125).

pub mod args;
pub mod bridge;
pub mod forge;
pub mod function;
pub mod help;
pub mod init;
pub mod memory;
pub mod notify;
pub mod plane;
pub mod search;
pub mod send;
pub mod serve;
pub mod session;
