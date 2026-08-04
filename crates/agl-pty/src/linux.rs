use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::time::{Duration, Instant};

use agl_exec::{ProcessError, ProcessErrorCode, Result};

const EVENT_PACKET: u8 = b'E';
const CONTROL_PACKET: u8 = b'C';
const CONTROL_DELIVERED_PACKET: &[u8] = b"A";
const RELAY_POLL_MS: libc::c_int = 50;

pub struct ShellIntegrationSocketPair {
    pub supervisor: OwnedFd,
    pub relay: OwnedFd,
    pub event_guard: OwnedFd,
}

pub enum ShellIntegrationReceive {
    Empty,
    Event(Vec<u8>),
    Closed,
}

pub fn create_shell_integration_transport(
    event_path: &Path,
    control_path: &Path,
) -> Result<ShellIntegrationSocketPair> {
    create_private_fifo(event_path)?;
    if let Err(error) = create_private_fifo(control_path) {
        let _ = fs::remove_file(event_path);
        return Err(error);
    }
    let event_guard = match open_private_fifo(event_path, "event") {
        Ok(guard) => guard,
        Err(error) => {
            let _ = fs::remove_file(event_path);
            let _ = fs::remove_file(control_path);
            return Err(error);
        }
    };
    let pipe_size = unsafe { libc::fcntl(event_guard.as_raw_fd(), libc::F_SETPIPE_SZ, 128 * 1024) };
    if pipe_size < 80 * 1024 {
        let error = ProcessError::new(
            ProcessErrorCode::Internal,
            "private shell integration event FIFO cannot fit one maximum frame",
        );
        drop(event_guard);
        let _ = fs::remove_file(event_path);
        let _ = fs::remove_file(control_path);
        return Err(error);
    }
    let mut descriptors = [-1; 2];
    let created = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            descriptors.as_mut_ptr(),
        )
    };
    if created != 0 {
        let error = shell_integration_io(
            "failed to create private shell integration socketpair",
            std::io::Error::last_os_error(),
        );
        let _ = fs::remove_file(event_path);
        let _ = fs::remove_file(control_path);
        return Err(error);
    }
    Ok(unsafe {
        ShellIntegrationSocketPair {
            supervisor: OwnedFd::from_raw_fd(descriptors[0]),
            relay: OwnedFd::from_raw_fd(descriptors[1]),
            event_guard,
        }
    })
}

pub fn receive_shell_integration_event(
    socket: &OwnedFd,
    maximum_frame_bytes: usize,
) -> Result<ShellIntegrationReceive> {
    let mut bytes = vec![0_u8; maximum_frame_bytes.saturating_add(2)];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut message = MaybeUninit::<libc::msghdr>::zeroed();
    let message = unsafe { message.assume_init_mut() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let received = unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            message,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if received == 0 {
        return Ok(ShellIntegrationReceive::Closed);
    }
    if received < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            return Ok(ShellIntegrationReceive::Empty);
        }
        return Err(shell_integration_io(
            "failed to receive private shell integration event",
            error,
        ));
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration event packet was truncated",
        ));
    }
    bytes.truncate(received as usize);
    if bytes.first() != Some(&EVENT_PACKET) || bytes.len() == 1 {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration relay returned an invalid packet",
        ));
    }
    bytes.remove(0);
    if bytes.len() > maximum_frame_bytes {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration event exceeded its frame bound",
        ));
    }
    Ok(ShellIntegrationReceive::Event(bytes))
}

/// Sends one typed control only after the launcher-owned relay has copied the
/// entire frame into the shell-private control FIFO. This delivery barrier is
/// what makes `ArmTypedCommand` observable before the PTY input transaction.
pub fn send_shell_integration_control(
    socket: &OwnedFd,
    frame: &[u8],
    timeout: Duration,
) -> Result<()> {
    let mut packet = Vec::with_capacity(frame.len().saturating_add(1));
    packet.push(CONTROL_PACKET);
    packet.extend_from_slice(frame);
    send_packet(socket.as_raw_fd(), &packet, timeout)?;

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration control delivery timed out",
            ));
        }
        wait_for(socket.as_raw_fd(), libc::POLLIN, remaining)?;
        let mut acknowledgement = [0_u8; 2];
        let received = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                acknowledgement.as_mut_ptr().cast(),
                acknowledgement.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if received < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(shell_integration_io(
                "failed to receive private shell integration delivery acknowledgement",
                error,
            ));
        }
        if received == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration relay closed during control delivery",
            ));
        }
        if &acknowledgement[..received as usize] != CONTROL_DELIVERED_PACKET {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration relay interleaved an unexpected packet",
            ));
        }
        return Ok(());
    }
}

pub fn interrupt_terminal_foreground(terminal: &OwnedFd) -> Result<()> {
    signal_terminal_foreground(terminal, libc::SIGINT, "interrupt")
}

