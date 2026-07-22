//! Irreversible Linux sandbox for the native inference worker.
//!
//! The caller must invoke [`enter`] while the worker is still single-threaded,
//! after its private control socket has been inherited and before any native
//! runtime or device inventory is initialized. Any error from `enter` is fatal
//! for that worker process: resource limits or a Landlock domain may already be
//! active when a later kernel operation fails.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fmt;
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

use agl_inference::worker_protocol::SandboxConfiguration;

const MIN_LANDLOCK_ABI: u32 = 5;
const MAX_SANDBOX_PATH_BYTES: usize = 4096;
const MIN_OPEN_FILES: libc::rlim_t = 64;

const ADDRESS_SPACE_LIMIT: libc::rlim_t = 512 * 1024 * 1024 * 1024;
const FILE_SIZE_LIMIT: libc::rlim_t = 512 * 1024 * 1024;
const OPEN_FILE_LIMIT: libc::rlim_t = 256;
const PROCESS_AND_THREAD_LIMIT: libc::rlim_t = 4096;
const STACK_LIMIT: libc::rlim_t = 64 * 1024 * 1024;
const LOCKED_MEMORY_LIMIT: libc::rlim_t = 8 * 1024 * 1024 * 1024;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

const LANDLOCK_ACCESS_NET_BIND_TCP: u64 = 1 << 0;
const LANDLOCK_ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;

const LANDLOCK_HANDLED_FS: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;

const LANDLOCK_READ_ONLY_DIRECTORY: u64 =
    LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const LANDLOCK_PRIVATE_TEMP: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE;
const LANDLOCK_GPU_DEVICE: u64 =
    LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE | LANDLOCK_ACCESS_FS_IOCTL_DEV;

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxErrorCode {
    InvalidConfiguration,
    UnsupportedKernel,
    UnexpectedProcessState,
    KernelEnforcement,
}

impl SandboxErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::UnsupportedKernel => "unsupported_kernel",
            Self::UnexpectedProcessState => "unexpected_process_state",
            Self::KernelEnforcement => "kernel_enforcement",
        }
    }
}

#[derive(Debug)]
pub struct SandboxError {
    code: SandboxErrorCode,
    message: String,
}

impl SandboxError {
    fn new(code: SandboxErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn last_os_error(code: SandboxErrorCode, action: &str) -> Self {
        Self::new(
            code,
            format!("{action}: {}", std::io::Error::last_os_error()),
        )
    }

    pub const fn code(&self) -> SandboxErrorCode {
        self.code
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for SandboxError {}

pub type Result<T> = std::result::Result<T, SandboxError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedResourceLimits {
    pub address_space_bytes: u64,
    pub file_size_bytes: u64,
    pub open_files: u64,
    pub processes_and_threads: u64,
    pub stack_bytes: u64,
    pub locked_memory_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SandboxReport {
    pub landlock_abi: u32,
    pub seccomp_tsync: bool,
    pub resource_limits: AppliedResourceLimits,
}

/// Enter the production inference sandbox.
///
/// `control_fd` must be the sole non-standard descriptor inherited by the
/// worker and must identify an `AF_UNIX` `SOCK_SEQPACKET` socket. The returned
/// process can create ordinary threads, but cannot fork, exec, create sockets,
/// signal another process, or access paths outside the admitted objects.
pub fn enter(configuration: &SandboxConfiguration, control_fd: RawFd) -> Result<SandboxReport> {
    validate_platform()?;
    validate_same_uid_process()?;
    validate_single_threaded()?;
    validate_control_socket(control_fd)?;
    validate_descriptor_set(control_fd)?;
    validate_path_relationships(configuration)?;

    let prepared = PreparedLandlock::new(configuration)?;
    if unsafe { libc::fchdir(prepared.private_temp.as_raw_fd()) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to enter the private inference temp root",
        ));
    }
    unsafe {
        libc::umask(0o077);
    }

    let resource_limits = apply_resource_limits()?;
    disable_process_dumping()?;
    drop_process_capabilities()?;
    enforce_no_new_privileges()?;
    prepared.restrict_self()?;
    install_seccomp_filter(control_fd)?;

    Ok(SandboxReport {
        landlock_abi: prepared.abi,
        seccomp_tsync: true,
        resource_limits,
    })
}

fn validate_platform() -> Result<()> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(SandboxError::new(
            SandboxErrorCode::UnsupportedKernel,
            "the inference seccomp policy supports only x86_64 and aarch64 Linux",
        ))
    }
}

