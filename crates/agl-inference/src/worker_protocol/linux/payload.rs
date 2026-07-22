use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::fd::{AsRawFd, FromRawFd as _, OwnedFd};
use std::os::unix::fs::MetadataExt as _;

use sha2::{Digest as _, Sha256};

use super::super::{
    MAX_CONTROL_DESCRIPTORS, MAX_SEALED_PAYLOAD_BYTES, Result, SealedPayload, WorkerProtocolError,
    WorkerProtocolErrorCode,
};
use super::DescriptorSet;

const REQUIRED_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

impl SealedPayload {
    pub fn read_from(&self, descriptors: &mut DescriptorSet) -> Result<Vec<u8>> {
        let descriptor = descriptors.take(usize::from(self.descriptor_index))?;
        self.read_descriptor(descriptor)
    }

    pub fn read_descriptor(&self, descriptor: OwnedFd) -> Result<Vec<u8>> {
        if self.byte_len > MAX_SEALED_PAYLOAD_BYTES {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::PayloadTooLarge,
                format!(
                    "sealed inference payload is {} bytes; the limit is {MAX_SEALED_PAYLOAD_BYTES}",
                    self.byte_len
                ),
            ));
        }

        let mut file = File::from(descriptor);
        let metadata = file.metadata().map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                format!("failed to inspect sealed inference payload: {error}"),
            )
        })?;
        if metadata.mode() & libc::S_IFMT != libc::S_IFREG {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                "sealed inference payload descriptor is not a regular file",
            ));
        }
        if metadata.len() != self.byte_len {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                format!(
                    "sealed inference payload length mismatch: manifest {}, descriptor {}",
                    self.byte_len,
                    metadata.len()
                ),
            ));
        }

        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::InvalidPayload,
                "failed to inspect inference payload seals",
            ));
        }
        if seals & REQUIRED_SEALS != REQUIRED_SEALS {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                "inference payload is not sealed against write, grow, shrink, and seal changes",
            ));
        }

        file.seek(SeekFrom::Start(0)).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                format!("failed to rewind sealed inference payload: {error}"),
            )
        })?;
        let length = usize::try_from(self.byte_len).map_err(|_| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::PayloadTooLarge,
                "sealed inference payload length is not addressable on this host",
            )
        })?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                format!("failed to reserve bounded inference payload buffer: {error}"),
            )
        })?;
        file.read_to_end(&mut bytes).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                format!("failed to read sealed inference payload: {error}"),
            )
        })?;
        if bytes.len() != length {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                "sealed inference payload changed length while it was read",
            ));
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != self.sha256 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::InvalidPayload,
                "sealed inference payload digest does not match its manifest",
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug)]
pub struct SealedPayloadTransfer {
    manifest: SealedPayload,
    descriptor: OwnedFd,
}

