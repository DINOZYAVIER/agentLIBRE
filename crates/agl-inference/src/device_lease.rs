//! Linux kernel-backed ownership of one physical inference device.
//!
//! The daemon and an allowed no-daemon standalone host must acquire the same
//! lease before creating a worker or reserving device memory. The lease is
//! deliberately process-local state backed by an open, exclusively locked
//! file: dropping it (or process death) releases ownership.

use std::error::Error;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::linux::fs::MetadataExt as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

const MAX_PHYSICAL_DEVICE_ID_BYTES: usize = 256;
const LOCK_FILE_PREFIX: &str = "device-";
const LOCK_FILE_SUFFIX: &str = ".lock";

/// A bounded canonical identity used to derive a device authority lock.
///
/// Physical device identifiers are backend-owned machine identities rather
/// than display labels. They must be printable ASCII without whitespace and
/// are normalized with ASCII lowercase before hashing. The raw identity never
/// becomes a filesystem component.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PhysicalDeviceLeaseIdentity {
    normalized: String,
    digest_hex: String,
}

impl PhysicalDeviceLeaseIdentity {
    pub fn new(identity: &str) -> Result<Self, DeviceAuthorityLeaseError> {
        if identity.is_empty()
            || identity.len() > MAX_PHYSICAL_DEVICE_ID_BYTES
            || !identity.is_ascii()
            || identity.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(DeviceAuthorityLeaseError::InvalidPhysicalDeviceIdentity);
        }

        let normalized = identity.to_ascii_lowercase();
        let digest = Sha256::digest(normalized.as_bytes());
        let mut digest_hex = String::with_capacity(digest.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in digest {
            digest_hex.push(char::from(HEX[usize::from(byte >> 4)]));
            digest_hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }

        Ok(Self {
            normalized,
            digest_hex,
        })
    }

    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    pub fn digest_hex(&self) -> &str {
        &self.digest_hex
    }

    fn lock_file_name(&self) -> String {
        format!("{LOCK_FILE_PREFIX}{}{LOCK_FILE_SUFFIX}", self.digest_hex)
    }
}

/// Exclusive AGL authority over one physical inference device.
///
/// This value must live for the complete worker/reservation lifetime. It is
/// intentionally neither cloneable nor serializable. `O_CLOEXEC` prevents an
/// inference worker from accidentally extending the host's lease lifetime.
#[derive(Debug)]
pub struct DeviceAuthorityLease {
    identity: PhysicalDeviceLeaseIdentity,
    lease_root: PathBuf,
    lock_path: PathBuf,
    // Keep the locked open file description alive until this value is dropped.
    _lock_file: File,
    // Pin the validated directory used by openat for the same lifetime.
    _root_directory: File,
}

impl DeviceAuthorityLease {
    /// Acquires the per-device lease without waiting.
    ///
    /// `lease_root` is always explicit: this module never consults HOME, XDG,
    /// environment variables, or a process working directory. Its direct
    /// parent must already exist. The final directory is created with mode
    /// 0700 when absent, then verified through its open descriptor.
    pub fn acquire(
        lease_root: impl AsRef<Path>,
        physical_device_identity: &str,
    ) -> Result<Self, DeviceAuthorityLeaseError> {
        let lease_root = lease_root.as_ref();
        let components = validated_absolute_components(lease_root)?;
        let identity = PhysicalDeviceLeaseIdentity::new(physical_device_identity)?;
        let root_directory = open_or_create_lease_root(&components)?;
        validate_lease_root(&root_directory)?;

        let lock_name = identity.lock_file_name();
        let lock_file = open_lock_file(&root_directory, &lock_name)?;
        validate_lock_file(&lock_file)?;
        lock_exclusive_nonblocking(&lock_file)?;

        // Recheck the directory and the directory entry after taking the lock.
        // This closes ordinary create/open races before the caller can allocate.
        validate_lease_root(&root_directory)?;
        validate_lock_entry(&root_directory, &lock_name, &lock_file)?;

        let lease_root = lease_root.to_path_buf();
        let lock_path = lease_root.join(&lock_name);
        Ok(Self {
            identity,
            lease_root,
            lock_path,
            _lock_file: lock_file,
            _root_directory: root_directory,
        })
    }

