use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::{ProcessError, ProcessErrorCode, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessBytesEncoding {
    Utf8,
    Base64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessBytes {
    pub encoding: ProcessBytesEncoding,
    pub data: String,
}

impl ProcessBytes {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self {
                encoding: ProcessBytesEncoding::Utf8,
                data: text.to_owned(),
            },
            Err(_) => Self {
                encoding: ProcessBytesEncoding::Base64,
                data: STANDARD.encode(bytes),
            },
        }
    }

    pub fn decode(&self, maximum_bytes: usize) -> Result<Vec<u8>> {
        let bytes = match self.encoding {
            ProcessBytesEncoding::Utf8 => self.data.as_bytes().to_vec(),
            ProcessBytesEncoding::Base64 => STANDARD.decode(&self.data).map_err(|_| {
                ProcessError::new(
                    ProcessErrorCode::InvalidBytes,
                    "process bytes contain invalid base64",
                )
            })?,
        };
        if bytes.len() > maximum_bytes {
            return Err(ProcessError::new(
                ProcessErrorCode::InputTooLarge,
                format!("process bytes exceed the {maximum_bytes}-byte limit"),
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_remains_text_and_binary_uses_base64() {
        let text = ProcessBytes::from_bytes(b"hello\n");
        assert_eq!(text.encoding, ProcessBytesEncoding::Utf8);
        assert_eq!(text.decode(6).unwrap(), b"hello\n");

        let binary = ProcessBytes::from_bytes(&[0xff, 0x00, 0x80]);
        assert_eq!(binary.encoding, ProcessBytesEncoding::Base64);
        assert_eq!(binary.decode(3).unwrap(), [0xff, 0x00, 0x80]);
    }

    #[test]
    fn decode_rejects_invalid_or_oversized_data() {
        let invalid = ProcessBytes {
            encoding: ProcessBytesEncoding::Base64,
            data: "***".to_owned(),
        };
        assert_eq!(
            invalid.decode(10).unwrap_err().code(),
            ProcessErrorCode::InvalidBytes
        );

        assert_eq!(
            ProcessBytes::from_bytes(b"large")
                .decode(4)
                .unwrap_err()
                .code(),
            ProcessErrorCode::InputTooLarge
        );
    }
}