impl SealedPayloadTransfer {
    pub fn new(bytes: &[u8], descriptor_index: u16) -> Result<Self> {
        if bytes.len() as u64 > MAX_SEALED_PAYLOAD_BYTES {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::PayloadTooLarge,
                format!(
                    "inference payload is {} bytes; the limit is {MAX_SEALED_PAYLOAD_BYTES}",
                    bytes.len()
                ),
            ));
        }
        if usize::from(descriptor_index) >= MAX_CONTROL_DESCRIPTORS {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::DescriptorLimit,
                format!(
                    "inference payload descriptor index {descriptor_index} exceeds the control descriptor bound"
                ),
            ));
        }

        let descriptor = unsafe {
            libc::memfd_create(
                c"agl-inference-payload".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if descriptor < 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::Io,
                "failed to create sealed inference payload",
            ));
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        file.write_all(bytes).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::Io,
                format!("failed to write sealed inference payload: {error}"),
            )
        })?;
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::Io,
                format!("failed to rewind sealed inference payload: {error}"),
            )
        })?;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS) } < 0 {
            return Err(WorkerProtocolError::last_os_error(
                WorkerProtocolErrorCode::Io,
                "failed to seal inference payload",
            ));
        }

        Ok(Self {
            manifest: SealedPayload {
                descriptor_index,
                byte_len: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
            },
            descriptor: file.into(),
        })
    }

    pub fn manifest(&self) -> &SealedPayload {
        &self.manifest
    }

    pub fn into_parts(self) -> (SealedPayload, OwnedFd) {
        (self.manifest, self.descriptor)
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::MetadataExt as _;

    use super::*;
    use crate::worker_protocol::{PacketSocket, WorkerProtocolErrorCode};

    #[test]
    fn sealed_payload_round_trips_over_scm_rights_with_single_owner() {
        let bytes = b"bounded image bytes";
        let transfer = SealedPayloadTransfer::new(bytes, 0).expect("sealed payload");
        let (manifest, descriptor) = transfer.into_parts();
        let source_raw_fd = descriptor.as_raw_fd();
        let source_identity = descriptor_identity(source_raw_fd);
        let (sender, receiver) = PacketSocket::pair().expect("socket pair");

        sender
            .send_packet(b"payload", vec![descriptor])
            .expect("send payload descriptor");
        assert_descriptor_identity_released(source_raw_fd, source_identity);

        let mut packet = receiver.receive_packet().expect("receive payload");
        assert_eq!(packet.descriptors().len(), 1);
        let received = packet.descriptors_mut().take(0).expect("received fd");
        let received_raw_fd = received.as_raw_fd();
        let received_identity = descriptor_identity(received_raw_fd);
        let flags = unsafe { libc::fcntl(received_raw_fd, libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert_eq!(
            manifest.read_descriptor(received).expect("verify payload"),
            bytes
        );
        assert_descriptor_identity_released(received_raw_fd, received_identity);
    }

    fn descriptor_identity(raw_fd: i32) -> (u64, u64) {
        let metadata = std::fs::metadata(format!("/proc/self/fd/{raw_fd}"))
            .expect("inspect owned descriptor identity");
        (metadata.dev(), metadata.ino())
    }

    fn assert_descriptor_identity_released(raw_fd: i32, original: (u64, u64)) {
        match std::fs::metadata(format!("/proc/self/fd/{raw_fd}")) {
            Ok(metadata) => assert_ne!(
                (metadata.dev(), metadata.ino()),
                original,
                "the moved descriptor identity must no longer be owned by this raw FD slot"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to inspect released descriptor slot: {error}"),
        }
    }

    #[test]
    fn payload_requires_all_seals_and_matching_manifest() {
        let bytes = b"content";
        let transfer = SealedPayloadTransfer::new(bytes, 0).expect("sealed payload");
        let (mut manifest, descriptor) = transfer.into_parts();
        manifest.sha256[0] ^= 0xff;
        assert_eq!(
            manifest
                .read_descriptor(descriptor)
                .expect_err("digest mismatch")
                .code(),
            WorkerProtocolErrorCode::InvalidPayload
        );

        let descriptor = unsafe {
            libc::memfd_create(
                c"agl-inference-unsealed-test".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(descriptor >= 0);
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        file.write_all(bytes).expect("write unsealed payload");
        let manifest = SealedPayload {
            descriptor_index: 0,
            byte_len: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        };
        assert_eq!(
            manifest
                .read_descriptor(file.into())
                .expect_err("unsealed payload")
                .code(),
            WorkerProtocolErrorCode::InvalidPayload
        );
    }

    #[test]
    fn payload_bounds_and_descriptor_use_are_enforced() {
        let manifest = SealedPayload {
            descriptor_index: 0,
            byte_len: MAX_SEALED_PAYLOAD_BYTES + 1,
            sha256: [0; 32],
        };
        let file = File::open("/dev/null").expect("open /dev/null");
        assert_eq!(
            manifest
                .read_descriptor(file.into())
                .expect_err("oversized manifest")
                .code(),
            WorkerProtocolErrorCode::PayloadTooLarge
        );

        let transfer = SealedPayloadTransfer::new(b"one shot", 0).expect("sealed payload");
        let (manifest, descriptor) = transfer.into_parts();
        let mut descriptors = DescriptorSet::new(vec![descriptor]);
        assert_eq!(
            manifest.read_from(&mut descriptors).expect("first consume"),
            b"one shot"
        );
        assert_eq!(
            manifest
                .read_from(&mut descriptors)
                .expect_err("second consume")
                .code(),
            WorkerProtocolErrorCode::InvalidPayload
        );
    }
}
