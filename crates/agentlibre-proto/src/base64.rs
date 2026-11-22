//! The standard base64 with padding, both ways (AGL-076, D14): the media
//! of a message on the wire, by the `base64` crate (AGL-121, D34).

use ::base64::Engine as _;
use ::base64::engine::general_purpose::STANDARD;

pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// The bytes of `text`; `None` when it is not base64.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}
