use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::{ProcessPlatformDiagnostics, STANDARD_RUNTIME_ROOTS};
use agl_exec::{ExecutionProfile, ProcessError, ProcessErrorCode, Result};

use super::super::LauncherRequest;
use super::{SANDBOX_HOME, SANDBOX_TMP, last_os_error};

const PRIVATE_DEVICE_PATHS: &[&str] = &["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];

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
const LANDLOCK_READ_ACCESS: u64 =
    LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const LANDLOCK_DEVICE_ACCESS: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE;
const LANDLOCK_WRITE_ACCESS: u64 = LANDLOCK_READ_ACCESS
    | LANDLOCK_ACCESS_FS_WRITE_FILE
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
    | LANDLOCK_ACCESS_FS_TRUNCATE;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
    _reserved: u32,
}

#[repr(C)]
#[derive(Default)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;
const AT_RECURSIVE: u32 = 0x8000;
const AT_EMPTY_PATH: u32 = 0x1000;

pub(super) fn enter_namespaces(profile: ExecutionProfile) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    unshare(libc::CLONE_NEWUSER, "user namespace")?;
    install_identity_map(uid, gid)?;
    let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID;
    if profile == ExecutionProfile::Workspace {
        flags |= libc::CLONE_NEWNET;
    }
    unshare(flags, "mount/PID/network namespaces")?;
    mount_raw(
        None,
        Path::new("/"),
        None,
        (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
        None,
        "failed to make mount propagation private",
    )
}

pub(super) fn validate_executable_admission(request: &LauncherRequest) -> Result<()> {
    if request.request.profile == ExecutionProfile::Host {
        return Ok(());
    }
    let program = request.request.program.canonicalize().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            format!("workspace executable cannot be resolved: {error}"),
        )
    })?;
    if program != request.request.program {
        return Err(ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            "workspace executable path changed after admission",
        ));
    }
    let metadata = fs::metadata(&program).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            format!("workspace executable cannot be inspected: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            "workspace executable is not a regular file",
        ));
    }

    let admitted = std::iter::once(request.request.workspace_root.as_path())
        .chain(request.request.read_only_roots.iter().map(PathBuf::as_path))
        .any(|root| program.starts_with(root))
        || STANDARD_RUNTIME_ROOTS.iter().any(|root| {
            Path::new(root)
                .canonicalize()
                .is_ok_and(|root| program.starts_with(root))
        });
    if admitted {
        Ok(())
    } else {
        Err(ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            "workspace executable is outside the workspace and admitted runtime roots; configure execution.runtime_read_only_roots or request the host profile",
        ))
    }
}

