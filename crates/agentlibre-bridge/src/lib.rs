//! agentLIBRE's bridge (AGL-083): the daemon's protocol over a WebSocket,
//! for the iPhone and, through it, the watch (D3, D4).
//!
//! It is a client of the daemon like any other (D8): one connection to
//! `agl.sock` for each WebSocket, the protocol as it is, one JSON message a
//! text frame (D9). A client comes through WireGuard and carries a token of
//! its device (D6, D10), over TLS with a certificate the bridge makes itself
//! and the app pins (D11). Only the requests of the allow-list go on; any
//! other closes the connection with 1008 (D21).
//!
//! It runs as `agl-bridge`, with no core and no nix (D18): everything it
//! needs comes as arguments, and it never starts the daemon.

pub mod allow;
pub mod serve;
pub mod store;

pub use allow::allowed;
pub use serve::{Config, serve};
pub use store::{Certificate, Store, StoreError, pairing_json};