    pub fn identity(&self) -> &PhysicalDeviceLeaseIdentity {
        &self.identity
    }

    pub fn lease_root(&self) -> &Path {
        &self.lease_root
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceLeaseSecurityObject {
    LeaseRoot,
    LockFile,
}

impl fmt::Display for DeviceLeaseSecurityObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LeaseRoot => formatter.write_str("device lease root"),
            Self::LockFile => formatter.write_str("device lease lock file"),
        }
    }
}

#[derive(Debug)]
pub enum DeviceAuthorityLeaseError {
    InvalidLeaseRoot,
    InvalidPhysicalDeviceIdentity,
    SecurityViolation {
        object: DeviceLeaseSecurityObject,
    },
    Busy,
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl DeviceAuthorityLeaseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidLeaseRoot => "device_authority_root_invalid",
            Self::InvalidPhysicalDeviceIdentity => "device_authority_identity_invalid",
            Self::SecurityViolation { .. } => "device_authority_security",
            Self::Busy => "device_authority_busy",
            Self::Io { .. } => "device_authority_io",
        }
    }

    fn security(object: DeviceLeaseSecurityObject) -> Self {
        Self::SecurityViolation { object }
    }

    fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl fmt::Display for DeviceAuthorityLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLeaseRoot => formatter.write_str(
                "device lease root must be an exact absolute Linux path with an existing parent",
            ),
            Self::InvalidPhysicalDeviceIdentity => formatter.write_str(
                "physical device identity must be 1..=256 printable non-whitespace ASCII bytes",
            ),
            Self::SecurityViolation { object } => write!(
                formatter,
                "{object} must be non-symlink, same-owner, and have its exact private type and mode",
            ),
            Self::Busy => formatter
                .write_str("physical device authority is already owned by another AGL host"),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
        }
    }
}

impl Error for DeviceAuthorityLeaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn validated_absolute_components(root: &Path) -> Result<Vec<OsString>, DeviceAuthorityLeaseError> {
    let bytes = root.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes.first() != Some(&b'/')
        || bytes.starts_with(b"//")
        || bytes.ends_with(b"/")
    {
        return Err(DeviceAuthorityLeaseError::InvalidLeaseRoot);
    }

    let mut components = Vec::new();
    for component in bytes[1..].split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." || component.contains(&0)
        {
            return Err(DeviceAuthorityLeaseError::InvalidLeaseRoot);
        }
        components.push(OsString::from_vec(component.to_vec()));
    }
    Ok(components)
}

fn open_or_create_lease_root(components: &[OsString]) -> Result<File, DeviceAuthorityLeaseError> {
    let mut directory = open_filesystem_root()?;
    for (index, component) in components.iter().enumerate() {
        let final_component = index + 1 == components.len();
        match open_directory_at(&directory, component) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == io::ErrorKind::NotFound && final_component => {
                create_directory_at(&directory, component)?;
                directory = open_directory_at(&directory, component).map_err(|error| {
                    classify_root_open_error("open new device lease root", error)
                })?;
            }
            Err(error) => {
                return Err(classify_root_open_error(
                    "open device lease root component",
                    error,
                ));
            }
        }
    }
    Ok(directory)
}

fn open_filesystem_root() -> Result<File, DeviceAuthorityLeaseError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|error| DeviceAuthorityLeaseError::io("open filesystem root", error))
}