fn validate_same_uid_process() -> Result<()> {
    let mut real = 0;
    let mut effective = 0;
    let mut saved = 0;
    if unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnexpectedProcessState,
            "failed to inspect inference worker user IDs",
        ));
    }
    if real != effective || effective != saved {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            "inference worker requires identical real, effective, and saved user IDs",
        ));
    }

    let mut real_group = 0;
    let mut effective_group = 0;
    let mut saved_group = 0;
    if unsafe { libc::getresgid(&mut real_group, &mut effective_group, &mut saved_group) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnexpectedProcessState,
            "failed to inspect inference worker group IDs",
        ));
    }
    if real_group != effective_group || effective_group != saved_group {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            "inference worker requires identical real, effective, and saved group IDs",
        ));
    }
    Ok(())
}

fn validate_single_threaded() -> Result<()> {
    let count = directory_numeric_entry_count(c"/proc/self/task")?;
    if count != 1 {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            format!("inference sandbox entry requires one thread, found {count}"),
        ));
    }
    Ok(())
}

fn validate_descriptor_set(control_fd: RawFd) -> Result<()> {
    let directory = unsafe { libc::opendir(c"/proc/self/fd".as_ptr()) };
    if directory.is_null() {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnexpectedProcessState,
            "failed to inspect inference worker descriptors",
        ));
    }
    let inspection_fd = unsafe { libc::dirfd(directory) };
    let mut unexpected = None;
    loop {
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let Ok(name) = name.to_string_lossy().parse::<RawFd>() else {
            continue;
        };
        if name > 2 && name != control_fd && name != inspection_fd {
            unexpected = Some(name);
            break;
        }
    }
    unsafe {
        libc::closedir(directory);
    }
    if let Some(descriptor) = unexpected {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            format!("inference worker inherited unrelated descriptor {descriptor}"),
        ));
    }
    Ok(())
}

fn directory_numeric_entry_count(path: &CStr) -> Result<usize> {
    let directory = unsafe { libc::opendir(path.as_ptr()) };
    if directory.is_null() {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnexpectedProcessState,
            "failed to inspect inference worker task set",
        ));
    }
    let mut count = 0_usize;
    loop {
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes().iter().all(u8::is_ascii_digit) {
            count = count.saturating_add(1);
        }
    }
    unsafe {
        libc::closedir(directory);
    }
    Ok(count)
}

fn validate_control_socket(control_fd: RawFd) -> Result<()> {
    if control_fd <= 2 || unsafe { libc::fcntl(control_fd, libc::F_GETFD) } < 0 {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            "inference control descriptor is unavailable",
        ));
    }
    let mut socket_type = 0;
    let mut socket_type_len = mem::size_of::<libc::c_int>() as libc::socklen_t;
    let socket_type_result = unsafe {
        libc::getsockopt(
            control_fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::addr_of_mut!(socket_type).cast(),
            &mut socket_type_len,
        )
    };
    if socket_type_result != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnexpectedProcessState,
            "failed to inspect inference control socket type",
        ));
    }
    if socket_type != libc::SOCK_SEQPACKET {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            format!("inference control descriptor is not SOCK_SEQPACKET (SO_TYPE={socket_type})"),
        ));
    }
    let mut address = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut address_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { libc::getsockname(control_fd, address.as_mut_ptr().cast(), &mut address_len) } != 0
    {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnexpectedProcessState,
            "failed to inspect inference control socket",
        ));
    }
    let family = unsafe { address.assume_init().ss_family } as libc::c_int;
    if family != libc::AF_UNIX {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            "inference control descriptor is not AF_UNIX",
        ));
    }
    Ok(())
}

fn validate_path_relationships(configuration: &SandboxConfiguration) -> Result<()> {
    let private_temp = validate_lexical_path(Path::new(configuration.private_temp_root()))?;
    for path in configuration
        .model_roots()
        .iter()
        .chain(configuration.projector_roots())
        .chain(configuration.runtime_roots())
    {
        let path = validate_lexical_path(Path::new(path))?;
        if path == private_temp || path.starts_with(&private_temp) {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "a read-only inference root cannot be inside the writable temp root",
            ));
        }
    }
    Ok(())
}