pub(super) fn prepare_pid_namespace(request: &LauncherRequest) -> Result<Option<PathBuf>> {
    if request.request.profile == ExecutionProfile::Host {
        return Ok(None);
    }

    let root = request.execution_root.join("rootfs");
    ensure_directory(&request.execution_root, 0o700)?;
    ensure_directory(&request.private_home, 0o700)?;
    ensure_directory(&request.private_tmp, 0o700)?;
    ensure_directory(&root, 0o700)?;
    mount_raw(
        Some(Path::new("tmpfs")),
        &root,
        Some("tmpfs"),
        (libc::MS_NOSUID | libc::MS_NODEV) as libc::c_ulong,
        Some("mode=0755,size=67108864"),
        "failed to create the workspace root filesystem",
    )?;

    // Install private runtime paths before admitted nested mounts. A runtime
    // root or workspace may live below /tmp; mounting private /tmp afterwards
    // would hide those exact admitted paths.
    bind_mount_at(
        &request.private_tmp,
        &root.join(SANDBOX_TMP.trim_start_matches('/')),
        false,
    )?;
    bind_mount_at(
        &request.private_home,
        &root.join(SANDBOX_HOME.trim_start_matches('/')),
        false,
    )?;

    let mut read_only = BTreeSet::new();
    for candidate in STANDARD_RUNTIME_ROOTS {
        let path = PathBuf::from(candidate);
        if path.exists() {
            read_only.insert(path);
        }
    }
    read_only.extend(request.request.read_only_roots.iter().cloned());
    for source in read_only {
        bind_mount(&source, &root, true)?;
    }
    for source in [
        "/etc/ld.so.cache",
        "/etc/nsswitch.conf",
        "/etc/passwd",
        "/etc/group",
        "/etc/hosts",
        "/etc/localtime",
    ] {
        let source = Path::new(source);
        if source.exists() {
            bind_mount(source, &root, true)?;
        }
    }

    bind_mount(
        &request.request.workspace_root,
        &root,
        !request.request.authorization.workspace_write,
    )?;

    let dev = root.join("dev");
    ensure_directory(&dev, 0o755)?;
    mount_raw(
        Some(Path::new("tmpfs")),
        &dev,
        Some("tmpfs"),
        (libc::MS_NOSUID | libc::MS_NOEXEC) as libc::c_ulong,
        Some("mode=0755,size=1048576"),
        "failed to create private /dev",
    )?;
    for source in PRIVATE_DEVICE_PATHS {
        if Path::new(source).exists() {
            bind_mount_at(
                Path::new(source),
                &root.join(source.trim_start_matches('/')),
                false,
            )?;
        }
    }
    std::os::unix::fs::symlink("/proc/self/fd", root.join("dev/fd")).map_err(|error| {
        sandbox_io(
            "failed to expose private /dev/fd for admitted script execution",
            error,
        )
    })?;

    let proc = root.join("proc");
    ensure_directory(&proc, 0o555)?;
    mount_raw(
        Some(Path::new("proc")),
        &proc,
        Some("proc"),
        (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong,
        None,
        "failed to mount private /proc",
    )?;
    Ok(Some(root))
}

pub(super) fn enter_target(
    request: &LauncherRequest,
    root: Option<&Path>,
    admitted_cwd: RawFd,
    admitted_program: RawFd,
) -> Result<OwnedFd> {
    apply_resource_limits()?;
    let program = if request.request.profile == ExecutionProfile::Workspace {
        let root = root.ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::SandboxUnavailable,
                "workspace root filesystem was not constructed",
            )
        })?;
        chroot(root)?;
        change_directory(&request.request.cwd, admitted_cwd)?;
        let program =
            verified_path_descriptor(&request.request.program, admitted_program, "executable")?;
        apply_landlock(request)?;
        drop_capabilities()?;
        apply_seccomp(true)?;
        program
    } else {
        change_directory(&request.request.cwd, admitted_cwd)?;
        let program =
            verified_path_descriptor(&request.request.program, admitted_program, "executable")?;
        drop_capabilities()?;
        apply_seccomp(false)?;
        program
    };
    Ok(program)
}

pub(super) fn diagnostics() -> ProcessPlatformDiagnostics {
    let landlock_abi = landlock_abi().ok();
    let pty = probe_pty();
    let pidfd = probe_pidfd();
    let user_namespace = probe_namespace(0);
    let mount_namespace = probe_namespace(libc::CLONE_NEWNS);
    let pid_namespace = probe_namespace(libc::CLONE_NEWPID);
    let network_namespace = probe_namespace(libc::CLONE_NEWNET);
    let execveat = probe_execveat();
    let namespaces = user_namespace
        && mount_namespace
        && pid_namespace
        && network_namespace
        && probe_namespaces();
    let seccomp = unsafe { libc::prctl(libc::PR_GET_SECCOMP) } >= 0;
    let supported = namespaces
        && landlock_abi.is_some_and(|abi| abi >= 3)
        && seccomp
        && pidfd
        && pty
        && execveat;
    ProcessPlatformDiagnostics {
        platform: "linux".to_owned(),
        supported,
        launcher: true,
        user_namespace,
        pid_namespace,
        mount_namespace,
        network_namespace,
        landlock_abi,
        seccomp,
        pidfd,
        pty,
        error_code: (!supported).then(|| ProcessErrorCode::SandboxUnavailable.as_str().to_owned()),
        remediation: (!supported).then(|| {
            "enable unprivileged user/PID/mount/network namespaces and Linux Landlock ABI 3+, seccomp, pidfd, execveat, and PTY support".to_owned()
        }),
    }
}

