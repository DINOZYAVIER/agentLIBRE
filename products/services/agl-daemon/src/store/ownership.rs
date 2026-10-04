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

impl Drop for StoreOwnership {
    fn drop(&mut self) {
        // A concurrent fork may hold this open description until exec closes
        // its inherited CLOEXEC descriptor. Release when the final Store owner
        // drops, rather than leaving exclusivity attached to that child.
        // SAFETY: this object still owns the live lock descriptor.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_owner_releases_lock_even_while_a_fork_holds_the_descriptor() {
        let root =
            std::env::temp_dir().join(format!("agl-store-fork-lock-{}", uuid::Uuid::now_v7()));
        let ownership = StoreOwnership::acquire(&root).unwrap();
        let mut pipe = [0; 2];
        // SAFETY: pipe points to two writable descriptors; child uses only
        // async-signal-safe libc calls and exits without running Rust destructors.
        let pid = unsafe {
            assert_eq!(libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC), 0);
            let pid = libc::fork();
            if pid == 0 {
                libc::close(pipe[1]);
                let mut byte = 0u8;
                libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
            pid
        };
        assert!(pid > 0);
        drop(ownership);
        let reopened = StoreOwnership::acquire(&root);
        // SAFETY: parent owns both pipe descriptors and waits for its child.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert!(reopened.is_ok(), "{reopened:?}");
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}