fn open_directory_at(parent: &File, component: &OsStr) -> io::Result<File> {
    let component = os_string_to_cstring(component)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn create_directory_at(parent: &File, component: &OsStr) -> Result<(), DeviceAuthorityLeaseError> {
    let component = os_string_to_cstring(component)
        .map_err(|error| DeviceAuthorityLeaseError::io("encode device lease root", error))?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) };
    if result == 0 {
        let chmod_result =
            unsafe { libc::fchmodat(parent.as_raw_fd(), component.as_ptr(), 0o700, 0) };
        if chmod_result == 0 {
            return Ok(());
        }
        return Err(DeviceAuthorityLeaseError::io(
            "set device lease root mode",
            io::Error::last_os_error(),
        ));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        // Another AGL process won the root creation race. The descriptor open
        // and validation immediately following this call remain authoritative.
        return Ok(());
    }
    Err(DeviceAuthorityLeaseError::io(
        "create device lease root",
        error,
    ))
}

fn validate_lease_root(directory: &File) -> Result<(), DeviceAuthorityLeaseError> {
    let metadata = directory
        .metadata()
        .map_err(|error| DeviceAuthorityLeaseError::io("inspect device lease root", error))?;
    if !metadata.is_dir()
        || metadata.st_uid() != effective_uid()
        || metadata.st_mode() & 0o7777 != 0o700
    {
        return Err(DeviceAuthorityLeaseError::security(
            DeviceLeaseSecurityObject::LeaseRoot,
        ));
    }
    require_close_on_exec(directory.as_raw_fd(), DeviceLeaseSecurityObject::LeaseRoot)
}

fn open_lock_file(root: &File, name: &str) -> Result<File, DeviceAuthorityLeaseError> {
    let name = CString::new(name)
        .map_err(|_| DeviceAuthorityLeaseError::security(DeviceLeaseSecurityObject::LockFile))?;
    let flags = libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
    };
    if descriptor >= 0 {
        let file = unsafe { File::from_raw_fd(descriptor) };
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(DeviceAuthorityLeaseError::io(
                "set device lease lock mode",
                io::Error::last_os_error(),
            ));
        }
        return Ok(file);
    }
    let create_error = io::Error::last_os_error();
    if create_error.kind() != io::ErrorKind::AlreadyExists {
        return Err(classify_lock_open_error(create_error));
    }

    let descriptor = unsafe { libc::openat(root.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(classify_lock_open_error(io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn validate_lock_file(file: &File) -> Result<(), DeviceAuthorityLeaseError> {
    let metadata = file
        .metadata()
        .map_err(|error| DeviceAuthorityLeaseError::io("inspect device lease lock file", error))?;
    if !metadata.is_file()
        || metadata.st_uid() != effective_uid()
        || metadata.st_mode() & 0o7777 != 0o600
        || metadata.st_nlink() != 1
    {
        return Err(DeviceAuthorityLeaseError::security(
            DeviceLeaseSecurityObject::LockFile,
        ));
    }
    require_close_on_exec(file.as_raw_fd(), DeviceLeaseSecurityObject::LockFile)
}

fn validate_lock_entry(
    root: &File,
    name: &str,
    file: &File,
) -> Result<(), DeviceAuthorityLeaseError> {
    let name = CString::new(name)
        .map_err(|_| DeviceAuthorityLeaseError::security(DeviceLeaseSecurityObject::LockFile))?;
    let entry = stat_at(root.as_raw_fd(), &name)?;
    let metadata = file
        .metadata()
        .map_err(|error| DeviceAuthorityLeaseError::io("reinspect device lease lock", error))?;
    if entry.st_mode & libc::S_IFMT != libc::S_IFREG
        || entry.st_uid != effective_uid()
        || entry.st_mode & 0o7777 != 0o600
        || entry.st_nlink != 1
        || entry.st_dev != metadata.st_dev()
        || entry.st_ino != metadata.st_ino()
    {
        return Err(DeviceAuthorityLeaseError::security(
            DeviceLeaseSecurityObject::LockFile,
        ));
    }
    Ok(())
}

fn stat_at(
    root_descriptor: RawFd,
    name: &CString,
) -> Result<libc::stat, DeviceAuthorityLeaseError> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            root_descriptor,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if is_security_path_error(&error) || error.kind() == io::ErrorKind::NotFound {
            return Err(DeviceAuthorityLeaseError::security(
                DeviceLeaseSecurityObject::LockFile,
            ));
        }
        return Err(DeviceAuthorityLeaseError::io(
            "inspect device lease directory entry",
            error,
        ));
    }
    Ok(unsafe { stat.assume_init() })
}

fn lock_exclusive_nonblocking(file: &File) -> Result<(), DeviceAuthorityLeaseError> {
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(DeviceAuthorityLeaseError::Busy);
        }
        return Err(DeviceAuthorityLeaseError::io(
            "acquire device authority lock",
            error,
        ));
    }
}