fn install_identity_map(uid: libc::uid_t, gid: libc::gid_t) -> Result<()> {
    match fs::write("/proc/self/setgroups", "deny\n") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(sandbox_io("failed to deny setgroups", error)),
    }
    fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))
        .map_err(|error| sandbox_io("failed to install the user namespace UID map", error))?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))
        .map_err(|error| sandbox_io("failed to install the user namespace GID map", error))?;
    Ok(())
}

fn bind_mount(source: &Path, root: &Path, read_only: bool) -> Result<()> {
    let relative = source.strip_prefix("/").map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            "sandbox bind source must be absolute",
        )
    })?;
    bind_mount_at(source, &root.join(relative), read_only)
}

fn bind_mount_at(source: &Path, target: &Path, read_only: bool) -> Result<()> {
    let metadata = fs::metadata(source).map_err(|error| {
        sandbox_io(
            &format!("sandbox runtime root {} is unavailable", source.display()),
            error,
        )
    })?;
    if metadata.is_dir() {
        ensure_directory(target, 0o755)?;
    } else if metadata.is_file() || metadata.file_type().is_char_device() {
        if let Some(parent) = target.parent() {
            ensure_directory(parent, 0o755)?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(target) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(sandbox_io("failed to create bind target", error)),
        }
    } else {
        return Err(ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            format!(
                "sandbox runtime root {} has an unsupported type",
                source.display()
            ),
        ));
    }
    mount_raw(
        Some(source),
        target,
        None,
        (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
        None,
        "failed to bind an admitted sandbox path",
    )?;
    set_mount_attributes(target, read_only, metadata.file_type().is_char_device())
}

fn set_mount_attributes(target: &Path, read_only: bool, permits_device: bool) -> Result<()> {
    let descriptor = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(target)
        .map_err(|error| sandbox_io("failed to open an admitted mount", error))?;
    let attributes = MountAttr {
        attr_set: MOUNT_ATTR_NOSUID
            | (u64::from(!permits_device) * MOUNT_ATTR_NODEV)
            | (u64::from(read_only) * MOUNT_ATTR_RDONLY),
        ..MountAttr::default()
    };
    let empty = CString::new("").expect("empty C string");
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            descriptor.as_raw_fd(),
            empty.as_ptr(),
            AT_EMPTY_PATH | AT_RECURSIVE,
            &attributes,
            mem::size_of::<MountAttr>(),
        )
    };
    if result != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "recursive mount hardening is unavailable",
        ));
    }
    Ok(())
}

fn chroot(root: &Path) -> Result<()> {
    let root = path_cstring(root)?;
    if unsafe { libc::chroot(root.as_ptr()) } != 0 || unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to enter the workspace root filesystem",
        ));
    }
    Ok(())
}

fn change_directory(path: &Path, admitted: RawFd) -> Result<()> {
    let path = path_cstring(path)?;
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(last_os_error(
            ProcessErrorCode::InvalidRequest,
            "admitted working directory is no longer available",
        ));
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    verify_same_identity(
        admitted,
        descriptor.as_raw_fd(),
        ProcessErrorCode::InvalidRequest,
        "working directory",
    )?;
    if unsafe { libc::fchdir(descriptor.as_raw_fd()) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::InvalidRequest,
            "failed to enter the admitted working directory",
        ));
    }
    Ok(())
}

fn verified_path_descriptor(path: &Path, admitted: RawFd, label: &str) -> Result<OwnedFd> {
    let path = path_cstring(path)?;
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxExecutableUnavailable,
            "admitted executable path is no longer available inside the target view",
        ));
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    verify_same_identity(
        admitted,
        descriptor.as_raw_fd(),
        ProcessErrorCode::SandboxExecutableUnavailable,
        label,
    )?;
    Ok(descriptor)
}

