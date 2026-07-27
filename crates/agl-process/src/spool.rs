use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agl_ids::ExecutionId;
use sha2::{Digest, Sha256};

use crate::{
    CommittedOutputFrame, ExecutionChannel, ExecutionOutputChunk, OutputSpool, OutputSpoolRead,
    ProcessBytes, ProcessError, ProcessErrorCode, Result,
};

const HEADER: &[u8] = b"AGLSPOOL\x01";
const FRAME_MAGIC: &[u8; 4] = b"FRM1";
const FRAME_FIXED_BYTES: usize = 4 + 8 + 1 + 8 + 4 + 32;

pub struct FileOutputSpool {
    root: PathBuf,
    lock: Mutex<()>,
}

impl FileOutputSpool {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        ensure_private_directory(&root)?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn execution_directory(&self, execution_id: &ExecutionId) -> PathBuf {
        self.root.join(execution_id.as_str())
    }

    fn spool_path(&self, execution_id: &ExecutionId) -> PathBuf {
        self.execution_directory(execution_id)
            .join("stream.aglspool")
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.lock.lock().map_err(|_| {
            ProcessError::new(ProcessErrorCode::Internal, "output spool lock is poisoned")
        })
    }

    fn open(&self, execution_id: &ExecutionId, create: bool) -> Result<File> {
        let path = self.spool_path(execution_id);
        reject_symlink(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(create);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let file = options.open(&path).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                format!("failed to open private output spool: {error}"),
            )
        })?;
        let metadata = file.metadata().map_err(spool_io)?;
        if !metadata.is_file() {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "private output spool is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.nlink() != 1 {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "private output spool has an unexpected hard-link count",
                ));
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "private output spool has an unexpected owner",
                ));
            }
            if metadata.permissions().mode() & 0o777 != 0o600 {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "private output spool permissions are not 0600",
                ));
            }
        }
        Ok(file)
    }
}

impl OutputSpool for FileOutputSpool {
    fn prepare(&self, execution_id: &ExecutionId) -> Result<()> {
        let _guard = self.lock()?;
        let directory = self.execution_directory(execution_id);
        ensure_private_directory(&directory)?;
        let mut file = self.open(execution_id, true)?;
        if file.metadata().map_err(spool_io)?.len() == 0 {
            file.write_all(HEADER).map_err(spool_io)?;
            file.sync_data().map_err(spool_io)?;
        } else {
            validate_header(&mut file)?;
        }
        Ok(())
    }

    fn append(&self, execution_id: &ExecutionId, chunk: &ExecutionOutputChunk) -> Result<u64> {
        let _guard = self.lock()?;
        let mut file = self.open(execution_id, false)?;
        validate_header(&mut file)?;
        let offset = file.seek(SeekFrom::End(0)).map_err(spool_io)?;
        let payload = chunk.bytes.decode(usize::MAX)?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::InputTooLarge,
                "output spool frame is too large",
            )
        })?;
        let digest = Sha256::digest(&payload);
        file.write_all(FRAME_MAGIC).map_err(spool_io)?;
        file.write_all(&chunk.sequence.to_le_bytes())
            .map_err(spool_io)?;
        file.write_all(&[channel_tag(chunk.channel)])
            .map_err(spool_io)?;
        file.write_all(&unix_millis().to_le_bytes())
            .map_err(spool_io)?;
        file.write_all(&payload_len.to_le_bytes())
            .map_err(spool_io)?;
        file.write_all(&digest).map_err(spool_io)?;
        file.write_all(&payload).map_err(spool_io)?;
        Ok(offset)
    }

    fn sync(&self, execution_id: &ExecutionId) -> Result<()> {
        let _guard = self.lock()?;
        self.open(execution_id, false)?
            .sync_data()
            .map_err(spool_io)
    }

    fn read(
        &self,
        execution_id: &ExecutionId,
        after_sequence: u64,
        through_sequence: u64,
        maximum_bytes: usize,
    ) -> Result<OutputSpoolRead> {
        let _guard = self.lock()?;
        let mut file = self.open(execution_id, false)?;
        validate_header(&mut file)?;
        let mut chunks = Vec::new();
        let mut returned_bytes = 0usize;
        let mut complete = true;
        while let Some(frame) = read_frame(&mut file)? {
            if frame.sequence <= after_sequence || frame.sequence > through_sequence {
                continue;
            }
            if returned_bytes != 0
                && returned_bytes.saturating_add(frame.payload.len()) > maximum_bytes
            {
                complete = false;
                break;
            }
            if frame.payload.len() > maximum_bytes {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "one committed output frame exceeds the configured read bound",
                ));
            }
            returned_bytes = returned_bytes.saturating_add(frame.payload.len());
            chunks.push(ExecutionOutputChunk {
                sequence: frame.sequence,
                channel: frame.channel,
                bytes: ProcessBytes::from_bytes(&frame.payload),
            });
        }
        Ok(OutputSpoolRead { chunks, complete })
    }

    fn recover(
        &self,
        execution_id: &ExecutionId,
        committed: &[CommittedOutputFrame],
    ) -> Result<()> {
        let _guard = self.lock()?;
        let mut file = self.open(execution_id, false)?;
        validate_header(&mut file)?;
        let mut truncate_at = file.stream_position().map_err(spool_io)?;
        let mut committed_index = 0usize;
        loop {
            let frame_start = file.stream_position().map_err(spool_io)?;
            let frame = match read_frame(&mut file) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) if committed_index == committed.len() => {
                    truncate_at = frame_start;
                    break;
                }
                Err(error) => return Err(error),
            };
            let Some(expected) = committed.get(committed_index) else {
                truncate_at = frame_start;
                break;
            };
            validate_committed_frame(frame_start, &frame, expected)?;
            committed_index += 1;
            truncate_at = file.stream_position().map_err(spool_io)?;
        }
        if committed_index != committed.len() {
            let missing = &committed[committed_index];
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                format!(
                    "private output spool is missing committed output sequence {}",
                    missing.sequence
                ),
            ));
        }
        let length = file.metadata().map_err(spool_io)?.len();
        if truncate_at < length {
            file.set_len(truncate_at).map_err(spool_io)?;
            file.sync_data().map_err(spool_io)?;
        }
        Ok(())
    }

    fn remove(&self, execution_id: &ExecutionId) -> Result<()> {
        let _guard = self.lock()?;
        let directory = self.execution_directory(execution_id);
        validate_private_directory(&directory)?;
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(spool_io(error)),
        }
    }
}

