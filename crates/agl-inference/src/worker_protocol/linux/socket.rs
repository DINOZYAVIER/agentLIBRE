use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::super::protocol::Frame;
use super::super::{
    HostCommand, MAX_CONTROL_DESCRIPTORS, MAX_CONTROL_FRAME_BYTES, Result, WORKER_FRAME_VERSION,
    WorkerEvent, WorkerProtocolError, WorkerProtocolErrorCode,
};
use super::{INHERITED_CONTROL_FD_ENV, INHERITED_PARENT_PID_ENV};

static INHERITED_CONTROL_FD_TAKEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default)]
pub struct DescriptorSet {
    descriptors: Vec<Option<OwnedFd>>,
}

impl DescriptorSet {
    pub(super) fn new(descriptors: Vec<OwnedFd>) -> Self {
        Self {
            descriptors: descriptors.into_iter().map(Some).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.descriptors
            .iter()
            .filter(|descriptor| descriptor.is_some())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn take(&mut self, index: usize) -> Result<OwnedFd> {
        self.descriptors
            .get_mut(index)
            .and_then(Option::take)
            .ok_or_else(|| {
                WorkerProtocolError::new(
                    WorkerProtocolErrorCode::InvalidPayload,
                    format!("sealed payload descriptor index {index} is absent or already used"),
                )
            })
    }

    pub fn ensure_empty(&self) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::UnexpectedDescriptors,
                format!(
                    "inference worker message left {} descriptors unclaimed",
                    self.len()
                ),
            ))
        }
    }
}

#[derive(Debug)]
pub struct ReceivedMessage<T> {
    message: T,
    descriptors: DescriptorSet,
}

impl<T> ReceivedMessage<T> {
    pub fn message(&self) -> &T {
        &self.message
    }

    pub fn descriptors(&self) -> &DescriptorSet {
        &self.descriptors
    }

    pub fn descriptors_mut(&mut self) -> &mut DescriptorSet {
        &mut self.descriptors
    }

    pub fn into_parts(self) -> (T, DescriptorSet) {
        (self.message, self.descriptors)
    }
}

#[derive(Debug)]
pub struct ReceivedPacket {
    bytes: Vec<u8>,
    descriptors: DescriptorSet,
}

impl ReceivedPacket {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn descriptors(&self) -> &DescriptorSet {
        &self.descriptors
    }

    pub fn descriptors_mut(&mut self) -> &mut DescriptorSet {
        &mut self.descriptors
    }

    pub fn into_parts(self) -> (Vec<u8>, DescriptorSet) {
        (self.bytes, self.descriptors)
    }
}

#[derive(Debug)]
pub struct PacketSocket {
    descriptor: OwnedFd,
}

