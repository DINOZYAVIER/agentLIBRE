use std::any::Any;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// Content verification performed when Forge synchronizes the materialization.
/// Opening a reader does not hash the file again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationStatus {
    Verified,
    Unchecked,
}

impl VerificationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unchecked => "unchecked",
        }
    }
}

impl From<VerificationStatus> for serde_json::Value {
    fn from(status: VerificationStatus) -> Self {
        Self::String(status.as_str().to_owned())
    }
}

/// An opened Forge reader that owns its backend read guard.
///
/// The backend must open a contained regular file and acquire the guard before
/// constructing this value. The SDK does not resolve paths or verify content.
/// The file is closed before its guard is released; neither can be extracted.
pub struct ForgeFile {
    file: File,
    _guard: Box<dyn Any + Send + Sync>,
    verification_status: VerificationStatus,
}

impl ForgeFile {
    pub fn new(
        file: File,
        guard: Box<dyn Any + Send + Sync>,
        verification_status: VerificationStatus,
    ) -> Self {
        Self {
            file,
            _guard: guard,
            verification_status,
        }
    }

    /// The published read-only copy path, resolved from the held file descriptor.
    /// Reading that path with another process does not transfer this read guard.
    pub fn read_only_path(&self) -> io::Result<std::path::PathBuf> {
        use std::os::fd::AsRawFd;
        std::fs::read_link(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }

    pub fn verification_status(&self) -> VerificationStatus {
        self.verification_status
    }
}

impl Read for ForgeFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for ForgeFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Guard(Arc<AtomicBool>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn binary_reader_seeks_and_holds_guard_until_drop() {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agl-forge-reader-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut writer = File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writer.write_all(&[0, 255, 128, 42]).unwrap();
        drop(writer);
        let file = File::open(&path).unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let mut reader = ForgeFile::new(
            file,
            Box::new(Guard(released.clone())),
            VerificationStatus::Verified,
        );
        assert_eq!(reader.read_only_path().unwrap(), path);
        std::fs::remove_file(path).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, [0, 255, 128, 42]);
        reader.seek(SeekFrom::Start(1)).unwrap();
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [255]);
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(reader.verification_status(), VerificationStatus::Verified);
        drop(reader);
        assert!(released.load(Ordering::Acquire));
        assert_eq!(
            serde_json::Value::from(VerificationStatus::Verified),
            "verified"
        );
        assert_eq!(
            serde_json::Value::from(VerificationStatus::Unchecked),
            "unchecked"
        );
    }
}
