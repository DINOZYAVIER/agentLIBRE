use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::io::{Read, Write};
use std::sync::atomic::{Ordering, compiler_fence};

use agl_exec::{ProcessError, ProcessErrorCode, Result};

pub const MAX_PRIVATE_ENVIRONMENT_ENTRIES: usize = 256;
pub const MAX_PRIVATE_ENVIRONMENT_NAME_BYTES: usize = 128;
pub const MAX_PRIVATE_ENVIRONMENT_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_PRIVATE_ENVIRONMENT_BYTES: usize = 64 * 1024;

const RESERVED_PREFIXES: &[&str] = &["AGL_SHELL_INTEGRATION_", "AGL_TERMINAL_"];
const RESERVED_NAMES: &[&str] = &["BASH_ENV", "ENV", "HISTFILE", "PROMPT_COMMAND", "ZDOTDIR"];
const PRIVATE_ENVIRONMENT_MAGIC: &[u8; 8] = b"AGLENV1\0";

/// A private environment value whose backing allocation is cleared on drop.
///
/// `Debug` never exposes the value. Callers may inspect it only at the exact
/// launcher/environment boundary and must not persist or log the returned
/// string.
pub struct PrivateEnvironmentValue(String);

impl PrivateEnvironmentValue {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let mut value = value.into();
        if value.len() > MAX_PRIVATE_ENVIRONMENT_VALUE_BYTES || value.contains('\0') {
            zeroize_string(&mut value);
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "private launch environment values must be bounded and contain no NUL bytes",
            ));
        }
        Ok(Self(value))
    }

    #[doc(hidden)]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Debug for PrivateEnvironmentValue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivateEnvironmentValue(<redacted>)")
    }
}

impl Drop for PrivateEnvironmentValue {
    fn drop(&mut self) {
        zeroize_string(&mut self.0);
    }
}

/// Exact private environment overlay transported to the native launcher.
///
/// Construction validates the same bounds accepted by the decoder, so an
/// in-process caller cannot create a payload that the launcher would reject.
#[derive(Default)]
pub struct PrivateLaunchEnvironment {
    values: BTreeMap<String, PrivateEnvironmentValue>,
}