impl PacketSocket {
    pub fn pair() -> Result<(Self, Self)> {
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
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::Io,
                "failed to create inference worker control socketpair",
            ));
        }
        Ok(unsafe {
            (
                Self::from_owned_fd(OwnedFd::from_raw_fd(descriptors[0]))?,
                Self::from_owned_fd(OwnedFd::from_raw_fd(descriptors[1]))?,
            )
        })
    }

    pub fn send_packet(&self, bytes: &[u8], descriptors: Vec<OwnedFd>) -> Result<()> {
        if bytes.is_empty() {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::MalformedFrame,
                "inference worker control frames must not be empty",
            ));
        }
        if bytes.len() > MAX_CONTROL_FRAME_BYTES {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::FrameTooLarge,
                format!(
                    "inference worker control frame is {} bytes; the limit is {MAX_CONTROL_FRAME_BYTES}",
                    bytes.len()
                ),
            ));
        }
        if descriptors.len() > MAX_CONTROL_DESCRIPTORS {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::DescriptorLimit,
                format!(
                    "inference worker control frame has {} descriptors; the limit is {MAX_CONTROL_DESCRIPTORS}",
                    descriptors.len()
                ),
            ));
        }

        let raw_descriptors = descriptors
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let descriptor_bytes = mem::size_of_val(raw_descriptors.as_slice());
        let control_len = if raw_descriptors.is_empty() {
            0
        } else {
            unsafe { libc::CMSG_SPACE(descriptor_bytes as libc::c_uint) as usize }
        };
        let mut control = vec![0u8; control_len];
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if !raw_descriptors.is_empty() {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = control.len();
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(descriptor_bytes as libc::c_uint) as _;
                std::ptr::copy_nonoverlapping(
                    raw_descriptors.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(header),
                    descriptor_bytes,
                );
            }
        }

        loop {
            let sent =
                unsafe { libc::sendmsg(self.descriptor.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
            if sent == bytes.len() as isize {
                return Ok(());
            }
            if sent < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if sent < 0 {
                return Err(WorkerProtocolError::last_os_error(
                    WorkerProtocolErrorCode::Io,
                    "failed to send inference worker control frame",
                ));
            }
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::Io,
                "inference worker SOCK_SEQPACKET send was unexpectedly partial",
            ));
        }
    }

    pub fn receive_packet(&self) -> Result<ReceivedPacket> {
        loop {
            let mut bytes = vec![0u8; MAX_CONTROL_FRAME_BYTES];
            let mut iov = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            };
            let descriptor_bytes = MAX_CONTROL_DESCRIPTORS * mem::size_of::<RawFd>();
            let control_len =
                unsafe { libc::CMSG_SPACE(descriptor_bytes as libc::c_uint) as usize };
            let mut control = vec![0u8; control_len];
            let mut message = MaybeUninit::<libc::msghdr>::zeroed();
            let message = unsafe { message.assume_init_mut() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = control.len();

            let received = unsafe {
                libc::recvmsg(self.descriptor.as_raw_fd(), message, libc::MSG_CMSG_CLOEXEC)
            };
            if received < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if received < 0 {
                return Err(WorkerProtocolError::last_os_error(
                    WorkerProtocolErrorCode::Io,
                    "failed to receive inference worker control frame",
                ));
            }
            if received == 0 {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::PeerClosed,
                    "inference worker control peer closed the channel",
                ));
            }

            let (descriptors, ancillary_error) = unsafe { collect_descriptors(message) };
            if message.msg_flags & libc::MSG_TRUNC != 0 {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::FrameTooLarge,
                    "inference worker control frame exceeded the receive bound",
                ));
            }
            if message.msg_flags & libc::MSG_CTRUNC != 0 {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::DescriptorLimit,
                    "inference worker descriptor set exceeded the receive bound",
                ));
            }
            if let Some(error) = ancillary_error {
                return Err(error);
            }
            if descriptors.len() > MAX_CONTROL_DESCRIPTORS {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::DescriptorLimit,
                    "inference worker descriptor set exceeded the receive bound",
                ));
            }

            bytes.truncate(received as usize);
            return Ok(ReceivedPacket {
                bytes,
                descriptors: DescriptorSet::new(descriptors),
            });
        }
    }

    pub fn receive_packet_timeout(&self, timeout: Duration) -> Result<ReceivedPacket> {
        self.wait_readable(timeout)?;
        self.receive_packet()
    }

    pub fn wait_readable(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            let timeout_ms = if remaining.is_zero() {
                0
            } else {
                i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX)
            };
            let mut poll = libc::pollfd {
                fd: self.descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
            if ready > 0 {
                if poll.revents & libc::POLLNVAL != 0 {
                    return Err(WorkerProtocolError::new(
                        WorkerProtocolErrorCode::Io,
                        "inference worker control descriptor became invalid",
                    ));
                }
                if poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    return Ok(());
                }
            } else if ready == 0 {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::TimedOut,
                    "timed out waiting for an inference worker control frame",
                ));
            } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(WorkerProtocolError::last_os_error(
                    WorkerProtocolErrorCode::Io,
                    "failed to wait for an inference worker control frame",
                ));
            }
            if Instant::now() >= deadline {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::TimedOut,
                    "timed out waiting for an inference worker control frame",
                ));
            }
        }
    }

    fn try_clone(&self) -> Result<Self> {
        let descriptor = unsafe {
            libc::fcntl(
                self.descriptor.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                libc::STDERR_FILENO + 1,
            )
        };
        if descriptor < 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::Io,
                "failed to duplicate inference worker control descriptor",
            ));
        }
        Self::from_owned_fd(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    fn shutdown(&self) -> Result<()> {
        if unsafe { libc::shutdown(self.descriptor.as_raw_fd(), libc::SHUT_RDWR) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOTCONN) | Some(libc::EBADF)
        ) {
            return Ok(());
        }
        Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::Io,
            format!("failed to shut down inference worker control socket: {error}"),
        ))
    }

    pub(crate) fn from_owned_fd(descriptor: OwnedFd) -> Result<Self> {
        require_seqpacket(descriptor.as_raw_fd())?;
        Ok(Self { descriptor })
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }
}