struct PreparedLandlock {
    abi: u32,
    ruleset: OwnedFd,
    private_temp: OwnedFd,
}

impl PreparedLandlock {
    fn new(configuration: &SandboxConfiguration) -> Result<Self> {
        let abi = landlock_abi()?;
        if abi < MIN_LANDLOCK_ABI {
            return Err(SandboxError::new(
                SandboxErrorCode::UnsupportedKernel,
                format!("Linux Landlock ABI {abi} is below required ABI {MIN_LANDLOCK_ABI}"),
            ));
        }
        let attr = LandlockRulesetAttr {
            handled_access_fs: LANDLOCK_HANDLED_FS,
            handled_access_net: LANDLOCK_ACCESS_NET_BIND_TCP | LANDLOCK_ACCESS_NET_CONNECT_TCP,
        };
        let descriptor = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr,
                mem::size_of::<LandlockRulesetAttr>(),
                0,
            )
        } as RawFd;
        if descriptor < 0 {
            return Err(SandboxError::last_os_error(
                SandboxErrorCode::KernelEnforcement,
                "failed to create inference Landlock ruleset",
            ));
        }
        let ruleset = unsafe { OwnedFd::from_raw_fd(descriptor) };

        let mut identities = BTreeSet::new();
        for path in configuration
            .model_roots()
            .iter()
            .chain(configuration.projector_roots())
            .chain(configuration.runtime_roots())
        {
            let admitted = AdmittedPath::open_read_only(Path::new(path))?;
            if identities.insert(admitted.identity) {
                add_landlock_path(
                    ruleset.as_raw_fd(),
                    admitted.descriptor.as_raw_fd(),
                    admitted.allowed_read_access,
                )?;
            }
        }

        for path in configuration.gpu_device_paths() {
            let directory = AdmittedPath::open_gpu_directory(Path::new(path))?;
            if identities.insert(directory.identity) {
                add_landlock_path(
                    ruleset.as_raw_fd(),
                    directory.descriptor.as_raw_fd(),
                    LANDLOCK_ACCESS_FS_READ_DIR,
                )?;
            }
            let admitted = AdmittedPath::open_gpu(Path::new(path))?;
            if identities.insert(admitted.identity) {
                add_landlock_path(
                    ruleset.as_raw_fd(),
                    admitted.descriptor.as_raw_fd(),
                    LANDLOCK_GPU_DEVICE,
                )?;
            }
        }

        let private_temp =
            AdmittedPath::open_private_temp(Path::new(configuration.private_temp_root()))?;
        if !identities.insert(private_temp.identity) {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "private temp root aliases another admitted sandbox object",
            ));
        }
        add_landlock_path(
            ruleset.as_raw_fd(),
            private_temp.descriptor.as_raw_fd(),
            LANDLOCK_PRIVATE_TEMP,
        )?;

        Ok(Self {
            abi,
            ruleset,
            private_temp: private_temp.descriptor,
        })
    }

    fn restrict_self(&self) -> Result<()> {
        if unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0,
            )
        } != 0
        {
            return Err(SandboxError::last_os_error(
                SandboxErrorCode::KernelEnforcement,
                "failed to enter inference Landlock domain",
            ));
        }
        Ok(())
    }
}

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
    _reserved: u32,
}

fn landlock_abi() -> Result<u32> {
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<LandlockRulesetAttr>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::UnsupportedKernel,
            "Linux Landlock is unavailable",
        ));
    }
    u32::try_from(abi).map_err(|_| {
        SandboxError::new(
            SandboxErrorCode::UnsupportedKernel,
            "Linux returned an invalid Landlock ABI",
        )
    })
}