fn verify_same_identity(
    admitted: RawFd,
    actual: RawFd,
    code: ProcessErrorCode,
    label: &str,
) -> Result<()> {
    if descriptor_identity(actual)? != descriptor_identity(admitted)? {
        return Err(ProcessError::new(
            code,
            format!("admitted {label} identity changed before target setup"),
        ));
    }
    Ok(())
}

fn descriptor_identity(descriptor: RawFd) -> Result<(libc::dev_t, libc::ino_t)> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::InvalidRequest,
            "failed to inspect the admitted working-directory handle",
        ));
    }
    let metadata = unsafe { metadata.assume_init() };
    Ok((metadata.st_dev, metadata.st_ino))
}

fn apply_landlock(request: &LauncherRequest) -> Result<()> {
    let abi = landlock_abi()?;
    if abi < 3 {
        return Err(ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            format!("Linux Landlock ABI {abi} is below the required ABI 3"),
        ));
    }
    let attr = LandlockRulesetAttr {
        handled_access_fs: LANDLOCK_WRITE_ACCESS,
    };
    let ruleset = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr,
            mem::size_of::<LandlockRulesetAttr>(),
            0,
        )
    } as RawFd;
    if ruleset < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to create the Landlock ruleset",
        ));
    }
    let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset) };
    add_landlock_path(ruleset.as_raw_fd(), Path::new("/"), LANDLOCK_READ_ACCESS)?;
    let mut writable_paths = vec![Path::new(SANDBOX_HOME), Path::new(SANDBOX_TMP)];
    if request.request.authorization.workspace_write {
        writable_paths.push(request.request.workspace_root.as_path());
    }
    for path in writable_paths {
        add_landlock_path(ruleset.as_raw_fd(), path, LANDLOCK_WRITE_ACCESS)?;
    }
    for path in PRIVATE_DEVICE_PATHS {
        let path = Path::new(path);
        if path.exists() {
            add_landlock_path(ruleset.as_raw_fd(), path, LANDLOCK_DEVICE_ACCESS)?;
        }
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to enable no_new_privs before Landlock",
        ));
    }
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to enforce the Landlock ruleset",
        ));
    }
    Ok(())
}

fn add_landlock_path(ruleset: RawFd, path: &Path, allowed_access: u64) -> Result<()> {
    let path = path_cstring(path)?;
    let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to open a Landlock path",
        ));
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let attr = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd: descriptor.as_raw_fd(),
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
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to add a Landlock path rule",
        ));
    }
    Ok(())
}

fn landlock_abi() -> Result<u32> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<LandlockRulesetAttr>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if result < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "Linux Landlock is unavailable",
        ));
    }
    u32::try_from(result).map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            "Linux Landlock returned an invalid ABI",
        )
    })
}

fn drop_capabilities() -> Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to drop process capabilities",
        ));
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to enforce no_new_privs",
        ));
    }
    Ok(())
}

fn apply_seccomp(networkless: bool) -> Result<()> {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_JMP_JSET_K: u16 = 0x45;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
    const NR_OFFSET: u32 = 0;
    const ARCH_OFFSET: u32 = 4;
    const ARG0_OFFSET: u32 = 16;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    fn stmt(code: u16, value: u32) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k: value,
        }
    }
    fn jump(code: u16, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt,
            jf,
            k: value,
        }
    }

    let permission_denied = SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let unsupported = SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
    let mut filters = vec![
        stmt(BPF_LD_W_ABS, ARCH_OFFSET),
        jump(BPF_JMP_JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        stmt(BPF_LD_W_ABS, NR_OFFSET),
        jump(BPF_JMP_JEQ_K, libc::SYS_clone as u32, 0, 4),
        stmt(BPF_LD_W_ABS, ARG0_OFFSET),
        jump(
            BPF_JMP_JSET_K,
            (libc::CLONE_NEWCGROUP
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWUSER
                | libc::CLONE_NEWUTS) as u32,
            0,
            1,
        ),
        stmt(BPF_RET_K, permission_denied),
        stmt(BPF_RET_K, SECCOMP_RET_ALLOW),
        // clone3 stores its flags behind a userspace pointer, which classic
        // seccomp BPF cannot inspect. Keep it unavailable, but report ENOSYS so
        // libc can safely fall back to clone, whose namespace flags are
        // filtered above. Returning EPERM here breaks ordinary thread creation.
        jump(BPF_JMP_JEQ_K, libc::SYS_clone3 as u32, 0, 1),
        stmt(BPF_RET_K, unsupported),
    ];
    let mut denied = vec![
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_mount_setattr,
    ];
    if networkless {
        denied.extend([
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
            libc::SYS_socketpair,
        ]);
    }
    for syscall in denied {
        filters.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
        filters.push(stmt(BPF_RET_K, permission_denied));
    }
    filters.push(stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
    let program = libc::sock_fprog {
        len: u16::try_from(filters.len()).map_err(|_| {
            ProcessError::new(ProcessErrorCode::Internal, "seccomp program is too large")
        })?,
        filter: filters.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to install the seccomp filter",
        ));
    }
    Ok(())
}