unsafe fn collect_descriptors(
    message: &mut libc::msghdr,
) -> (Vec<OwnedFd>, Option<WorkerProtocolError>) {
    let mut descriptors = Vec::new();
    let mut error = None;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
    while !header.is_null() {
        let header_length = unsafe { (*header).cmsg_len } as usize;
        let minimum_length = unsafe { libc::CMSG_LEN(0) } as usize;
        if header_length < minimum_length {
            error = Some(WorkerProtocolError::new(
                WorkerProtocolErrorCode::MalformedFrame,
                "inference worker ancillary descriptor header is malformed",
            ));
            break;
        }
        if unsafe { (*header).cmsg_level } != libc::SOL_SOCKET
            || unsafe { (*header).cmsg_type } != libc::SCM_RIGHTS
        {
            error = Some(WorkerProtocolError::new(
                WorkerProtocolErrorCode::MalformedFrame,
                "inference worker control frame used an unknown ancillary message",
            ));
        } else {
            let payload_bytes = header_length - minimum_length;
            if !payload_bytes.is_multiple_of(mem::size_of::<RawFd>()) {
                error = Some(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::MalformedFrame,
                    "inference worker descriptor payload is malformed",
                ));
            } else {
                let count = payload_bytes / mem::size_of::<RawFd>();
                let raw = unsafe { libc::CMSG_DATA(header).cast::<RawFd>() };
                for index in 0..count {
                    let descriptor = unsafe { *raw.add(index) };
                    if descriptor < 0 {
                        error = Some(WorkerProtocolError::new(
                            WorkerProtocolErrorCode::MalformedFrame,
                            "inference worker descriptor payload contained an invalid descriptor",
                        ));
                    } else {
                        descriptors.push(unsafe { OwnedFd::from_raw_fd(descriptor) });
                    }
                }
            }
        }
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
    }
    (descriptors, error)
}

fn require_seqpacket(descriptor: RawFd) -> Result<()> {
    let mut socket_type: libc::c_int = 0;
    let mut length = mem::size_of::<libc::c_int>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(WorkerProtocolError::last_os_error(
            WorkerProtocolErrorCode::MalformedFrame,
            "inference worker control descriptor is not a socket",
        ));
    }
    if socket_type != libc::SOCK_SEQPACKET {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::MalformedFrame,
            "inference worker control descriptor is not SOCK_SEQPACKET",
        ));
    }

    let mut peer = MaybeUninit::<libc::sockaddr_un>::zeroed();
    let mut peer_length = mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    if unsafe { libc::getpeername(descriptor, peer.as_mut_ptr().cast(), &mut peer_length) } != 0 {
        return Err(WorkerProtocolError::last_os_error(
            WorkerProtocolErrorCode::MalformedFrame,
            "inference worker control socket is not connected",
        ));
    }
    let peer = unsafe { peer.assume_init() };
    if peer.sun_family != libc::AF_UNIX as libc::sa_family_t {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::MalformedFrame,
            "inference worker control socket is not AF_UNIX",
        ));
    }
    Ok(())
}

struct FramedChannel {
    socket: PacketSocket,
    next_send_sequence: u64,
    next_receive_sequence: u64,
}

impl FramedChannel {
    fn new(socket: PacketSocket) -> Self {
        Self {
            socket,
            next_send_sequence: 1,
            next_receive_sequence: 1,
        }
    }

    fn send<T: Serialize>(&mut self, message: T, descriptors: Vec<OwnedFd>) -> Result<()> {
        send_frame(
            &self.socket,
            &mut self.next_send_sequence,
            message,
            descriptors,
        )
    }

    fn receive<T: DeserializeOwned>(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<ReceivedMessage<T>> {
        receive_frame(&self.socket, &mut self.next_receive_sequence, timeout)
    }

    fn into_split(self) -> Result<(FramedSender, FramedReceiver)> {
        let sender_socket = self.socket.try_clone()?;
        Ok((
            FramedSender {
                socket: sender_socket,
                next_sequence: self.next_send_sequence,
            },
            FramedReceiver {
                socket: self.socket,
                next_sequence: self.next_receive_sequence,
            },
        ))
    }
}

struct FramedSender {
    socket: PacketSocket,
    next_sequence: u64,
}

impl FramedSender {
    fn send<T: Serialize>(&mut self, message: T, descriptors: Vec<OwnedFd>) -> Result<()> {
        send_frame(&self.socket, &mut self.next_sequence, message, descriptors)
    }