fn validate_committed_frame(
    frame_start: u64,
    frame: &Frame,
    expected: &CommittedOutputFrame,
) -> Result<()> {
    let digest = sha256_digest(&frame.payload);
    if frame.sequence != expected.sequence
        || frame.channel != expected.channel
        || frame_start != expected.spool_offset
        || frame.payload.len() as u64 != expected.byte_length
        || digest != expected.safe_digest
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!(
                "private output spool frame does not match committed metadata at sequence {}",
                expected.sequence
            ),
        ));
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in Sha256::digest(bytes) {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

struct Frame {
    sequence: u64,
    channel: ExecutionChannel,
    payload: Vec<u8>,
}

fn read_frame(file: &mut File) -> Result<Option<Frame>> {
    let mut fixed = [0u8; FRAME_FIXED_BYTES];
    let mut read = 0usize;
    while read < fixed.len() {
        match file.read(&mut fixed[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "private output spool has a truncated frame header",
                ));
            }
            Ok(bytes) => read += bytes,
            Err(error) => return Err(spool_io(error)),
        }
    }
    if &fixed[..4] != FRAME_MAGIC {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private output spool frame magic is invalid",
        ));
    }
    let sequence = u64::from_le_bytes(fixed[4..12].try_into().expect("fixed sequence slice"));
    let channel = parse_channel(fixed[12])?;
    let payload_len = u32::from_le_bytes(fixed[21..25].try_into().expect("fixed length slice"));
    let expected_digest = &fixed[25..57];
    let mut payload = vec![0u8; payload_len as usize];
    file.read_exact(&mut payload).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!("private output spool payload is truncated: {error}"),
        )
    })?;
    if Sha256::digest(&payload).as_slice() != expected_digest {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private output spool frame digest is invalid",
        ));
    }
    Ok(Some(Frame {
        sequence,
        channel,
        payload,
    }))
}

fn validate_header(file: &mut File) -> Result<()> {
    file.seek(SeekFrom::Start(0)).map_err(spool_io)?;
    let mut header = vec![0u8; HEADER.len()];
    file.read_exact(&mut header).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!("private output spool header is missing: {error}"),
        )
    })?;
    if header != HEADER {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private output spool version is unsupported",
        ));
    }
    Ok(())
}

fn channel_tag(channel: ExecutionChannel) -> u8 {
    match channel {
        ExecutionChannel::Stdout => 1,
        ExecutionChannel::Stderr => 2,
        ExecutionChannel::Terminal => 3,
        ExecutionChannel::Lifecycle => 4,
    }
}

fn parse_channel(value: u8) -> Result<ExecutionChannel> {
    match value {
        1 => Ok(ExecutionChannel::Stdout),
        2 => Ok(ExecutionChannel::Stderr),
        3 => Ok(ExecutionChannel::Terminal),
        4 => Ok(ExecutionChannel::Lifecycle),
        _ => Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private output spool channel tag is invalid",
        )),
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "private output path is not a real directory",
            ));
        }
    } else {
        fs::create_dir_all(path).map_err(spool_io)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(spool_io)?;
        let metadata = fs::symlink_metadata(path).map_err(spool_io)?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "private output directory owner or permissions are invalid",
            ));
        }
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(spool_io(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private output path is not a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "private output directory owner or permissions are invalid",
            ));
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private output path must not be a symlink",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(spool_io(error)),
    }
}

fn spool_io(error: std::io::Error) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::StoreCorrupt,
        format!("private output spool I/O failed: {error}"),
    )
}