fn add_landlock_path(ruleset: RawFd, path: RawFd, allowed_access: u64) -> Result<()> {
    let attr = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd: path,
        _reserved: 0,
    };
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr,
            0,
        )
    } != 0
    {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to admit an inference Landlock path",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FileIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

struct AdmittedPath {
    descriptor: OwnedFd,
    identity: FileIdentity,
    allowed_read_access: u64,
}

impl AdmittedPath {
    fn open_gpu_directory(device_path: &Path) -> Result<Self> {
        let device_path = validate_lexical_path(device_path)?;
        if device_path.parent() != Some(Path::new("/dev/dri")) {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "GPU admission accepts only the exact /dev/dri directory",
            ));
        }
        let descriptor = open_path_without_symlinks(Path::new("/dev/dri"))?;
        let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "GPU render-node parent is not a directory",
            ));
        }
        Ok(Self::from_metadata(
            descriptor,
            metadata,
            LANDLOCK_ACCESS_FS_READ_DIR,
        ))
    }

    fn open_read_only(path: &Path) -> Result<Self> {
        reject_broad_read_root(path)?;
        let descriptor = open_path_without_symlinks(path)?;
        let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
        let file_type = metadata.st_mode & libc::S_IFMT;
        let allowed_read_access = if file_type == libc::S_IFDIR {
            LANDLOCK_READ_ONLY_DIRECTORY
        } else if file_type == libc::S_IFREG {
            LANDLOCK_ACCESS_FS_READ_FILE
        } else {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "read-only inference roots must be regular files or directories",
            ));
        };
        Ok(Self::from_metadata(
            descriptor,
            metadata,
            allowed_read_access,
        ))
    }

    fn open_private_temp(path: &Path) -> Result<Self> {
        let path = validate_lexical_path(path)?;
        if is_broad_path(&path) {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "private inference temp root is too broad",
            ));
        }
        let descriptor = open_path_without_symlinks(&path)?;
        let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "private inference temp root is not a directory",
            ));
        }
        if metadata.st_uid != unsafe { libc::geteuid() } {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "private inference temp root is not owned by the worker UID",
            ));
        }
        if metadata.st_mode & 0o7777 != 0o700 {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "private inference temp root must have exact mode 0700",
            ));
        }
        Ok(Self::from_metadata(
            descriptor,
            metadata,
            LANDLOCK_PRIVATE_TEMP,
        ))
    }

    fn open_gpu(path: &Path) -> Result<Self> {
        let path = validate_lexical_path(path)?;
        let parent_ok = path.parent() == Some(Path::new("/dev/dri"));
        let name_ok = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("renderD"))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            });
        if !parent_ok || !name_ok {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "GPU admission accepts only exact /dev/dri/renderD<N> paths",
            ));
        }
        let descriptor = open_path_without_symlinks(&path)?;
        let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFCHR || libc::major(metadata.st_rdev) != 226
        {
            return Err(SandboxError::new(
                SandboxErrorCode::InvalidConfiguration,
                "admitted GPU path is not a DRM render device",
            ));
        }
        Ok(Self::from_metadata(
            descriptor,
            metadata,
            LANDLOCK_GPU_DEVICE,
        ))
    }

    fn from_metadata(descriptor: OwnedFd, metadata: libc::stat, allowed_read_access: u64) -> Self {
        Self {
            descriptor,
            identity: FileIdentity {
                device: metadata.st_dev,
                inode: metadata.st_ino,
            },
            allowed_read_access,
        }
    }
}

fn reject_broad_read_root(path: &Path) -> Result<()> {
    let path = validate_lexical_path(path)?;
    if is_broad_path(&path) {
        return Err(SandboxError::new(
            SandboxErrorCode::InvalidConfiguration,
            "read-only inference root is too broad",
        ));
    }
    Ok(())
}

fn is_broad_path(path: &Path) -> bool {
    [
        "/",
        "/dev",
        "/etc",
        "/home",
        "/nix/store",
        "/proc",
        "/root",
        "/run",
        "/sys",
        "/tmp",
        "/usr",
        "/var",
    ]
    .iter()
    .any(|broad| path == Path::new(broad))
}