    fn shutdown(&self) -> Result<()> {
        self.socket.shutdown()
    }
}

struct FramedReceiver {
    socket: PacketSocket,
    next_sequence: u64,
}

impl FramedReceiver {
    fn receive<T: DeserializeOwned>(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<ReceivedMessage<T>> {
        receive_frame(&self.socket, &mut self.next_sequence, timeout)
    }
}

fn send_frame<T: Serialize>(
    socket: &PacketSocket,
    next_sequence: &mut u64,
    message: T,
    descriptors: Vec<OwnedFd>,
) -> Result<()> {
    if *next_sequence == u64::MAX {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::SequenceViolation,
            "inference worker outbound sequence space is exhausted",
        ));
    }
    let frame = Frame::new(*next_sequence, message);
    let bytes = serde_json::to_vec(&frame).map_err(|error| {
        WorkerProtocolError::new(
            WorkerProtocolErrorCode::MalformedFrame,
            format!("failed to encode inference worker control frame: {error}"),
        )
    })?;
    socket.send_packet(&bytes, descriptors)?;
    *next_sequence += 1;
    Ok(())
}

fn receive_frame<T: DeserializeOwned>(
    socket: &PacketSocket,
    next_sequence: &mut u64,
    timeout: Option<Duration>,
) -> Result<ReceivedMessage<T>> {
    let packet = match timeout {
        Some(timeout) => socket.receive_packet_timeout(timeout)?,
        None => socket.receive_packet()?,
    };
    let (bytes, descriptors) = packet.into_parts();
    let frame: Frame<T> = serde_json::from_slice(&bytes).map_err(|error| {
        WorkerProtocolError::new(
            WorkerProtocolErrorCode::MalformedFrame,
            format!("failed to decode inference worker control frame: {error}"),
        )
    })?;
    if frame.frame_version != WORKER_FRAME_VERSION {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::IdentityMismatch,
            format!(
                "inference worker frame version mismatch: expected {WORKER_FRAME_VERSION}, received {}",
                frame.frame_version
            ),
        ));
    }
    if frame.sequence != *next_sequence {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::SequenceViolation,
            format!(
                "inference worker frame sequence mismatch: expected {}, received {}",
                *next_sequence, frame.sequence
            ),
        ));
    }
    if *next_sequence == u64::MAX {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::SequenceViolation,
            "inference worker inbound sequence space is exhausted",
        ));
    }
    *next_sequence += 1;
    Ok(ReceivedMessage {
        message: frame.message,
        descriptors,
    })
}

pub struct HostControlChannel {
    channel: FramedChannel,
}

impl std::fmt::Debug for HostControlChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostControlChannel")
            .finish_non_exhaustive()
    }
}

impl HostControlChannel {
    pub fn send(&mut self, command: HostCommand) -> Result<()> {
        self.send_with_descriptors(command, Vec::new())
    }

    pub fn send_with_descriptors(
        &mut self,
        command: HostCommand,
        descriptors: Vec<OwnedFd>,
    ) -> Result<()> {
        command.validate_descriptor_contract(descriptors.len())?;
        self.channel.send(command, descriptors)
    }

    pub fn receive(&mut self) -> Result<WorkerEvent> {
        receive_event_without_descriptors(self.receive_with_descriptors()?)
    }

    pub fn receive_timeout(&mut self, timeout: Duration) -> Result<WorkerEvent> {
        receive_event_without_descriptors(self.receive_timeout_with_descriptors(timeout)?)
    }

    pub fn receive_with_descriptors(&mut self) -> Result<ReceivedMessage<WorkerEvent>> {
        let received: ReceivedMessage<WorkerEvent> = self.channel.receive(None)?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }

    pub fn receive_timeout_with_descriptors(
        &mut self,
        timeout: Duration,
    ) -> Result<ReceivedMessage<WorkerEvent>> {
        let received: ReceivedMessage<WorkerEvent> = self.channel.receive(Some(timeout))?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }

