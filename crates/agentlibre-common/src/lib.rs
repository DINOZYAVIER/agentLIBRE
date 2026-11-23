//! agentLIBRE's base crate, under the core (AGL-121, D77, D78, D79).

pub mod clock;
pub mod i18n;
pub mod programs;
pub mod style;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
pub mod tty;