pub fn notify_terminal_resize(terminal: &OwnedFd) -> Result<()> {
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

pub fn terminal_foreground_process_group(
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

/// Runs inside the launcher's PID namespace as a sibling of the managed shell.
/// The relay is the only process that owns the shell-side SOCK_SEQPACKET end;
/// the shell and every ordinary exec child inherit no integration descriptor.
pub fn run_shell_integration_relay(
    socket: OwnedFd,
    terminal_slave: RawFd,
    event_path: &Path,
    control_path: &Path,
    maximum_frame_bytes: usize,
) -> i32 {
    match relay_loop(
        socket,
        terminal_slave,
        event_path,
        control_path,
        maximum_frame_bytes,
    ) {
        Ok(()) => 0,
        Err(_) => 125,
    }
}

fn relay_loop(
    socket: OwnedFd,
    terminal_slave: RawFd,
    event_path: &Path,
    control_path: &Path,
    maximum_frame_bytes: usize,
) -> Result<()> {
    let event = open_private_fifo(event_path, "event")?;
    let control = open_private_fifo(control_path, "control")?;
    let mut event_buffer = Vec::with_capacity(maximum_frame_bytes.min(16 * 1024));
    loop {
        if !fifo_path_matches(event_path, &event) || !fifo_path_matches(control_path, &control) {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration FIFO was replaced",
            ));
        }
        let mut polls = [
            libc::pollfd {
                fd: event.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, RELAY_POLL_MS) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(shell_integration_io(
                "failed to poll private shell integration relay",
                error,
            ));
        }
        if polls[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(());
        }
        if polls[1].revents & libc::POLLIN != 0 {
            relay_control(&socket, &control, maximum_frame_bytes)?;
        }
        if polls[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration event FIFO failed",
            ));
        }
        if polls[0].revents & libc::POLLIN != 0 {
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                let read = unsafe {
                    libc::read(event.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len())
                };
                if read > 0 {
                    event_buffer.extend_from_slice(&chunk[..read as usize]);
                    if event_buffer.len() > maximum_frame_bytes {
                        return Err(ProcessError::new(
                            ProcessErrorCode::StateConflict,
                            "private shell integration event exceeded its frame bound",
                        ));
                    }
                    while let Some(frame_len) = complete_event_frame_len(&event_buffer)? {
                        let mut frame = event_buffer.drain(..frame_len).collect::<Vec<_>>();
                        install_prompt_input_probe(&mut frame, terminal_slave)?;
                        let mut packet = Vec::with_capacity(frame.len().saturating_add(1));
                        packet.push(EVENT_PACKET);
                        packet.extend_from_slice(&frame);
                        send_packet(socket.as_raw_fd(), &packet, Duration::from_secs(1))?;
                    }
                    continue;
                }
                if read == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    break;
                }
                return Err(shell_integration_io(
                    "failed to read private shell integration event FIFO",
                    error,
                ));
            }
        }
    }
}

fn relay_control(socket: &OwnedFd, control: &OwnedFd, maximum_frame_bytes: usize) -> Result<()> {
    let mut packet = vec![0_u8; maximum_frame_bytes.saturating_add(2)];
    let received = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            packet.as_mut_ptr().cast(),
            packet.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if received == 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration supervisor endpoint closed",
        ));
    }
    if received < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            return Ok(());
        }
        return Err(shell_integration_io(
            "failed to receive private shell integration control",
            error,
        ));
    }
    packet.truncate(received as usize);
    if packet.first() != Some(&CONTROL_PACKET)
        || packet.len() <= 1
        || packet.len() - 1 > maximum_frame_bytes
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration control packet was invalid",
        ));
    }
    write_all_bounded(control.as_raw_fd(), &packet[1..], Duration::from_secs(1))?;
    send_packet(
        socket.as_raw_fd(),
        CONTROL_DELIVERED_PACKET,
        Duration::from_secs(1),
    )
}

fn complete_event_frame_len(bytes: &[u8]) -> Result<Option<usize>> {
    let mut fields = 0_usize;
    let mut kind = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != 0 {
            continue;
        }
        fields += 1;
        if fields == 4 {
            let mut starts = bytes.split(|byte| *byte == 0);
            let event_kind = starts.nth(3).unwrap_or_default();
            kind = Some(match event_kind {
                b"prompt_ready" => 7,
                b"command_started" => 7,
                b"command_finished" => 8,
                _ => {
                    return Err(ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "private shell integration event kind is unsupported",
                    ));
                }
            });
        }
        if kind == Some(fields) {
            return Ok(Some(index + 1));
        }
    }
    Ok(None)
}

fn install_prompt_input_probe(frame: &mut Vec<u8>, terminal_slave: RawFd) -> Result<()> {
    let fields = frame.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.get(3).copied() != Some(b"prompt_ready") {
        return Ok(());
    }
    let mut pending = 0_i32;
    if unsafe { libc::ioctl(terminal_slave, libc::FIONREAD, &mut pending) } != 0 {
        return Err(shell_integration_io(
            "failed to probe pending managed-shell input",
            std::io::Error::last_os_error(),
        ));
    }
    let last_separator = frame.len().checked_sub(1).ok_or_else(|| {
        ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration prompt frame was empty",
        )
    })?;
    let field_start = frame[..last_separator]
        .iter()
        .rposition(|byte| *byte == 0)
        .map_or(0, |index| index + 1);
    let replacement = if pending == 0 { b"0" } else { b"1" };
    frame.splice(field_start..last_separator, replacement.iter().copied());
    Ok(())
}