fn require_close_on_exec(
    descriptor: RawFd,
    object: DeviceLeaseSecurityObject,
) -> Result<(), DeviceAuthorityLeaseError> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(DeviceAuthorityLeaseError::io(
            "inspect device lease descriptor flags",
            io::Error::last_os_error(),
        ));
    }
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(DeviceAuthorityLeaseError::security(object));
    }
    Ok(())
}

fn classify_root_open_error(
    operation: &'static str,
    error: io::Error,
) -> DeviceAuthorityLeaseError {
    if is_security_path_error(&error) {
        DeviceAuthorityLeaseError::security(DeviceLeaseSecurityObject::LeaseRoot)
    } else if error.kind() == io::ErrorKind::NotFound {
        DeviceAuthorityLeaseError::InvalidLeaseRoot
    } else {
        DeviceAuthorityLeaseError::io(operation, error)
    }
}

fn classify_lock_open_error(error: io::Error) -> DeviceAuthorityLeaseError {
    if is_security_path_error(&error)
        || matches!(error.raw_os_error(), Some(libc::EISDIR | libc::ENXIO))
    {
        DeviceAuthorityLeaseError::security(DeviceLeaseSecurityObject::LockFile)
    } else {
        DeviceAuthorityLeaseError::io("open device lease lock file", error)
    }
}

fn is_security_path_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EACCES | libc::EPERM)
    )
}

