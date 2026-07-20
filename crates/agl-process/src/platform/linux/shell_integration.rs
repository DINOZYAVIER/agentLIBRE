use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use crate::{ProcessError, ProcessErrorCode, Result};

pub(crate) fn create_shell_integration_reader(path: &Path) -> Result<OwnedFd> {
    let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "private shell integration path contains NUL",
        )
    })?;
    let created = unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) };
    if created != 0 {
        return Err(shell_integration_io(
            "failed to create private shell integration FIFO",
            std::io::Error::last_os_error(),
        ));
    }

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(path);
            return Err(shell_integration_io(
                "failed to open private shell integration FIFO",
                error,
            ));
        }
    };
    let metadata = file.metadata().map_err(|error| {
        shell_integration_io("failed to inspect private shell integration FIFO", error)
    })?;
    if !metadata.file_type().is_fifo()
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "shell integration transport must be one owned private FIFO",
        ));
    }
    Ok(file.into())
}

pub(crate) fn shell_integration_path_is_intact(path: &Path, reader: &OwnedFd) -> bool {
    let Ok(path_metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !path_metadata.file_type().is_fifo() {
        return false;
    }
    let mut descriptor_metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(reader.as_raw_fd(), descriptor_metadata.as_mut_ptr()) } != 0 {
        return false;
    }
    let descriptor_metadata = unsafe { descriptor_metadata.assume_init() };
    path_metadata.dev() == descriptor_metadata.st_dev
        && path_metadata.ino() == descriptor_metadata.st_ino
}

pub(crate) fn interrupt_terminal_foreground(terminal: &OwnedFd) -> Result<()> {
    signal_terminal_foreground(terminal, libc::SIGINT, "interrupt")
}

pub(crate) fn notify_terminal_resize(terminal: &OwnedFd) -> Result<()> {
    signal_terminal_foreground(terminal, libc::SIGWINCH, "redraw")
}

fn signal_terminal_foreground(terminal: &OwnedFd, signal: libc::c_int, action: &str) -> Result<()> {
    let process_group = read_terminal_foreground_process_group(terminal)?;
    if unsafe { libc::kill(-process_group, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            format!("failed to {action} terminal foreground process group: {error}"),
        ));
    }
    Ok(())
}

pub(crate) fn terminal_foreground_process_group(
    terminal: &OwnedFd,
    shell_process_group: i32,
) -> Result<Option<i32>> {
    if shell_process_group <= 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "managed shell process group must be positive",
        ));
    }
    let foreground = read_terminal_foreground_process_group(terminal)?;
    Ok((foreground != shell_process_group).then_some(foreground))
}

fn read_terminal_foreground_process_group(terminal: &OwnedFd) -> Result<i32> {
    let process_group = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
    if process_group <= 0 {
        let error = std::io::Error::last_os_error();
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            format!("failed to observe terminal foreground process group: {error}"),
        ));
    }
    Ok(process_group)
}

fn shell_integration_io(context: &str, error: std::io::Error) -> ProcessError {
    ProcessError::new(ProcessErrorCode::Internal, format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;

    use agl_ids::ExecutionId;

    use super::*;

    #[test]
    fn private_fifo_reader_is_nonblocking_close_on_exec_and_rejects_replacement() {
        let root = std::env::temp_dir().join(format!(
            "agl-shell-integration-fifo-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let path = root.join("integration.fifo");

        let reader = create_shell_integration_reader(&path).unwrap();

        let descriptor_flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFD) };
        let status_flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(status_flags & libc::O_NONBLOCK, 0);
        assert!(shell_integration_path_is_intact(&path, &reader));
        assert_eq!(
            create_shell_integration_reader(&path).unwrap_err().code(),
            ProcessErrorCode::Internal
        );
        fs::remove_file(&path).unwrap();
        assert!(!shell_integration_path_is_intact(&path, &reader));
        drop(reader);
        fs::remove_dir_all(root).unwrap();
    }
}
