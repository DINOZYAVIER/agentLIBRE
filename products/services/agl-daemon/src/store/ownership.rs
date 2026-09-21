use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::path::{ensure_private_dir, validate_private_regular_file};
use super::{Result, StoreError};

/// An independent open file description makes exclusivity process-wide too.
/// The lock file is never unlinked: doing so would allow two lock identities.
#[derive(Debug)]
pub(super) struct StoreOwnership {
    _file: File,
}

impl StoreOwnership {
    pub(super) fn acquire(root: &Path) -> Result<Self> {
        ensure_private_dir(root)?;
        let path = root.join("store.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        validate_private_regular_file(&path)?;
        // SAFETY: file owns a live descriptor and flock does not retain pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(StoreError::Busy {
                    path: root.to_path_buf(),
                });
            }
            return Err(error.into());
        }
        Ok(Self { _file: file })
    }
}