fn unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_round_trips_binary_frames_and_truncates_orphan_tail() {
        let root = std::env::temp_dir().join(format!(
            "agl-process-spool-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let spool = FileOutputSpool::new(&root).unwrap();
        let id = ExecutionId::generate();
        spool.prepare(&id).unwrap();
        let first = ExecutionOutputChunk {
            sequence: 3,
            channel: ExecutionChannel::Stdout,
            bytes: ProcessBytes::from_bytes(b"hello"),
        };
        let orphan = ExecutionOutputChunk {
            sequence: 4,
            channel: ExecutionChannel::Stderr,
            bytes: ProcessBytes::from_bytes(&[0xff, 0x00]),
        };
        let first_offset = spool.append(&id, &first).unwrap();
        spool.append(&id, &orphan).unwrap();
        spool.sync(&id).unwrap();
        let first_committed = CommittedOutputFrame {
            sequence: first.sequence,
            channel: first.channel,
            spool_offset: first_offset,
            byte_length: 5,
            safe_digest: sha256_digest(b"hello"),
        };
        spool
            .recover(&id, std::slice::from_ref(&first_committed))
            .unwrap();

        assert_eq!(
            spool.read(&id, 0, 3, 64).unwrap().chunks,
            vec![first.clone()]
        );
        let terminal_gap = spool.read(&id, 0, 4, 64).unwrap();
        assert_eq!(terminal_gap.chunks.as_slice(), std::slice::from_ref(&first));
        assert!(terminal_gap.complete);

        spool.append(&id, &orphan).unwrap();
        spool.sync(&id).unwrap();
        let bounded = spool.read(&id, 0, 4, 5).unwrap();
        assert_eq!(bounded.chunks.as_slice(), std::slice::from_ref(&first));
        assert!(!bounded.complete);
        let path = spool.spool_path(&id);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(file.metadata().unwrap().len() - 1).unwrap();
        spool
            .recover(&id, std::slice::from_ref(&first_committed))
            .unwrap();
        assert_eq!(spool.read(&id, 0, 3, 64).unwrap().chunks, vec![first]);

        let committed = ExecutionId::generate();
        spool.prepare(&committed).unwrap();
        let committed_chunk = ExecutionOutputChunk {
            sequence: 1,
            channel: ExecutionChannel::Stdout,
            bytes: ProcessBytes::from_bytes(b"committed"),
        };
        let committed_offset = spool.append(&committed, &committed_chunk).unwrap();
        spool.sync(&committed).unwrap();
        let committed_frame = CommittedOutputFrame {
            sequence: 1,
            channel: ExecutionChannel::Stdout,
            spool_offset: committed_offset,
            byte_length: 9,
            safe_digest: sha256_digest(b"committed"),
        };
        let path = spool.spool_path(&committed);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(file.metadata().unwrap().len() - 1).unwrap();
        assert_eq!(
            spool
                .recover(&committed, &[committed_frame])
                .unwrap_err()
                .code(),
            ProcessErrorCode::StoreCorrupt
        );

        let missing_early = ExecutionId::generate();
        spool.prepare(&missing_early).unwrap();
        let later = ExecutionOutputChunk {
            sequence: 5,
            channel: ExecutionChannel::Terminal,
            bytes: ProcessBytes::from_bytes(b"later"),
        };
        let later_offset = spool.append(&missing_early, &later).unwrap();
        spool.sync(&missing_early).unwrap();
        let missing = CommittedOutputFrame {
            sequence: 3,
            channel: ExecutionChannel::Terminal,
            spool_offset: later_offset,
            byte_length: 5,
            safe_digest: sha256_digest(b"earlier"),
        };
        let later = CommittedOutputFrame {
            sequence: 5,
            channel: ExecutionChannel::Terminal,
            spool_offset: later_offset,
            byte_length: 5,
            safe_digest: sha256_digest(b"later"),
        };
        assert_eq!(
            spool
                .recover(&missing_early, &[missing, later])
                .unwrap_err()
                .code(),
            ProcessErrorCode::StoreCorrupt
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn spool_rejects_unsafe_modes_hard_links_and_symlink_cleanup() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!(
            "agl-process-spool-safety-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let spool = FileOutputSpool::new(&root).unwrap();
        let id = ExecutionId::generate();
        spool.prepare(&id).unwrap();
        let path = spool.spool_path(&id);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            spool.read(&id, 0, 0, 64).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let alias = root.join("hard-link-probe");
        fs::hard_link(&path, &alias).unwrap();
        assert_eq!(
            spool.read(&id, 0, 0, 64).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
        fs::remove_file(alias).unwrap();

        let victim = root.join("victim");
        fs::create_dir(&victim).unwrap();
        let linked_id = ExecutionId::generate();
        symlink(&victim, spool.execution_directory(&linked_id)).unwrap();
        assert_eq!(
            spool.remove(&linked_id).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
        assert!(victim.is_dir());
        fs::remove_file(spool.execution_directory(&linked_id)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