fn validate_lexical_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.as_os_str().as_bytes().len() > MAX_SANDBOX_PATH_BYTES {
        return Err(SandboxError::new(
            SandboxErrorCode::InvalidConfiguration,
            "sandbox path must be a bounded absolute path",
        ));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(SandboxError::new(
            SandboxErrorCode::InvalidConfiguration,
            "sandbox path cannot contain dot, parent, or platform-prefix components",
        ));
    }
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(SandboxError::new(
            SandboxErrorCode::InvalidConfiguration,
            "sandbox path contains a NUL byte",
        ));
    }
    Ok(path.to_path_buf())
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn open_path_without_symlinks(path: &Path) -> Result<OwnedFd> {
    let path = validate_lexical_path(path)?;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        SandboxError::new(
            SandboxErrorCode::InvalidConfiguration,
            "sandbox path contains a NUL byte",
        )
    })?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            mem::size_of::<OpenHow>(),
        )
    } as RawFd;
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        let code = if error.raw_os_error() == Some(libc::ENOSYS) {
            SandboxErrorCode::UnsupportedKernel
        } else {
            SandboxErrorCode::InvalidConfiguration
        };
        return Err(SandboxError::new(
            code,
            format!("failed to pin a symlink-free sandbox path: {error}"),
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn descriptor_metadata(descriptor: RawFd) -> Result<libc::stat> {
    let mut metadata = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::InvalidConfiguration,
            "failed to inspect an admitted sandbox path",
        ));
    }
    Ok(unsafe { metadata.assume_init() })
}

fn apply_resource_limits() -> Result<AppliedResourceLimits> {
    set_exact_limit(libc::RLIMIT_CORE, 0)?;
    let address_space_bytes = clamp_limit(libc::RLIMIT_AS, ADDRESS_SPACE_LIMIT)?;
    let file_size_bytes = clamp_limit(libc::RLIMIT_FSIZE, FILE_SIZE_LIMIT)?;
    let open_files = clamp_limit(libc::RLIMIT_NOFILE, OPEN_FILE_LIMIT)?;
    if open_files < MIN_OPEN_FILES {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            format!("worker open-file hard limit {open_files} is below {MIN_OPEN_FILES}"),
        ));
    }
    let processes_and_threads = clamp_limit(libc::RLIMIT_NPROC, PROCESS_AND_THREAD_LIMIT)?;
    let stack_bytes = clamp_limit(libc::RLIMIT_STACK, STACK_LIMIT)?;
    let locked_memory_bytes = clamp_limit(libc::RLIMIT_MEMLOCK, LOCKED_MEMORY_LIMIT)?;
    Ok(AppliedResourceLimits {
        address_space_bytes,
        file_size_bytes,
        open_files,
        processes_and_threads,
        stack_bytes,
        locked_memory_bytes,
    })
}

fn set_exact_limit(resource: libc::__rlimit_resource_t, value: libc::rlim_t) -> Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to set an inference worker resource limit",
        ));
    }
    Ok(())
}

fn clamp_limit(resource: libc::__rlimit_resource_t, cap: libc::rlim_t) -> Result<u64> {
    let mut current = MaybeUninit::<libc::rlimit>::zeroed();
    if unsafe { libc::getrlimit(resource, current.as_mut_ptr()) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to read an inference worker resource limit",
        ));
    }
    let current = unsafe { current.assume_init() };
    let maximum = if current.rlim_max == libc::RLIM_INFINITY {
        cap
    } else {
        current.rlim_max.min(cap)
    };
    let soft = if current.rlim_cur == libc::RLIM_INFINITY {
        maximum
    } else {
        current.rlim_cur.min(maximum)
    };
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: maximum,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to clamp an inference worker resource limit",
        ));
    }
    Ok(soft)
}

fn disable_process_dumping() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to disable inference worker dumps",
        ));
    }
    Ok(())
}

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn drop_process_capabilities() -> Result<()> {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let mut header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let empty = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capset, &mut header, empty.as_ptr()) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to clear inference worker capabilities",
        ));
    }
    if unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    } != 0
    {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to clear inference worker ambient capabilities",
        ));
    }
    Ok(())
}

fn enforce_no_new_privileges() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to enforce no_new_privs for inference worker",
        ));
    }
    Ok(())
}