    pub fn into_split(self) -> Result<(HostCommandSender, WorkerEventReceiver)> {
        let (sender, receiver) = self.channel.into_split()?;
        Ok((
            HostCommandSender { sender },
            WorkerEventReceiver { receiver },
        ))
    }
}

pub struct HostCommandSender {
    sender: FramedSender,
}

impl std::fmt::Debug for HostCommandSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostCommandSender")
            .finish_non_exhaustive()
    }
}

impl HostCommandSender {
    pub fn send(&mut self, command: HostCommand) -> Result<()> {
        self.send_with_descriptors(command, Vec::new())
    }

    pub fn send_with_descriptors(
        &mut self,
        command: HostCommand,
        descriptors: Vec<OwnedFd>,
    ) -> Result<()> {
        command.validate_descriptor_contract(descriptors.len())?;
        self.sender.send(command, descriptors)
    }

    pub fn shutdown(&self) -> Result<()> {
        self.sender.shutdown()
    }
}

pub struct WorkerEventReceiver {
    receiver: FramedReceiver,
}

impl std::fmt::Debug for WorkerEventReceiver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerEventReceiver")
            .finish_non_exhaustive()
    }
}

impl WorkerEventReceiver {
    pub fn receive(&mut self) -> Result<WorkerEvent> {
        receive_event_without_descriptors(self.receive_with_descriptors()?)
    }

    pub fn receive_timeout(&mut self, timeout: Duration) -> Result<WorkerEvent> {
        receive_event_without_descriptors(self.receive_timeout_with_descriptors(timeout)?)
    }

    pub fn receive_with_descriptors(&mut self) -> Result<ReceivedMessage<WorkerEvent>> {
        let received: ReceivedMessage<WorkerEvent> = self.receiver.receive(None)?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }

    pub fn receive_timeout_with_descriptors(
        &mut self,
        timeout: Duration,
    ) -> Result<ReceivedMessage<WorkerEvent>> {
        let received: ReceivedMessage<WorkerEvent> = self.receiver.receive(Some(timeout))?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }
}

pub struct WorkerControlChannel {
    channel: FramedChannel,
}

impl std::fmt::Debug for WorkerControlChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerControlChannel")
            .finish_non_exhaustive()
    }
}