impl PrivateLaunchEnvironment {
    pub fn new(values: BTreeMap<String, PrivateEnvironmentValue>) -> Result<Self> {
        validate_values(&values, ProcessErrorCode::InvalidRequest)?;
        Ok(Self { values })
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    #[doc(hidden)]
    pub fn exposed_values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.expose()))
    }

    #[doc(hidden)]
    pub fn write_launch_transport(&self, writer: &mut impl Write) -> Result<()> {
        writer
            .write_all(PRIVATE_ENVIRONMENT_MAGIC)
            .and_then(|()| writer.write_all(&(self.values.len() as u32).to_be_bytes()))
            .map_err(private_environment_io)?;
        for (name, value) in &self.values {
            let name_length =
                u16::try_from(name.len()).map_err(|_| private_environment_invalid())?;
            let value_length =
                u32::try_from(value.expose().len()).map_err(|_| private_environment_invalid())?;
            writer
                .write_all(&name_length.to_be_bytes())
                .and_then(|()| writer.write_all(&value_length.to_be_bytes()))
                .and_then(|()| writer.write_all(name.as_bytes()))
                .and_then(|()| writer.write_all(value.expose().as_bytes()))
                .map_err(private_environment_io)?;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn read_launch_transport(reader: &mut impl Read) -> Result<Self> {
        let mut magic = [0u8; PRIVATE_ENVIRONMENT_MAGIC.len()];
        read_exact(reader, &mut magic)?;
        if &magic != PRIVATE_ENVIRONMENT_MAGIC {
            return Err(private_environment_invalid());
        }

        let mut count = [0u8; 4];
        read_exact(reader, &mut count)?;
        let count = usize::try_from(u32::from_be_bytes(count))
            .map_err(|_| private_environment_invalid())?;
        if count > MAX_PRIVATE_ENVIRONMENT_ENTRIES {
            return Err(private_environment_invalid());
        }

        let mut values = BTreeMap::new();
        let mut total_bytes = 0usize;
        for _ in 0..count {
            let mut name_length = [0u8; 2];
            let mut value_length = [0u8; 4];
            read_exact(reader, &mut name_length)?;
            read_exact(reader, &mut value_length)?;
            let name_length = usize::from(u16::from_be_bytes(name_length));
            let value_length = usize::try_from(u32::from_be_bytes(value_length))
                .map_err(|_| private_environment_invalid())?;
            if name_length == 0
                || name_length > MAX_PRIVATE_ENVIRONMENT_NAME_BYTES
                || value_length > MAX_PRIVATE_ENVIRONMENT_VALUE_BYTES
            {
                return Err(private_environment_invalid());
            }
            total_bytes = total_bytes
                .checked_add(name_length)
                .and_then(|total| total.checked_add(value_length))
                .and_then(|total| total.checked_add(2))
                .ok_or_else(private_environment_invalid)?;
            if total_bytes > MAX_PRIVATE_ENVIRONMENT_BYTES {
                return Err(private_environment_invalid());
            }

            let mut name = vec![0u8; name_length];
            read_exact(reader, &mut name)?;
            let name = String::from_utf8(name).map_err(|_| private_environment_invalid())?;
            validate_name(&name, ProcessErrorCode::LauncherProtocol)?;

            let mut value = vec![0u8; value_length];
            if let Err(error) = read_exact(reader, &mut value) {
                zeroize_private_bytes(&mut value);
                return Err(error);
            }
            let value = match String::from_utf8(value) {
                Ok(value) => value,
                Err(error) => {
                    let mut value = error.into_bytes();
                    zeroize_private_bytes(&mut value);
                    return Err(private_environment_invalid());
                }
            };
            let value =
                PrivateEnvironmentValue::new(value).map_err(|_| private_environment_invalid())?;
            if values.insert(name, value).is_some() {
                return Err(private_environment_invalid());
            }
        }

        let mut trailing = [0u8; 1];
        loop {
            match reader.read(&mut trailing) {
                Ok(0) => return Ok(Self { values }),
                Ok(_) => return Err(private_environment_invalid()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(private_environment_invalid()),
            }
        }
    }

    #[cfg(test)]
    fn value_for_test(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(PrivateEnvironmentValue::expose)
    }
}

impl Debug for PrivateLaunchEnvironment {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateLaunchEnvironment")
            .field("names", &self.values.keys())
            .finish_non_exhaustive()
    }
}

fn validate_values(
    values: &BTreeMap<String, PrivateEnvironmentValue>,
    code: ProcessErrorCode,
) -> Result<()> {
    if values.len() > MAX_PRIVATE_ENVIRONMENT_ENTRIES {
        return Err(private_environment_error(code));
    }
    let mut total_bytes = 0usize;
    for (name, value) in values {
        validate_name(name, code)?;
        total_bytes = total_bytes
            .checked_add(name.len())
            .and_then(|total| total.checked_add(value.expose().len()))
            .and_then(|total| total.checked_add(2))
            .ok_or_else(|| private_environment_error(code))?;
        if total_bytes > MAX_PRIVATE_ENVIRONMENT_BYTES {
            return Err(private_environment_error(code));
        }
    }
    Ok(())
}

fn validate_name(name: &str, code: ProcessErrorCode) -> Result<()> {
    if name.len() > MAX_PRIVATE_ENVIRONMENT_NAME_BYTES
        || !is_posix_name(name)
        || RESERVED_NAMES.contains(&name)
        || RESERVED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    {
        return Err(private_environment_error(code));
    }
    Ok(())
}

fn is_posix_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn read_exact(reader: &mut impl Read, bytes: &mut [u8]) -> Result<()> {
    reader.read_exact(bytes).map_err(private_environment_io)
}

fn private_environment_io(_error: std::io::Error) -> ProcessError {
    private_environment_invalid()
}

fn private_environment_invalid() -> ProcessError {
    private_environment_error(ProcessErrorCode::LauncherProtocol)
}

fn private_environment_error(code: ProcessErrorCode) -> ProcessError {
    ProcessError::new(code, "private terminal environment transport is invalid")
}

fn zeroize_string(value: &mut String) {
    // SAFETY: zero is valid UTF-8, the string is exclusively borrowed, and
    // the allocation remains valid for its normal destructor.
    zeroize_private_bytes(unsafe { value.as_mut_vec() });
}

#[doc(hidden)]
pub fn zeroize_private_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment() -> PrivateLaunchEnvironment {
        PrivateLaunchEnvironment::new(BTreeMap::from([(
            "TOKEN".to_owned(),
            PrivateEnvironmentValue::new("do-not-log-me").unwrap(),
        )]))
        .unwrap()
    }

    #[test]
    fn round_trip_is_exact_and_debug_is_redacted() {
        let environment = environment();
        assert!(!format!("{environment:?}").contains("do-not-log-me"));
        let mut encoded = Vec::new();
        environment.write_launch_transport(&mut encoded).unwrap();
        let decoded =
            PrivateLaunchEnvironment::read_launch_transport(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.value_for_test("TOKEN"), Some("do-not-log-me"));
    }

    #[test]
    fn decoder_rejects_trailing_duplicate_and_reserved_names() {
        let mut trailing = Vec::new();
        environment().write_launch_transport(&mut trailing).unwrap();
        trailing.push(0);
        assert_eq!(
            PrivateLaunchEnvironment::read_launch_transport(&mut trailing.as_slice())
                .unwrap_err()
                .code(),
            ProcessErrorCode::LauncherProtocol
        );

        let mut duplicate = Vec::from(*PRIVATE_ENVIRONMENT_MAGIC);
        duplicate.extend_from_slice(&2u32.to_be_bytes());
        for _ in 0..2 {
            duplicate.extend_from_slice(&5u16.to_be_bytes());
            duplicate.extend_from_slice(&1u32.to_be_bytes());
            duplicate.extend_from_slice(b"TOKEN");
            duplicate.extend_from_slice(b"x");
        }
        assert!(
            PrivateLaunchEnvironment::read_launch_transport(&mut duplicate.as_slice()).is_err()
        );

        assert!(
            PrivateLaunchEnvironment::new(BTreeMap::from([(
                "HISTFILE".to_owned(),
                PrivateEnvironmentValue::new("x").unwrap(),
            )]))
            .is_err()
        );
    }
}