fn os_string_to_cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
    use std::thread;

    use super::*;

    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    // The subprocess test necessarily forks the multithreaded test binary.
    // Serialize this small module so the pre-exec child cannot momentarily
    // inherit another test's CLOEXEC lease descriptor and extend that lock.
    static TEST_SERIALIZATION: Mutex<()> = Mutex::new(());
    const CHILD_ROOT_ENV: &str = "AGL_DEVICE_LEASE_TEST_CHILD_ROOT";
    const CHILD_READY: &str = "AGL_DEVICE_LEASE_TEST_READY";

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let parent = std::env::temp_dir().join(format!(
                "agl-device-lease-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            let root = parent.join("leases");
            Self { parent, root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn identity_is_bounded_normalized_and_never_used_as_a_path() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Fixture::new("identity");
        let lease = DeviceAuthorityLease::acquire(&fixture.root, "PCI:0000:03:00.0").unwrap();

        assert_eq!(lease.identity().normalized(), "pci:0000:03:00.0");
        assert_eq!(lease.identity().digest_hex().len(), 64);
        assert!(!lease.lock_path().to_string_lossy().contains("0000:03"));
        assert_eq!(lease.lease_root(), fixture.root);

        for invalid in ["", "device id", "device\n", "dévice"] {
            assert!(matches!(
                PhysicalDeviceLeaseIdentity::new(invalid),
                Err(DeviceAuthorityLeaseError::InvalidPhysicalDeviceIdentity)
            ));
        }
        assert!(matches!(
            PhysicalDeviceLeaseIdentity::new(&"x".repeat(MAX_PHYSICAL_DEVICE_ID_BYTES + 1)),
            Err(DeviceAuthorityLeaseError::InvalidPhysicalDeviceIdentity)
        ));
    }

    #[test]
    fn creates_exact_private_root_and_private_cloexec_lock_file() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Fixture::new("create");
        let lease = DeviceAuthorityLease::acquire(&fixture.root, "pci:0000:03:00.0").unwrap();

        let root_metadata = fs::symlink_metadata(&fixture.root).unwrap();
        assert!(root_metadata.is_dir());
        assert_eq!(root_metadata.mode() & 0o7777, 0o700);
        assert_eq!(root_metadata.uid(), effective_uid());

        let lock_metadata = fs::symlink_metadata(lease.lock_path()).unwrap();
        assert!(lock_metadata.is_file());
        assert_eq!(lock_metadata.mode() & 0o7777, 0o600);
        assert_eq!(lock_metadata.uid(), effective_uid());
        assert_eq!(lock_metadata.nlink(), 1);
        assert!(
            unsafe { libc::fcntl(lease._lock_file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC
                != 0
        );
        assert!(
            unsafe { libc::fcntl(lease._root_directory.as_raw_fd(), libc::F_GETFD) }
                & libc::FD_CLOEXEC
                != 0
        );
    }

    #[test]
    fn lock_is_nonblocking_exclusive_and_raii_releases_it() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Fixture::new("busy");
        let first = DeviceAuthorityLease::acquire(&fixture.root, "PCI:0000:03:00.0").unwrap();
        let error = DeviceAuthorityLease::acquire(&fixture.root, "pci:0000:03:00.0").unwrap_err();
        assert!(matches!(error, DeviceAuthorityLeaseError::Busy));
        assert_eq!(error.code(), "device_authority_busy");

        let other = DeviceAuthorityLease::acquire(&fixture.root, "pci:0000:04:00.0").unwrap();
        drop(other);
        drop(first);

        DeviceAuthorityLease::acquire(&fixture.root, "pci:0000:03:00.0").unwrap();
    }

    #[test]
    fn simultaneous_contenders_have_exactly_one_kernel_lock_winner() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const CONTENDERS: usize = 8;
        let fixture = Fixture::new("race");
        let start = Arc::new(Barrier::new(CONTENDERS + 1));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (sender, receiver) = mpsc::channel();
        let mut threads = Vec::new();

        for _ in 0..CONTENDERS {
            let root = fixture.root.clone();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let sender = sender.clone();
            threads.push(thread::spawn(move || {
                start.wait();
                match DeviceAuthorityLease::acquire(&root, "pci:0000:03:00.0") {
                    Ok(lease) => {
                        sender.send("acquired").unwrap();
                        let (lock, condition) = &*release;
                        let mut released = lock.lock().unwrap();
                        while !*released {
                            released = condition.wait(released).unwrap();
                        }
                        drop(lease);
                    }
                    Err(DeviceAuthorityLeaseError::Busy) => {
                        sender.send("busy").unwrap();
                    }
                    Err(error) => {
                        sender.send(error.code()).unwrap();
                    }
                }
            }));
        }
        drop(sender);
        start.wait();

        let results = receiver.iter().take(CONTENDERS).collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == "acquired")
                .count(),
            1,
            "unexpected contention results: {results:?}"
        );
        assert_eq!(
            results.iter().filter(|result| **result == "busy").count(),
            CONTENDERS - 1,
            "unexpected contention results: {results:?}"
        );

        let (lock, condition) = &*release;
        *lock.lock().unwrap() = true;
        condition.notify_all();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn separate_process_observes_busy_and_exit_releases_the_lease() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Fixture::new("process");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "device_lease::tests::device_lease_child_fixture",
                "--nocapture",
            ])
            .env(CHILD_ROOT_ENV, &fixture.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut child_input = child.stdin.take().unwrap();
        let mut child_output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = child_output.read_line(&mut line).unwrap();
            assert!(bytes != 0, "lease child exited before acquiring its lock");
            if line.contains(CHILD_READY) {
                break;
            }
        }

        let contention = DeviceAuthorityLease::acquire(&fixture.root, "pci:0000:03:00.0");
        child_input.write_all(b"release\n").unwrap();
        drop(child_input);
        let mut remaining_output = String::new();
        child_output.read_to_string(&mut remaining_output).unwrap();
        let status = child.wait().unwrap();

        assert!(status.success(), "lease child failed: {remaining_output}");
        assert!(matches!(contention, Err(DeviceAuthorityLeaseError::Busy)));
        DeviceAuthorityLease::acquire(&fixture.root, "pci:0000:03:00.0").unwrap();
    }

    #[test]
    fn device_lease_child_fixture() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(root) = std::env::var_os(CHILD_ROOT_ENV) else {
            return;
        };
        let _lease = DeviceAuthorityLease::acquire(root, "pci:0000:03:00.0").unwrap();
        println!("{CHILD_READY}");
        std::io::stdout().flush().unwrap();
        let mut release = [0_u8; 1];
        std::io::stdin().read_exact(&mut release).unwrap();
    }

    #[test]
    fn rejects_non_exact_or_missing_parent_roots() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Fixture::new("root-syntax");
        for root in [
            PathBuf::from("relative"),
            PathBuf::from("/"),
            fixture.parent.join("./leases"),
            fixture.parent.join("missing/leases"),
        ] {
            assert!(matches!(
                DeviceAuthorityLease::acquire(root, "device"),
                Err(DeviceAuthorityLeaseError::InvalidLeaseRoot)
            ));
        }
    }

    #[test]
    fn rejects_insecure_or_symlinked_root_without_repairing_it() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Fixture::new("root-security");
        fs::create_dir(&fixture.root).unwrap();
        fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = DeviceAuthorityLease::acquire(&fixture.root, "device").unwrap_err();
        assert!(matches!(
            error,
            DeviceAuthorityLeaseError::SecurityViolation {
                object: DeviceLeaseSecurityObject::LeaseRoot
            }
        ));
        assert_eq!(fs::metadata(&fixture.root).unwrap().mode() & 0o7777, 0o755);

        fs::remove_dir(&fixture.root).unwrap();
        let target = fixture.parent.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &fixture.root).unwrap();
        let error = DeviceAuthorityLease::acquire(&fixture.root, "device").unwrap_err();
        assert!(matches!(
            error,
            DeviceAuthorityLeaseError::SecurityViolation {
                object: DeviceLeaseSecurityObject::LeaseRoot
            }
        ));
        assert_eq!(error.code(), "device_authority_security");
    }

    #[test]
    fn rejects_symlink_nonregular_or_hardlinked_lock_entries() {
        let _serial = TEST_SERIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for case in ["symlink", "directory", "hardlink"] {
            let fixture = Fixture::new(case);
            fs::create_dir(&fixture.root).unwrap();
            fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o700)).unwrap();
            let identity = PhysicalDeviceLeaseIdentity::new("device").unwrap();
            let lock_path = fixture.root.join(identity.lock_file_name());

            match case {
                "symlink" => symlink(fixture.parent.join("target"), &lock_path).unwrap(),
                "directory" => {
                    fs::create_dir(&lock_path).unwrap();
                }
                "hardlink" => {
                    let other = fixture.parent.join("other-lock");
                    fs::write(&other, "").unwrap();
                    fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
                    fs::hard_link(&other, &lock_path).unwrap();
                }
                _ => unreachable!(),
            }

            let error = DeviceAuthorityLease::acquire(&fixture.root, "device").unwrap_err();
            assert!(matches!(
                error,
                DeviceAuthorityLeaseError::SecurityViolation {
                    object: DeviceLeaseSecurityObject::LockFile
                }
            ));
        }
    }
}