impl WorkerControlChannel {
    pub fn from_inherited_env() -> Result<Self> {
        if INHERITED_CONTROL_FD_TAKEN.swap(true, Ordering::AcqRel) {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::MalformedFrame,
                "inherited inference worker control descriptor was already claimed",
            ));
        }
        let expected_parent = std::env::var(INHERITED_PARENT_PID_ENV)
            .ok()
            .and_then(|value| value.parse::<libc::pid_t>().ok())
            .filter(|parent| *parent > 1)
            .ok_or_else(|| {
                WorkerProtocolError::new(
                    WorkerProtocolErrorCode::MalformedFrame,
                    "inference worker parent identity is absent or invalid",
                )
            })?;
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::Io,
                "failed to arm inference worker parent-death handling",
            ));
        }
        if unsafe { libc::getppid() } != expected_parent {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::PeerClosed,
                "inference worker parent exited before control-channel admission",
            ));
        }
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::Io,
                "failed to disable inference worker core dumps",
            ));
        }
        let descriptor = std::env::var(INHERITED_CONTROL_FD_ENV)
            .ok()
            .and_then(|value| value.parse::<RawFd>().ok())
            .filter(|descriptor| *descriptor > libc::STDERR_FILENO)
            .ok_or_else(|| {
                WorkerProtocolError::new(
                    WorkerProtocolErrorCode::MalformedFrame,
                    "inherited inference worker control descriptor is absent or invalid",
                )
            })?;
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        set_close_on_exec(descriptor.as_raw_fd())?;
        Ok(Self {
            channel: FramedChannel::new(PacketSocket::from_owned_fd(descriptor)?),
        })
    }

    pub fn receive(&mut self) -> Result<HostCommand> {
        receive_command_without_descriptors(self.receive_with_descriptors()?)
    }

    pub fn validate_inherited_process_hardening(&self) -> Result<()> {
        if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                "inference worker did not inherit no-new-privileges",
            ));
        }
        if unsafe { libc::prctl(libc::PR_GET_DUMPABLE) } != 0 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                "inference worker remained dumpable after exec",
            ));
        }
        let mut core_limit = MaybeUninit::<libc::rlimit>::zeroed();
        if unsafe { libc::getrlimit(libc::RLIMIT_CORE, core_limit.as_mut_ptr()) } != 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::WorkerUntrusted,
                "failed to inspect inference worker core limit",
            ));
        }
        let core_limit = unsafe { core_limit.assume_init() };
        if core_limit.rlim_cur != 0 || core_limit.rlim_max != 0 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                "inference worker core dumps were not disabled",
            ));
        }

        let directory = std::fs::read_dir("/proc/self/fd").map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!("failed to inspect inference worker descriptor table: {error}"),
            )
        })?;
        let candidates = directory
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
            .collect::<Vec<_>>();
        let control_descriptor = self.channel.socket.as_raw_fd();
        for descriptor in candidates {
            if descriptor <= libc::STDERR_FILENO || descriptor == control_descriptor {
                continue;
            }
            if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0 {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::UnexpectedDescriptors,
                    format!(
                        "inference worker inherited unrelated descriptor {descriptor} across exec"
                    ),
                ));
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
                return Err(WorkerProtocolError::last_os_error(
                    WorkerProtocolErrorCode::WorkerUntrusted,
                    "failed to verify inference worker descriptor closure",
                ));
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn control_descriptor(&self) -> RawFd {
        self.channel.socket.as_raw_fd()
    }

    pub fn receive_timeout(&mut self, timeout: Duration) -> Result<HostCommand> {
        receive_command_without_descriptors(self.receive_timeout_with_descriptors(timeout)?)
    }

    pub fn send(&mut self, event: WorkerEvent) -> Result<()> {
        self.send_with_descriptors(event, Vec::new())
    }

    pub fn send_with_descriptors(
        &mut self,
        event: WorkerEvent,
        descriptors: Vec<OwnedFd>,
    ) -> Result<()> {
        event.validate_descriptor_contract(descriptors.len())?;
        self.channel.send(event, descriptors)
    }

    pub fn receive_with_descriptors(&mut self) -> Result<ReceivedMessage<HostCommand>> {
        let received: ReceivedMessage<HostCommand> = self.channel.receive(None)?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }

    pub fn receive_timeout_with_descriptors(
        &mut self,
        timeout: Duration,
    ) -> Result<ReceivedMessage<HostCommand>> {
        let received: ReceivedMessage<HostCommand> = self.channel.receive(Some(timeout))?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }

    pub fn into_split(self) -> Result<(WorkerCommandReceiver, WorkerEventSender)> {
        let (sender, receiver) = self.channel.into_split()?;
        Ok((
            WorkerCommandReceiver { receiver },
            WorkerEventSender { sender },
        ))
    }
}

pub struct WorkerCommandReceiver {
    receiver: FramedReceiver,
}

impl std::fmt::Debug for WorkerCommandReceiver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerCommandReceiver")
            .finish_non_exhaustive()
    }
}

impl WorkerCommandReceiver {
    pub fn receive(&mut self) -> Result<HostCommand> {
        receive_command_without_descriptors(self.receive_with_descriptors()?)
    }

    pub fn receive_timeout(&mut self, timeout: Duration) -> Result<HostCommand> {
        receive_command_without_descriptors(self.receive_timeout_with_descriptors(timeout)?)
    }

    pub fn receive_with_descriptors(&mut self) -> Result<ReceivedMessage<HostCommand>> {
        let received: ReceivedMessage<HostCommand> = self.receiver.receive(None)?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }

    pub fn receive_timeout_with_descriptors(
        &mut self,
        timeout: Duration,
    ) -> Result<ReceivedMessage<HostCommand>> {
        let received: ReceivedMessage<HostCommand> = self.receiver.receive(Some(timeout))?;
        received
            .message()
            .validate_descriptor_contract(received.descriptors().len())?;
        Ok(received)
    }
}

pub struct WorkerEventSender {
    sender: FramedSender,
}

impl std::fmt::Debug for WorkerEventSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerEventSender")
            .finish_non_exhaustive()
    }
}

impl WorkerEventSender {
    pub fn send(&mut self, event: WorkerEvent) -> Result<()> {
        self.send_with_descriptors(event, Vec::new())
    }

