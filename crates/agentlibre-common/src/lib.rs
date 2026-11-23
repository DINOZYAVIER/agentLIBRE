//! agentLIBRE's base crate, under the core (AGL-121, D77, D78, D79).

pub mod clock;
pub mod error;
pub mod file;
pub mod i18n;
pub mod nix;
pub mod paths;
pub mod programs;
pub mod settings;
pub mod style;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
pub mod tty;