fn apply_resource_limits() -> Result<()> {
    for (resource, current, maximum) in [
        (libc::RLIMIT_CORE, 0, 0),
        (libc::RLIMIT_NOFILE, 256, 256),
        (libc::RLIMIT_NPROC, 256, 256),
    ] {
        let limit = libc::rlimit {
            rlim_cur: current,
            rlim_max: maximum,
        };
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(last_os_error(
                ProcessErrorCode::SandboxUnavailable,
                "failed to apply process resource limits",
            ));
        }
    }
    Ok(())
}

fn probe_namespaces() -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        let ok = enter_namespaces(ExecutionProfile::Workspace).is_ok();
        unsafe { libc::_exit(i32::from(!ok)) }
    }
    let mut status = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0) == pid
            && libc::WIFEXITED(status)
            && libc::WEXITSTATUS(status) == 0
    }
}

fn probe_namespace(flag: libc::c_int) -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let mut ok = unshare(libc::CLONE_NEWUSER, "user namespace").is_ok();
        if ok {
            ok = install_identity_map(uid, gid).is_ok();
        }
        if ok && flag != 0 {
            ok = unshare(flag, "diagnostic namespace").is_ok();
        }
        unsafe { libc::_exit(i32::from(!ok)) }
    }
    let mut status = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0) == pid
            && libc::WIFEXITED(status)
            && libc::WEXITSTATUS(status) == 0
    }
}

fn probe_pidfd() -> bool {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) } as RawFd;
    if fd < 0 {
        return false;
    }
    drop(unsafe { OwnedFd::from_raw_fd(fd) });
    true
}

fn probe_execveat() -> bool {
    let result = unsafe {
        libc::syscall(
            libc::SYS_execveat,
            -1,
            c"".as_ptr(),
            std::ptr::null::<*const libc::c_char>(),
            std::ptr::null::<*const libc::c_char>(),
            libc::AT_EMPTY_PATH,
        )
    };
    result != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS)
}

fn probe_pty() -> bool {
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    unsafe { libc::grantpt(fd.as_raw_fd()) == 0 && libc::unlockpt(fd.as_raw_fd()) == 0 }
}

fn unshare(flags: libc::c_int, label: &str) -> Result<()> {
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            &format!("failed to create the required {label}"),
        ));
    }
    Ok(())
}

fn mount_raw(
    source: Option<&Path>,
    target: &Path,
    filesystem: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
    context: &str,
) -> Result<()> {
    let source = source.map(path_cstring).transpose()?;
    let target = path_cstring(target)?;
    let filesystem = filesystem.map(CString::new).transpose().map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            "mount type contains NUL",
        )
    })?;
    let data = data.map(CString::new).transpose().map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            "mount data contains NUL",
        )
    })?;
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            filesystem
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result != 0 {
        return Err(last_os_error(ProcessErrorCode::SandboxUnavailable, context));
    }
    Ok(())
}

fn ensure_directory(path: &Path, mode: u32) -> Result<()> {
    fs::create_dir_all(path)
        .map_err(|error| sandbox_io("failed to create sandbox directory", error))?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| sandbox_io("failed to harden sandbox directory", error))
}

