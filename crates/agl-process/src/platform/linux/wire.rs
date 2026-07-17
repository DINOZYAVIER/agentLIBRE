use std::mem::{self, MaybeUninit};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{ProcessError, ProcessErrorCode, Result};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub(super) fn socket_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(protocol_os_error(
            "failed to create launcher control socket",
        ));
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

pub(super) fn send_json_with_fds<T: Serialize>(
    fd: RawFd,
    value: &T,
    descriptors: &[RawFd],
) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            format!("failed to encode launcher response: {error}"),
        )
    })?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "launcher response exceeds the private protocol limit",
        ));
    }
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let control_len = if descriptors.is_empty() {
        0
    } else {
        unsafe { libc::CMSG_SPACE(mem::size_of_val(descriptors) as libc::c_uint) as usize }
    };
    let mut control = vec![0u8; control_len];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if !descriptors.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(mem::size_of_val(descriptors) as libc::c_uint) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                mem::size_of_val(descriptors),
            );
        }
    }
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    if sent < 0 || sent as usize != bytes.len() {
        return Err(protocol_os_error("failed to send launcher response"));
    }
    Ok(())
}

pub(super) fn receive_json_with_fds<T: DeserializeOwned>(
    fd: RawFd,
    maximum_descriptors: usize,
) -> Result<(T, Vec<OwnedFd>)> {
    let mut bytes = vec![0u8; MAX_MESSAGE_BYTES];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let descriptor_bytes = maximum_descriptors.saturating_mul(mem::size_of::<RawFd>());
    let control_len = unsafe { libc::CMSG_SPACE(descriptor_bytes as libc::c_uint) as usize };
    let mut control = vec![0u8; control_len];
    let mut message = MaybeUninit::<libc::msghdr>::zeroed();
    let message = unsafe { message.assume_init_mut() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received = unsafe { libc::recvmsg(fd, message, libc::MSG_CMSG_CLOEXEC) };
    if received <= 0 {
        return Err(protocol_os_error("failed to receive launcher response"));
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "launcher response or descriptor set was truncated",
        ));
    }
    bytes.truncate(received as usize);
    let value = serde_json::from_slice(&bytes).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            format!("failed to decode launcher response: {error}"),
        )
    })?;
    let mut descriptors = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let payload_bytes = (*header)
                    .cmsg_len
                    .saturating_sub(libc::CMSG_LEN(0) as usize);
                if !payload_bytes.is_multiple_of(mem::size_of::<RawFd>()) {
                    return Err(ProcessError::new(
                        ProcessErrorCode::LauncherProtocol,
                        "launcher descriptor payload is malformed",
                    ));
                }
                let count = payload_bytes / mem::size_of::<RawFd>();
                let raw = libc::CMSG_DATA(header).cast::<RawFd>();
                for index in 0..count {
                    descriptors.push(OwnedFd::from_raw_fd(*raw.add(index)));
                }
            }
            header = libc::CMSG_NXTHDR(message, header);
        }
    }
    if descriptors.len() > maximum_descriptors {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "launcher returned too many descriptors",
        ));
    }
    Ok((value, descriptors))
}

fn protocol_os_error(context: &str) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::LauncherProtocol,
        format!("{context}: {}", std::io::Error::last_os_error()),
    )
}