    pub fn send_with_descriptors(
        &mut self,
        event: WorkerEvent,
        descriptors: Vec<OwnedFd>,
    ) -> Result<()> {
        event.validate_descriptor_contract(descriptors.len())?;
        self.sender.send(event, descriptors)
    }

    pub fn shutdown(&self) -> Result<()> {
        self.sender.shutdown()
    }
}

fn receive_command_without_descriptors(
    received: ReceivedMessage<HostCommand>,
) -> Result<HostCommand> {
    let (command, descriptors) = received.into_parts();
    descriptors.ensure_empty()?;
    Ok(command)
}

fn receive_event_without_descriptors(
    received: ReceivedMessage<WorkerEvent>,
) -> Result<WorkerEvent> {
    let (event, descriptors) = received.into_parts();
    descriptors.ensure_empty()?;
    Ok(event)
}

pub fn control_channel_pair() -> Result<(HostControlChannel, WorkerControlChannel)> {
    let (host, worker) = PacketSocket::pair()?;
    Ok((
        HostControlChannel {
            channel: FramedChannel::new(host),
        },
        WorkerControlChannel {
            channel: FramedChannel::new(worker),
        },
    ))
}

pub(super) fn launch_channel_pair() -> Result<(HostControlChannel, PacketSocket)> {
    let (host, worker) = PacketSocket::pair()?;
    Ok((
        HostControlChannel {
            channel: FramedChannel::new(host),
        },
        worker,
    ))
}

