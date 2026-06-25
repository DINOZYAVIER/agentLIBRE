use std::path::Path;

use crate::hash::sha256_file;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptHookTrust {
    TrustedHash { sha256: String },
    Unsupported,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ScriptHookTrustError {
    Unsupported,
    HashReadFailed { message: String },
    HashMismatch { expected: String, actual: String },
}

impl ScriptHookTrustError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "script_hook.untrusted",
            Self::HashReadFailed { .. } => "script_hook.hash_read_failed",
            Self::HashMismatch { .. } => "script_hook.hash_mismatch",
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Unsupported => "script hook is not trusted for execution",
            Self::HashReadFailed { .. } => "script hook hash could not be verified",
            Self::HashMismatch { .. } => "script hook hash does not match trusted value",
        }
    }
}

impl std::fmt::Display for ScriptHookTrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "script hook trust state is unsupported"),
            Self::HashReadFailed { message } => write!(f, "{message}"),
            Self::HashMismatch { expected, actual } => {
                write!(f, "expected sha256 {expected}, got {actual}")
            }
        }
    }
}

pub(crate) fn verify_trust(
    command: &Path,
    trust: &ScriptHookTrust,
) -> std::result::Result<(), ScriptHookTrustError> {
    match trust {
        ScriptHookTrust::Unsupported => Err(ScriptHookTrustError::Unsupported),
        ScriptHookTrust::TrustedHash { sha256 } => {
            let actual =
                sha256_file(command).map_err(|err| ScriptHookTrustError::HashReadFailed {
                    message: err.to_string(),
                })?;
            if actual == *sha256 {
                Ok(())
            } else {
                Err(ScriptHookTrustError::HashMismatch {
                    expected: sha256.clone(),
                    actual,
                })
            }
        }
    }
}