fn create_private_fifo(path: &Path) -> Result<()> {
    let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "private shell integration path contains NUL",
        )
    })?;
    if unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) } != 0 {
        return Err(shell_integration_io(
            "failed to create private shell integration FIFO",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn open_private_fifo(path: &Path, label: &str) -> Result<OwnedFd> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            shell_integration_io(
                &format!("failed to open private shell integration {label} FIFO"),
                error,
            )
        })?;
    let file: OwnedFd = file.into();
    validate_private_fifo(&file, label)?;
    Ok(file)
}

fn validate_private_fifo(file: &OwnedFd, label: &str) -> Result<()> {
    let mut metadata = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(shell_integration_io(
            &format!("failed to inspect private shell integration {label} FIFO"),
            std::io::Error::last_os_error(),
        ));
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO
        || metadata.st_mode & 0o777 != 0o600
        || metadata.st_uid != unsafe { libc::geteuid() }
        || metadata.st_nlink != 1
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("shell integration {label} transport must be one owned private FIFO"),
        ));
    }
    Ok(())
}

fn fifo_path_matches(path: &Path, descriptor: &OwnedFd) -> bool {
    let Ok(path_metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !path_metadata.file_type().is_fifo() {
        return false;
    }
    let mut descriptor_metadata = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(descriptor.as_raw_fd(), descriptor_metadata.as_mut_ptr()) } != 0 {
        return false;
    }
    let descriptor_metadata = unsafe { descriptor_metadata.assume_init() };
    path_metadata.dev() == descriptor_metadata.st_dev
        && path_metadata.ino() == descriptor_metadata.st_ino
}

fn send_packet(fd: RawFd, bytes: &[u8], timeout: Duration) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        let sent = unsafe {
            libc::send(
                fd,
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if sent == bytes.len() as isize {
            return Ok(());
        }
        if sent >= 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration packet was partially sent",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(shell_integration_io(
                "failed to send private shell integration packet",
                error,
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration packet send timed out",
            ));
        }
        wait_for(fd, libc::POLLOUT, remaining)?;
    }
}

fn write_all_bounded(fd: RawFd, mut bytes: &[u8], timeout: Duration) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration FIFO write made no progress",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(shell_integration_io(
                "failed to write private shell integration control FIFO",
                error,
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "private shell integration FIFO write timed out",
            ));
        }
        wait_for(fd, libc::POLLOUT, remaining)?;
    }
    Ok(())
}

fn wait_for(fd: RawFd, events: libc::c_short, timeout: Duration) -> Result<()> {
    let timeout_ms = i32::try_from(timeout.as_millis())
        .unwrap_or(i32::MAX)
        .max(1);
    let mut poll = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
    if ready > 0 && poll.revents & events != 0 {
        return Ok(());
    }
    if ready == 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "private shell integration operation timed out",
        ));
    }
    if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
        return Ok(());
    }
    Err(shell_integration_io(
        "private shell integration descriptor failed",
        std::io::Error::last_os_error(),
    ))
}

fn shell_integration_io(context: &str, error: std::io::Error) -> ProcessError {
    ProcessError::new(ProcessErrorCode::Internal, format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use agl_exec::ExecutionId;

    use super::*;

    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-shell-integration-socket-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn private_transport_is_seqpacket_close_on_exec_and_rejects_fifo_replacement() {
        let root = root();
        let event = root.join("events.fifo");
        let control = root.join("controls.fifo");
        let pair = create_shell_integration_transport(&event, &control).unwrap();
        for descriptor in [&pair.supervisor, &pair.relay] {
            let kind = unsafe {
                let mut value = 0_i32;
                let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
                assert_eq!(
                    libc::getsockopt(
                        descriptor.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_TYPE,
                        (&mut value as *mut i32).cast(),
                        &mut length,
                    ),
                    0
                );
                value
            };
            assert_eq!(kind, libc::SOCK_SEQPACKET);
            assert_ne!(
                unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let opened = open_private_fifo(&event, "event").unwrap();
        assert!(fifo_path_matches(&event, &opened));
        fs::remove_file(&event).unwrap();
        assert!(!fifo_path_matches(&event, &opened));
        drop(opened);
        drop(pair);
        let _ = fs::remove_file(control);
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn relay_extracts_complete_nul_frames_and_installs_boolean_probe() {
        let mut bytes = b"AGL2\0token\x001\0prompt_ready\0/workspace\0-\0-\0tail".to_vec();
        let length = complete_event_frame_len(&bytes).unwrap().unwrap();
        assert_eq!(
            &bytes[..length],
            b"AGL2\0token\x001\0prompt_ready\0/workspace\0-\0-\0"
        );
        bytes.drain(..length);
        assert_eq!(bytes, b"tail");
    }
}