fn set_close_on_exec(descriptor: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(WorkerProtocolError::last_os_error(
            WorkerProtocolErrorCode::Io,
            "failed to protect the inherited inference worker control descriptor",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;

    use serde_json::json;

    use super::*;
    use crate::worker_protocol::{Handshake, ProtocolLimits, Ready};

    #[test]
    fn typed_channels_round_trip_and_enforce_sequence() {
        let (mut host, mut worker) = control_channel_pair().expect("channel pair");
        host.send(HostCommand::Handshake(Handshake::current()))
            .expect("send handshake");
        assert!(matches!(
            worker.receive().expect("receive handshake"),
            HostCommand::Handshake(_)
        ));
        worker
            .send(WorkerEvent::Ready(Ready::current()))
            .expect("send ready");
        assert!(matches!(
            host.receive().expect("receive ready"),
            WorkerEvent::Ready(_)
        ));
    }

    #[test]
    fn malformed_unknown_and_out_of_order_frames_fail_closed() {
        for bytes in [b"{".as_slice(), &[0xff][..]] {
            let (sender, receiver) = PacketSocket::pair().expect("socket pair");
            sender.send_packet(bytes, Vec::new()).expect("send raw");
            let mut worker = WorkerControlChannel {
                channel: FramedChannel::new(receiver),
            };
            assert_eq!(
                worker.receive().expect_err("malformed frame").code(),
                WorkerProtocolErrorCode::MalformedFrame
            );
        }

        let unknown_kind = serde_json::to_vec(&json!({
            "frame_version": WORKER_FRAME_VERSION,
            "sequence": 1,
            "message": {"unknown_command": {}}
        }))
        .expect("unknown kind JSON");
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        sender
            .send_packet(&unknown_kind, Vec::new())
            .expect("send unknown kind");
        let mut worker = WorkerControlChannel {
            channel: FramedChannel::new(receiver),
        };
        assert_eq!(
            worker.receive().expect_err("unknown kind").code(),
            WorkerProtocolErrorCode::MalformedFrame
        );

        let unknown_field = serde_json::to_vec(&json!({
            "frame_version": WORKER_FRAME_VERSION,
            "sequence": 1,
            "message": {"handshake": {
                "identity": {
                    "protocol_id": super::super::super::WORKER_PROTOCOL_ID,
                    "build_id": super::super::super::WORKER_BUILD_ID,
                    "unknown": true
                },
                "limits": ProtocolLimits::current()
            }}
        }))
        .expect("unknown field JSON");
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        sender
            .send_packet(&unknown_field, Vec::new())
            .expect("send unknown field");
        let mut worker = WorkerControlChannel {
            channel: FramedChannel::new(receiver),
        };
        assert_eq!(
            worker.receive().expect_err("unknown field").code(),
            WorkerProtocolErrorCode::MalformedFrame
        );

        let frame =
            serde_json::to_vec(&Frame::new(2, HostCommand::Handshake(Handshake::current())))
                .expect("frame JSON");
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        sender.send_packet(&frame, Vec::new()).expect("send gap");
        let mut worker = WorkerControlChannel {
            channel: FramedChannel::new(receiver),
        };
        assert_eq!(
            worker.receive().expect_err("sequence gap").code(),
            WorkerProtocolErrorCode::SequenceViolation
        );
    }

    #[test]
    fn frame_bounds_are_enforced_at_send_and_receive() {
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        let maximum = vec![b'x'; MAX_CONTROL_FRAME_BYTES];
        sender
            .send_packet(&maximum, Vec::new())
            .expect("maximum frame");
        assert_eq!(
            receiver.receive_packet().expect("receive maximum").bytes(),
            maximum
        );

        let oversized = vec![b'x'; MAX_CONTROL_FRAME_BYTES + 1];
        assert_eq!(
            sender
                .send_packet(&oversized, Vec::new())
                .expect_err("oversized send")
                .code(),
            WorkerProtocolErrorCode::FrameTooLarge
        );

        let sent = unsafe {
            libc::send(
                sender.as_raw_fd(),
                oversized.as_ptr().cast(),
                oversized.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        assert_eq!(sent, oversized.len() as isize);
        assert_eq!(
            receiver
                .receive_packet()
                .expect_err("oversized receive")
                .code(),
            WorkerProtocolErrorCode::FrameTooLarge
        );
    }

    #[test]
    fn typed_frames_reject_descriptors() {
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        let frame =
            serde_json::to_vec(&Frame::new(1, HostCommand::Handshake(Handshake::current())))
                .expect("frame JSON");
        let descriptor = std::fs::File::open("/dev/null").expect("open /dev/null");
        sender
            .send_packet(&frame, vec![descriptor.into()])
            .expect("send descriptor");
        let mut worker = WorkerControlChannel {
            channel: FramedChannel::new(receiver),
        };
        assert_eq!(
            worker.receive().expect_err("unexpected descriptor").code(),
            WorkerProtocolErrorCode::UnexpectedDescriptors
        );

        let (probe_write, mut probe_read) = UnixStream::pair().expect("ownership probe pair");
        probe_read
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("bound ownership probe read");
        let too_many =
            std::iter::once(OwnedFd::from(probe_write))
                .chain((1..=MAX_CONTROL_DESCRIPTORS).map(|_| {
                    OwnedFd::from(std::fs::File::open("/dev/null").expect("open /dev/null"))
                }))
                .collect::<Vec<OwnedFd>>();
        assert_eq!(
            sender
                .send_packet(&frame, too_many)
                .expect_err("descriptor send bound")
                .code(),
            WorkerProtocolErrorCode::DescriptorLimit
        );
        let mut byte = [0_u8; 1];
        assert_eq!(
            probe_read
                .read(&mut byte)
                .expect("probe descriptor ownership"),
            0,
            "rejected descriptor vector must be dropped by the send boundary"
        );

        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        let files = (0..=MAX_CONTROL_DESCRIPTORS)
            .map(|_| std::fs::File::open("/dev/null").expect("open /dev/null"))
            .collect::<Vec<_>>();
        send_unbounded_descriptors(&sender, &frame, &files);
        assert_eq!(
            receiver
                .receive_packet()
                .expect_err("descriptor receive bound")
                .code(),
            WorkerProtocolErrorCode::DescriptorLimit
        );
    }

    #[test]
    fn peer_close_is_a_typed_protocol_event() {
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");
        drop(sender);
        assert_eq!(
            receiver.receive_packet().expect_err("peer close").code(),
            WorkerProtocolErrorCode::PeerClosed
        );
    }

    fn send_unbounded_descriptors(socket: &PacketSocket, bytes: &[u8], files: &[std::fs::File]) {
        let raw_descriptors = files.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let descriptor_bytes = mem::size_of_val(raw_descriptors.as_slice());
        let control_len = unsafe { libc::CMSG_SPACE(descriptor_bytes as libc::c_uint) as usize };
        let mut control = vec![0_u8; control_len];
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(descriptor_bytes as libc::c_uint) as _;
            std::ptr::copy_nonoverlapping(
                raw_descriptors.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                descriptor_bytes,
            );
        }
        assert_eq!(
            unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }
}