fn install_seccomp_filter(control_fd: RawFd) -> Result<()> {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_JMP_JSET_K: u16 = 0x45;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_SET_MODE_FILTER: u32 = 1;
    const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;
    const NUMBER_OFFSET: u32 = 0;
    const ARCH_OFFSET: u32 = 4;
    const ARGUMENT_ZERO_OFFSET: u32 = 16;

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    fn statement(code: u16, value: u32) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k: value,
        }
    }

    fn jump(code: u16, value: u32, yes: u8, no: u8) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: yes,
            jf: no,
            k: value,
        }
    }

    fn filtered_argument(
        filters: &mut Vec<libc::sock_filter>,
        syscall: libc::c_long,
        expected: u32,
        allow: u32,
        deny: u32,
    ) {
        filters.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 4));
        filters.push(statement(BPF_LD_W_ABS, ARGUMENT_ZERO_OFFSET));
        filters.push(jump(BPF_JMP_JEQ_K, expected, 0, 1));
        filters.push(statement(BPF_RET_K, allow));
        filters.push(statement(BPF_RET_K, deny));
    }

    fn filtered_argument_pair(
        filters: &mut Vec<libc::sock_filter>,
        syscall: libc::c_long,
        first: u32,
        second: u32,
        allow: u32,
        deny: u32,
    ) {
        filters.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 5));
        filters.push(statement(BPF_LD_W_ABS, ARGUMENT_ZERO_OFFSET));
        filters.push(jump(BPF_JMP_JEQ_K, first, 1, 0));
        filters.push(jump(BPF_JMP_JEQ_K, second, 0, 1));
        filters.push(statement(BPF_RET_K, allow));
        filters.push(statement(BPF_RET_K, deny));
    }

    fn filtered_prctl(filters: &mut Vec<libc::sock_filter>, allow: u32, deny: u32) {
        // `prctl` is a multiplexed authority surface. Keep only read-only
        // hardening probes and thread naming after sandbox entry. In
        // particular, PR_SET_PDEATHSIG must never be available to native code,
        // because clearing it would let the worker outlive its supervisor.
        filters.push(jump(BPF_JMP_JEQ_K, libc::SYS_prctl as u32, 0, 8));
        filters.push(statement(BPF_LD_W_ABS, ARGUMENT_ZERO_OFFSET));
        filters.push(jump(BPF_JMP_JEQ_K, libc::PR_GET_DUMPABLE as u32, 5, 0));
        filters.push(jump(BPF_JMP_JEQ_K, libc::PR_GET_NO_NEW_PRIVS as u32, 4, 0));
        filters.push(jump(BPF_JMP_JEQ_K, libc::PR_GET_PDEATHSIG as u32, 3, 0));
        filters.push(jump(BPF_JMP_JEQ_K, libc::PR_SET_NAME as u32, 2, 0));
        filters.push(jump(BPF_JMP_JEQ_K, libc::PR_GET_NAME as u32, 1, 0));
        filters.push(statement(BPF_RET_K, deny));
        filters.push(statement(BPF_RET_K, allow));
        filters.push(statement(BPF_LD_W_ABS, NUMBER_OFFSET));
    }

    let denied = SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let unsupported = SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
    // Descriptor validation above proves that the inherited control socket is
    // the sole descriptor above stderr. After sandbox entry the service splits
    // it with F_DUPFD_CLOEXEC(min=3), so this is the exact future event-sender
    // descriptor once the temporary Landlock descriptors have been closed.
    let event_sender_fd = if control_fd == libc::STDERR_FILENO + 1 {
        control_fd + 1
    } else {
        libc::STDERR_FILENO + 1
    };
    let mut filters = vec![
        statement(BPF_LD_W_ABS, ARCH_OFFSET),
        jump(BPF_JMP_JEQ_K, AUDIT_ARCH, 1, 0),
        statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        statement(BPF_LD_W_ABS, NUMBER_OFFSET),
        // clone is admitted only for ordinary threads, never for a process or
        // namespace. clone3 cannot be inspected by classic BPF, so libc gets
        // ENOSYS and falls back to the filtered clone syscall.
        jump(BPF_JMP_JEQ_K, libc::SYS_clone as u32, 0, 6),
        statement(BPF_LD_W_ABS, ARGUMENT_ZERO_OFFSET),
        jump(
            BPF_JMP_JSET_K,
            (libc::CLONE_NEWCGROUP
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWUSER
                | libc::CLONE_NEWUTS) as u32,
            2,
            0,
        ),
        jump(BPF_JMP_JSET_K, libc::CLONE_THREAD as u32, 0, 1),
        statement(BPF_RET_K, SECCOMP_RET_ALLOW),
        statement(BPF_RET_K, denied),
        statement(BPF_LD_W_ABS, NUMBER_OFFSET),
        jump(BPF_JMP_JEQ_K, libc::SYS_clone3 as u32, 0, 1),
        statement(BPF_RET_K, unsupported),
    ];

    filtered_argument_pair(
        &mut filters,
        libc::SYS_sendmsg,
        control_fd as u32,
        event_sender_fd as u32,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_argument(
        &mut filters,
        libc::SYS_recvmsg,
        control_fd as u32,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_argument(
        &mut filters,
        libc::SYS_getsockopt,
        event_sender_fd as u32,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_argument(
        &mut filters,
        libc::SYS_getpeername,
        event_sender_fd as u32,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_argument(
        &mut filters,
        libc::SYS_shutdown,
        event_sender_fd as u32,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_argument(
        &mut filters,
        libc::SYS_tgkill,
        unsafe { libc::getpid() } as u32,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_argument(
        &mut filters,
        libc::SYS_prlimit64,
        0,
        SECCOMP_RET_ALLOW,
        denied,
    );
    filtered_prctl(&mut filters, SECCOMP_RET_ALLOW, denied);

    for &syscall in allowed_syscalls() {
        filters.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
        filters.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));
    }
    filters.push(statement(BPF_RET_K, denied));

    let mut program = libc::sock_fprog {
        len: u16::try_from(filters.len()).map_err(|_| {
            SandboxError::new(
                SandboxErrorCode::KernelEnforcement,
                "inference seccomp program exceeded the kernel instruction bound",
            )
        })?,
        filter: filters.as_mut_ptr(),
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &mut program,
        )
    };
    if result < 0 {
        return Err(SandboxError::last_os_error(
            SandboxErrorCode::KernelEnforcement,
            "failed to install inference seccomp allowlist",
        ));
    }
    if result != 0 {
        return Err(SandboxError::new(
            SandboxErrorCode::UnexpectedProcessState,
            format!("seccomp TSYNC rejected worker thread {result}"),
        ));
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn allowed_syscalls() -> &'static [libc::c_long] {
    &[
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_lseek,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_brk,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_ioctl,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_poll,
        libc::SYS_select,
        libc::SYS_sched_yield,
        libc::SYS_mremap,
        libc::SYS_msync,
        libc::SYS_mincore,
        libc::SYS_madvise,
        libc::SYS_dup,
        libc::SYS_dup2,
        libc::SYS_nanosleep,
        libc::SYS_getitimer,
        libc::SYS_alarm,
        libc::SYS_setitimer,
        libc::SYS_getpid,
        libc::SYS_sendfile,
        libc::SYS_exit,
        libc::SYS_wait4,
        libc::SYS_uname,
        libc::SYS_fcntl,
        libc::SYS_flock,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        libc::SYS_ftruncate,
        libc::SYS_getcwd,
        libc::SYS_getdents64,
        libc::SYS_readlink,
        libc::SYS_getrlimit,
        libc::SYS_getrusage,
        libc::SYS_sysinfo,
        libc::SYS_times,
        libc::SYS_getuid,
        libc::SYS_getgid,
        libc::SYS_geteuid,
        libc::SYS_getegid,
        libc::SYS_getppid,
        libc::SYS_gettid,
        libc::SYS_futex,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_getparam,
        libc::SYS_sched_getscheduler,
        libc::SYS_set_tid_address,
        libc::SYS_restart_syscall,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_clock_nanosleep,
        libc::SYS_exit_group,
        libc::SYS_epoll_wait,
        libc::SYS_epoll_ctl,
        libc::SYS_openat,
        // The Vulkan loader probes its explicit manifest and driver paths with
        // access(2) before opening them. Landlock remains the path-authority
        // boundary for the probe and all subsequent reads.
        libc::SYS_access,
        // glibc implements stat(2) with newfstatat(2). llama.cpp uses it to
        // discover backends inside the exact content-addressed runtime root;
        // Landlock continues to gate every subsequent file read/open.
        libc::SYS_newfstatat,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_symlinkat,
        libc::SYS_readlinkat,
        libc::SYS_faccessat,
        libc::SYS_pselect6,
        libc::SYS_ppoll,
        libc::SYS_set_robust_list,
        libc::SYS_get_robust_list,
        libc::SYS_splice,
        libc::SYS_tee,
        libc::SYS_sync_file_range,
        libc::SYS_utimensat,
        libc::SYS_epoll_pwait,
        libc::SYS_signalfd4,
        libc::SYS_eventfd2,
        libc::SYS_epoll_create1,
        libc::SYS_dup3,
        libc::SYS_pipe2,
        libc::SYS_inotify_init1,
        libc::SYS_preadv,
        libc::SYS_pwritev,
        libc::SYS_prlimit64,
        libc::SYS_getrandom,
        libc::SYS_memfd_create,
        libc::SYS_membarrier,
        libc::SYS_mlock2,
        libc::SYS_copy_file_range,
        libc::SYS_preadv2,
        libc::SYS_pwritev2,
        libc::SYS_statx,
        libc::SYS_rseq,
        libc::SYS_close_range,
        libc::SYS_openat2,
        libc::SYS_faccessat2,
        libc::SYS_epoll_pwait2,
        libc::SYS_futex_waitv,
        libc::SYS_set_mempolicy,
        libc::SYS_get_mempolicy,
        libc::SYS_mbind,
        libc::SYS_mlock,
        libc::SYS_munlock,
        libc::SYS_mlockall,
        libc::SYS_munlockall,
        libc::SYS_sigaltstack,
        libc::SYS_arch_prctl,
        libc::SYS_setrlimit,
        libc::SYS_gettimeofday,
        libc::SYS_time,
        libc::SYS_getcpu,
        libc::SYS_umask,
        libc::SYS_renameat2,
    ]
}

#[cfg(target_arch = "aarch64")]
fn allowed_syscalls() -> &'static [libc::c_long] {
    &[
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_lseek,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_brk,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_ioctl,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_ppoll,
        libc::SYS_pselect6,
        libc::SYS_sched_yield,
        libc::SYS_mremap,
        libc::SYS_msync,
        libc::SYS_mincore,
        libc::SYS_madvise,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_nanosleep,
        libc::SYS_getitimer,
        libc::SYS_setitimer,
        libc::SYS_getpid,
        libc::SYS_sendfile,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_uname,
        libc::SYS_fcntl,
        libc::SYS_flock,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        libc::SYS_ftruncate,
        libc::SYS_getcwd,
        libc::SYS_getdents64,
        libc::SYS_readlinkat,
        libc::SYS_getrusage,
        libc::SYS_sysinfo,
        libc::SYS_times,
        libc::SYS_getuid,
        libc::SYS_getgid,
        libc::SYS_geteuid,
        libc::SYS_getegid,
        libc::SYS_getppid,
        libc::SYS_gettid,
        libc::SYS_futex,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_getparam,
        libc::SYS_sched_getscheduler,
        libc::SYS_set_tid_address,
        libc::SYS_restart_syscall,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_clock_nanosleep,
        libc::SYS_epoll_pwait,
        libc::SYS_epoll_ctl,
        libc::SYS_openat,
        // glibc implements stat(2) with newfstatat(2). llama.cpp uses it to
        // discover backends inside the exact content-addressed runtime root;
        // Landlock continues to gate every subsequent file read/open.
        libc::SYS_newfstatat,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_symlinkat,
        libc::SYS_readlinkat,
        libc::SYS_faccessat,
        libc::SYS_set_robust_list,
        libc::SYS_get_robust_list,
        libc::SYS_splice,
        libc::SYS_tee,
        libc::SYS_sync_file_range,
        libc::SYS_utimensat,
        libc::SYS_signalfd4,
        libc::SYS_eventfd2,
        libc::SYS_epoll_create1,
        libc::SYS_pipe2,
        libc::SYS_inotify_init1,
        libc::SYS_preadv,
        libc::SYS_pwritev,
        libc::SYS_getrandom,
        libc::SYS_memfd_create,
        libc::SYS_membarrier,
        libc::SYS_mlock2,
        libc::SYS_copy_file_range,
        libc::SYS_preadv2,
        libc::SYS_pwritev2,
        libc::SYS_statx,
        libc::SYS_rseq,
        libc::SYS_close_range,
        libc::SYS_openat2,
        libc::SYS_faccessat2,
        libc::SYS_epoll_pwait2,
        libc::SYS_futex_waitv,
        libc::SYS_set_mempolicy,
        libc::SYS_get_mempolicy,
        libc::SYS_mbind,
        libc::SYS_mlock,
        libc::SYS_munlock,
        libc::SYS_mlockall,
        libc::SYS_munlockall,
        libc::SYS_sigaltstack,
        libc::SYS_setrlimit,
        libc::SYS_gettimeofday,
        libc::SYS_getcpu,
        libc::SYS_umask,
        libc::SYS_renameat2,
    ]
}
