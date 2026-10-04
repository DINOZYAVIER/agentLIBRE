//! Fail-closed Linux sandbox for the private llama-server process.

use std::fs::File;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;
const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SYM: u64 = 1 << 12;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;
const IOCTL_DEV: u64 = 1 << 15;
const BIND_TCP: u64 = 1 << 0;
const CONNECT_TCP: u64 = 1 << 1;
const HANDLED_FS: u64 = EXECUTE
    | WRITE_FILE
    | READ_FILE
    | READ_DIR
    | REMOVE_DIR
    | REMOVE_FILE
    | MAKE_DIR
    | MAKE_REG
    | MAKE_SYM
    | REFER
    | TRUNCATE
    | IOCTL_DEV;
const READ_TREE: u64 = READ_FILE | READ_DIR;
const RUNTIME_TREE: u64 = READ_TREE | EXECUTE;
const PRIVATE_TREE: u64 = READ_TREE
    | WRITE_FILE
    | REMOVE_DIR
    | REMOVE_FILE
    | MAKE_DIR
    | MAKE_REG
    | MAKE_SYM
    | REFER
    | TRUNCATE;
const DEVICE: u64 = READ_FILE | WRITE_FILE | IOCTL_DEV;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
    reserved: u32,
}

pub(crate) struct PreparedSandbox {
    ruleset: OwnedFd,
    _paths: Vec<OwnedFd>,
}

impl PreparedSandbox {
    pub(crate) fn prepare(
        executable_path: &Path,
        executable: &File,
        models: &[&File],
        private_directory: &Path,
        device_paths: &[std::path::PathBuf],
    ) -> std::io::Result<Self> {
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 5 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("private inference requires Landlock ABI 5; kernel returned {abi}"),
            ));
        }
        let attr = RulesetAttr {
            handled_access_fs: HANDLED_FS,
            handled_access_net: BIND_TCP | CONNECT_TCP,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr,
                mem::size_of::<RulesetAttr>(),
                0,
            )
        } as RawFd;
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut paths = Vec::new();
        add_fd(&ruleset, executable.as_raw_fd(), READ_FILE | EXECUTE)?;
        for model in models {
            add_fd(&ruleset, model.as_raw_fd(), READ_FILE)?;
        }
        if let Some(parent) = executable_path.parent() {
            add_path(&ruleset, &mut paths, parent, RUNTIME_TREE)?;
        }
        add_path(&ruleset, &mut paths, private_directory, PRIVATE_TREE)?;
        let dri = Path::new("/dev/dri");
        if dri.exists() {
            add_path(&ruleset, &mut paths, dri, READ_DIR)?;
        }
        // Device discovery needs read-only sysfs metadata, but opening and
        // issuing ioctls remains limited to the explicit character devices
        // admitted below.
        for path in [
            "/nix/store",
            "/lib",
            "/lib64",
            "/usr/lib",
            "/usr/lib64",
            "/run/opengl-driver",
            "/etc/ld.so.cache",
            "/etc/vulkan",
            "/usr/share/vulkan",
            "/proc/self/fd",
            "/proc/self/exe",
            "/proc/cpuinfo",
            "/proc/meminfo",
            "/sys/bus/pci",
            "/sys/class/drm",
            "/sys/dev/char",
            "/sys/devices",
            "/dev/null",
            "/dev/urandom",
        ] {
            let path = Path::new(path);
            if path.exists() {
                let access = if path.is_dir() {
                    if path.starts_with("/proc") || path.starts_with("/sys") {
                        READ_TREE
                    } else {
                        RUNTIME_TREE
                    }
                } else {
                    READ_FILE
                };
                add_path(&ruleset, &mut paths, path, access)?;
            }
        }
        for path in device_paths {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "inference device authority must name a character device",
                ));
            }
            add_path(&ruleset, &mut paths, path, DEVICE)?;
        }
        Ok(Self {
            ruleset,
            _paths: paths,
        })
    }

    pub(crate) fn enter(&self) -> std::io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
            || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

fn add_path(
    ruleset: &OwnedFd,
    paths: &mut Vec<OwnedFd>,
    path: &Path,
    access: u64,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox path contains NUL",
        )
    })?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    add_fd(ruleset, fd.as_raw_fd(), access)?;
    paths.push(fd);
    Ok(())
}

fn add_fd(ruleset: &OwnedFd, fd: RawFd, access: u64) -> std::io::Result<()> {
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: fd,
        reserved: 0,
    };
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &attr,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