fn path_cstring(path: &Path) -> Result<CString> {
    os_cstring(path.as_os_str())
}

fn os_cstring(value: &std::ffi::OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::SandboxUnavailable,
            "sandbox path contains NUL",
        )
    })
}

fn sandbox_io(context: &str, error: std::io::Error) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::SandboxUnavailable,
        format!("{context}: {error}"),
    )
}

#[allow(dead_code)]
fn write_probe(path: &Path, value: &[u8]) -> Result<()> {
    let mut file = File::create(path).map_err(|error| sandbox_io("probe create failed", error))?;
    file.write_all(value)
        .map_err(|error| sandbox_io("probe write failed", error))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::test_support::{RunId, StepId};
    use agl_exec::ExecutionId;

    use super::*;
    use agl_exec::{
        EnvironmentOverride, ExecutionAuthorization, ExecutionIo, ExecutionKind, ExecutionLimits,
        ExecutionRequest,
    };

    fn launcher_request(workspace_root: PathBuf, program: PathBuf) -> LauncherRequest {
        let run_id = RunId::generate();
        LauncherRequest {
            protocol_version: super::super::super::LAUNCHER_PROTOCOL_VERSION.to_owned(),
            build_id: super::super::super::LAUNCHER_BUILD_ID.to_owned(),
            execution_id: ExecutionId::generate(),
            request: ExecutionRequest {
                owner: crate::test_support::run_owner(&run_id, &run_id),
                correlation: crate::test_support::correlation(&run_id, &StepId::generate()),
                kind: ExecutionKind::Argv,
                argv0: program.display().to_string(),
                program,
                program_digest: None,
                args: Vec::new(),
                workspace_root: workspace_root.clone(),
                cwd: workspace_root.clone(),
                read_only_roots: Vec::new(),
                environment: EnvironmentOverride {
                    values: BTreeMap::new(),
                },
                stdin: None,
                close_stdin_after_initial: true,
                io: ExecutionIo::Pipes,
                terminal_size: None,
                profile: ExecutionProfile::Workspace,
                authorization: ExecutionAuthorization::default(),
                grant_lease: None,
                limits: ExecutionLimits {
                    timeout_ms: Some(1_000),
                    max_input_bytes: 1,
                    max_output_bytes: 1,
                },
            },
            execution_root: workspace_root.join("execution"),
            private_home: workspace_root.join("home"),
            private_tmp: workspace_root.join("tmp"),
            setup_timeout_ms: 1_000,
            has_private_environment: false,
            has_shell_integration: false,
        }
    }

    #[test]
    fn workspace_executable_must_be_under_an_admitted_root() {
        let workspace = std::env::temp_dir().join(format!(
            "agl-process-executable-admission-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        let mut request = launcher_request(workspace.clone(), program.clone());

        let error = validate_executable_admission(&request).unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::SandboxExecutableUnavailable);

        request.request.read_only_roots = vec![program.parent().unwrap().canonicalize().unwrap()];
        validate_executable_admission(&request).unwrap();

        request.request.profile = ExecutionProfile::Host;
        request.request.read_only_roots.clear();
        validate_executable_admission(&request).unwrap();
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn retained_working_directory_identity_detects_path_substitution() {
        let root = std::env::temp_dir().join(format!(
            "agl-process-cwd-identity-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let admitted_path = root.join("cwd");
        let moved_path = root.join("moved");
        fs::create_dir_all(&admitted_path).unwrap();
        let admitted = File::open(&admitted_path).unwrap();
        fs::rename(&admitted_path, &moved_path).unwrap();
        fs::create_dir(&admitted_path).unwrap();
        let replacement = File::open(&admitted_path).unwrap();

        assert_ne!(
            descriptor_identity(admitted.as_raw_fd()).unwrap(),
            descriptor_identity(replacement.as_raw_fd()).unwrap()
        );

        fs::remove_dir_all(root).unwrap();
    }
}
