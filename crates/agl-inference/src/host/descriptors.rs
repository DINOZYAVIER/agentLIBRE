use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use agl_model::{ModelArtifactRole, ModelExecutionPlan};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactFileHandle {
    pub role: ModelArtifactRole,
    pub basename: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct VerifiedArtifactFile {
    pub role: ModelArtifactRole,
    pub basename: String,
    pub file: File,
}

#[derive(Debug)]
pub(crate) struct VerifiedDescriptorSet {
    pub files: Vec<VerifiedArtifactFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DescriptorSetError {
    #[error("artifact handle set does not match the execution plan: {reason}")]
    SetMismatch { reason: String },
    #[error("artifact descriptor changed while verifying `{basename}`")]
    Changed { basename: String },
    #[error("artifact descriptor `{basename}` is invalid: {reason}")]
    Invalid { basename: String, reason: String },
}

pub(crate) fn open_verified(
    plan: &ModelExecutionPlan,
    handles: &[ArtifactFileHandle],
) -> Result<VerifiedDescriptorSet, DescriptorSetError> {
    let expected = plan
        .artifact_roles()
        .iter()
        .flat_map(|artifact| {
            artifact.files().iter().map(move |file| {
                (
                    artifact.role(),
                    file.basename(),
                    file.byte_size(),
                    file.sha256(),
                )
            })
        })
        .collect::<Vec<_>>();
    if expected.len() != handles.len() {
        return Err(DescriptorSetError::SetMismatch {
            reason: format!(
                "expected {} ordered files, received {}",
                expected.len(),
                handles.len()
            ),
        });
    }
    let mut verified = Vec::with_capacity(handles.len());
    for ((role, basename, byte_size, sha256), handle) in expected.into_iter().zip(handles) {
        if handle.role != role || handle.basename != basename {
            return Err(DescriptorSetError::SetMismatch {
                reason: format!(
                    "expected {role:?}/{basename}, received {:?}/{}",
                    handle.role, handle.basename
                ),
            });
        }
        verified.push(verify_one(handle, byte_size, sha256)?);
    }
    Ok(VerifiedDescriptorSet { files: verified })
}

fn verify_one(
    handle: &ArtifactFileHandle,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<VerifiedArtifactFile, DescriptorSetError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&handle.path)
        .map_err(|error| invalid(handle, error))?;
    let before = file.metadata().map_err(|error| invalid(handle, error))?;
    if !before.is_file() || before.len() != expected_size {
        return Err(DescriptorSetError::Invalid {
            basename: handle.basename.clone(),
            reason: "not a regular file of the planned size".to_owned(),
        });
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| invalid(handle, error))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let after = file.metadata().map_err(|error| invalid(handle, error))?;
    if !same_file(&before, &after) {
        return Err(DescriptorSetError::Changed {
            basename: handle.basename.clone(),
        });
    }
    let actual = lower_hex(&digest.finalize());
    if actual != expected_sha256 {
        return Err(DescriptorSetError::Invalid {
            basename: handle.basename.clone(),
            reason: "SHA-256 does not match the execution plan".to_owned(),
        });
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| invalid(handle, error))?;
    Ok(VerifiedArtifactFile {
        role: handle.role,
        basename: handle.basename.clone(),
        file,
    })
}

#[cfg(unix)]
fn same_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
}

#[cfg(not(unix))]
fn same_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.len() == after.len() && before.modified().ok() == after.modified().ok()
}

fn invalid(handle: &ArtifactFileHandle, error: std::io::Error) -> DescriptorSetError {
    DescriptorSetError::Invalid {
        basename: handle.basename.clone(),
        reason: error.to_string(),
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}
