use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agl_model::{HostCapabilityDevice, HostCapabilityDeviceKind};

use crate::host::descriptors::VerifiedDescriptorSet;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
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
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_IOCTL_DEV;
const READ_FILE: u64 = LANDLOCK_ACCESS_FS_READ_FILE;
const READ_DIRECTORY: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const RUNTIME_DIRECTORY: u64 = READ_DIRECTORY | LANDLOCK_ACCESS_FS_EXECUTE;
const PRIVATE_DIRECTORY: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE;
const GPU_DEVICE: u64 =
    LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE | LANDLOCK_ACCESS_FS_IOCTL_DEV;

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

pub(super) struct PreparedSandbox {
    ruleset: OwnedFd,
    _paths: Vec<OwnedFd>,
    device_namespace: Option<DeviceNamespace>,
}

struct DeviceNamespace {
    source: CString,
    view: CString,
    target: CString,
    selected_target: CString,
    uid_map: CString,
    gid_map: CString,
}

impl PreparedSandbox {
    pub(super) fn prepare(
        executable_path: &Path,
        executable: &File,
        descriptors: &VerifiedDescriptorSet,
        private_directory: &Path,
        selected_device: Option<&HostCapabilityDevice>,
    ) -> std::io::Result<Self> {
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if abi < 5 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("inference requires Linux Landlock ABI 5; kernel returned {abi}"),
            ));
        }
        let attr = RulesetAttr {
            handled_access_fs: LANDLOCK_HANDLED_FS,
            handled_access_net: LANDLOCK_ACCESS_NET_BIND_TCP | LANDLOCK_ACCESS_NET_CONNECT_TCP,
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
        let mut seen = BTreeSet::new();
        let mut device_namespace = None;

        add_path(
            &ruleset,
            &mut paths,
            &mut seen,
            executable_path,
            READ_FILE | LANDLOCK_ACCESS_FS_EXECUTE,
        )?;
        if let Some(parent) = executable_path.parent() {
            add_path(&ruleset, &mut paths, &mut seen, parent, RUNTIME_DIRECTORY)?;
        }
        for path in embedded_runtime_paths(executable, executable_path.parent())? {
            let store_root = nix_store_root(&path);
            let allowed = store_root.as_deref().unwrap_or(path.as_path());
            add_path(
                &ruleset,
                &mut paths,
                &mut seen,
                allowed,
                if allowed.is_dir() {
                    RUNTIME_DIRECTORY
                } else {
                    READ_FILE | LANDLOCK_ACCESS_FS_EXECUTE
                },
            )?;
        }
        for path in dynamic_runtime_files(executable_path)? {
            add_path(
                &ruleset,
                &mut paths,
                &mut seen,
                &path,
                READ_FILE | LANDLOCK_ACCESS_FS_EXECUTE,
            )?;
        }
        for path in [
            Path::new("/proc/self/fd"),
            Path::new("/proc/cpuinfo"),
            Path::new("/proc/meminfo"),
            Path::new("/sys/devices/system/cpu"),
            Path::new("/dev/null"),
            Path::new("/dev/urandom"),
        ] {
            if path.exists() {
                add_path(
                    &ruleset,
                    &mut paths,
                    &mut seen,
                    path,
                    if path.is_dir() {
                        READ_DIRECTORY
                    } else {
                        READ_FILE
                    },
                )?;
            }
        }
        for artifact in &descriptors.files {
            add_fd_rule(&ruleset, artifact.file.as_raw_fd(), READ_FILE)?;
        }
        add_path(
            &ruleset,
            &mut paths,
            &mut seen,
            private_directory,
            PRIVATE_DIRECTORY,
        )?;
        if let Some(device) = selected_device
            && !matches!(device.kind, HostCapabilityDeviceKind::Cpu)
        {
            add_path(
                &ruleset,
                &mut paths,
                &mut seen,
                Path::new("/dev"),
                LANDLOCK_ACCESS_FS_READ_DIR,
            )?;
            let nodes = selected_render_nodes(device)?;
            if nodes.len() != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "selected accelerator does not resolve to one DRM render node",
                ));
            }
            add_path(
                &ruleset,
                &mut paths,
                &mut seen,
                Path::new("/dev/dri"),
                LANDLOCK_ACCESS_FS_READ_DIR,
            )?;
            add_path(&ruleset, &mut paths, &mut seen, &nodes[0], GPU_DEVICE)?;
            let render_name = nodes[0].file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "selected DRM render node has no basename",
                )
            })?;
            let selected_sysfs = std::fs::canonicalize(
                Path::new("/sys/class/drm").join(render_name).join("device"),
            )?;
            add_path(
                &ruleset,
                &mut paths,
                &mut seen,
                &selected_sysfs,
                READ_DIRECTORY,
            )?;
            device_namespace = Some(prepare_device_namespace(
                private_directory,
                &nodes[0],
                render_name,
            )?);
            if Path::new("/sys/dev/char").exists() {
                add_path(
                    &ruleset,
                    &mut paths,
                    &mut seen,
                    Path::new("/sys/dev/char"),
                    LANDLOCK_ACCESS_FS_READ_DIR,
                )?;
            }
            for library in vulkan_driver_libraries()? {
                let runtime_root = nix_store_root(&library).unwrap_or_else(|| library.clone());
                add_path(
                    &ruleset,
                    &mut paths,
                    &mut seen,
                    &runtime_root,
                    if runtime_root.is_dir() {
                        RUNTIME_DIRECTORY
                    } else {
                        READ_FILE | LANDLOCK_ACCESS_FS_EXECUTE
                    },
                )?;
                for dependency in dynamic_runtime_files(&library)? {
                    let dependency_root =
                        nix_store_root(&dependency).unwrap_or_else(|| dependency.clone());
                    add_path(
                        &ruleset,
                        &mut paths,
                        &mut seen,
                        &dependency_root,
                        if dependency_root.is_dir() {
                            RUNTIME_DIRECTORY
                        } else {
                            READ_FILE | LANDLOCK_ACCESS_FS_EXECUTE
                        },
                    )?;
                }
            }
            for path in [
                Path::new("/run/opengl-driver"),
                Path::new("/run/opengl-driver/share/vulkan/icd.d"),
                Path::new("/run/opengl-driver/share/vulkan/implicit_layer.d"),
                Path::new("/etc/vulkan/icd.d"),
                Path::new("/usr/share/vulkan/icd.d"),
            ] {
                if path.exists() {
                    add_path(&ruleset, &mut paths, &mut seen, path, RUNTIME_DIRECTORY)?;
                }
            }
        }
        Ok(Self {
            ruleset,
            _paths: paths,
            device_namespace,
        })
    }

    pub(super) fn enter(&self) -> std::io::Result<()> {
        set_limits()?;
        if let Some(namespace) = &self.device_namespace {
            namespace.enter()?;
        }
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
        install_seccomp()
    }
}

fn prepare_device_namespace(
    private_directory: &Path,
    selected_node: &Path,
    render_name: &std::ffi::OsStr,
) -> std::io::Result<DeviceNamespace> {
    let view = private_directory.join("device-view");
    std::fs::create_dir(&view)?;
    let selected_target = view.join(render_name);
    File::create(&selected_target)?;
    let cstring = |path: &Path| {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "device namespace path contains an interior NUL byte",
            )
        })
    };
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    Ok(DeviceNamespace {
        source: cstring(selected_node)?,
        view: cstring(&view)?,
        target: cstring(Path::new("/dev/dri"))?,
        selected_target: cstring(&selected_target)?,
        uid_map: CString::new(format!("0 {uid} 1\n")).expect("numeric UID map has no NUL"),
        gid_map: CString::new(format!("0 {gid} 1\n")).expect("numeric GID map has no NUL"),
    })
}

impl DeviceNamespace {
    fn enter(&self) -> std::io::Result<()> {
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        write_proc_map(b"/proc/self/setgroups\0", b"deny\n")?;
        write_proc_map(b"/proc/self/uid_map\0", self.uid_map.as_bytes())?;
        write_proc_map(b"/proc/self/gid_map\0", self.gid_map.as_bytes())?;
        if unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        bind_mount(&self.source, &self.selected_target, false)?;
        bind_mount(&self.view, &self.target, true)
    }
}

fn bind_mount(source: &CString, target: &CString, recursive: bool) -> std::io::Result<()> {
    let flags = if recursive {
        libc::MS_BIND | libc::MS_REC
    } else {
        libc::MS_BIND
    };
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    } != 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_proc_map(path: &[u8], value: &[u8]) -> std::io::Result<()> {
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut offset = 0;
    while offset < value.len() {
        let written =
            unsafe { libc::write(fd, value[offset..].as_ptr().cast(), value.len() - offset) };
        if written < 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        offset += written as usize;
    }
    if unsafe { libc::close(fd) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn nix_store_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let root = components.next()?;
    let nix = components.next()?;
    let store = components.next()?;
    let package = components.next()?;
    if root.as_os_str() == "/" && nix.as_os_str() == "nix" && store.as_os_str() == "store" {
        Some(
            Path::new("/")
                .join(nix.as_os_str())
                .join(store.as_os_str())
                .join(package.as_os_str()),
        )
    } else {
        None
    }
}

fn dynamic_runtime_files(executable: &Path) -> std::io::Result<Vec<PathBuf>> {
    let ldd = [
        Path::new("/run/current-system/sw/bin/ldd"),
        Path::new("/usr/bin/ldd"),
        Path::new("/bin/ldd"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "sealed engine runtime closure inspector is unavailable",
        )
    })?;
    let output = Command::new(ldd).env_clear().arg(executable).output()?;
    if !output.status.success() || output.stdout.len() > 1024 * 1024 {
        return Err(std::io::Error::other(
            "sealed engine dynamic runtime closure inspection failed",
        ));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "dynamic runtime closure is not UTF-8",
        )
    })?;
    let mut files = BTreeSet::new();
    for line in text.lines() {
        let candidate = line
            .split_once("=>")
            .map(|(_, tail)| tail)
            .unwrap_or(line)
            .split_whitespace()
            .next()
            .unwrap_or_default();
        if !candidate.starts_with('/') {
            continue;
        }
        let path = std::fs::canonicalize(candidate)?;
        if !path.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "dynamic runtime closure contains a non-file entry",
            ));
        }
        files.insert(path);
    }
    if files.is_empty() || files.len() > 256 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "dynamic runtime closure is empty or exceeds 256 files",
        ));
    }
    Ok(files.into_iter().collect())
}

fn add_path(
    ruleset: &OwnedFd,
    paths: &mut Vec<OwnedFd>,
    seen: &mut BTreeSet<PathBuf>,
    path: &Path,
    access: u64,
) -> std::io::Result<()> {
    let canonical = std::fs::canonicalize(path)?;
    if !seen.insert(canonical.clone()) {
        return Ok(());
    }
    let value = CString::new(canonical.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox path contains an interior NUL byte",
        )
    })?;
    let fd = unsafe { libc::open(value.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    add_fd_rule(ruleset, owned.as_raw_fd(), access)?;
    paths.push(owned);
    Ok(())
}

fn add_fd_rule(ruleset: &OwnedFd, fd: RawFd, access: u64) -> std::io::Result<()> {
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: fd,
        reserved: 0,
    };
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &attr,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn embedded_runtime_paths(
    executable: &File,
    sibling_directory: Option<&Path>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    let mut scanned = BTreeSet::new();
    scan_embedded_paths(executable.try_clone()?, &mut paths)?;
    if let Some(directory) = sibling_directory {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let canonical = std::fs::canonicalize(entry.path())?;
            if metadata.is_file()
                && metadata.len() <= 1024 * 1024 * 1024
                && scanned.insert(canonical.clone())
            {
                scan_embedded_paths(File::open(canonical)?, &mut paths)?;
            }
        }
    }
    Ok(paths.into_iter().collect())
}

fn scan_embedded_paths(mut file: File, paths: &mut BTreeSet<PathBuf>) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut cursor = 0;
    while let Some(offset) = bytes[cursor..].iter().position(|byte| *byte == b'/') {
        let start = cursor + offset;
        if !bytes[start..].starts_with(b"/nix/store/") {
            cursor = start + 1;
            continue;
        }
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|length| start + length)
            .unwrap_or(bytes.len());
        if let Ok(value) = std::str::from_utf8(&bytes[start..end]) {
            for entry in value.split(':') {
                let path = PathBuf::from(entry);
                if path.starts_with("/nix/store/") && path.exists() {
                    paths.insert(path);
                }
            }
        }
        cursor = end.saturating_add(1);
        if cursor >= bytes.len() {
            break;
        }
    }
    Ok(())
}

fn selected_render_nodes(device: &HostCapabilityDevice) -> std::io::Result<Vec<PathBuf>> {
    let Some(expected_device) = device.pci_device_id.as_deref() else {
        return Ok(Vec::new());
    };
    let mut nodes = Vec::new();
    let root = match std::fs::read_dir("/sys/class/drm") {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(nodes),
        Err(error) => return Err(error),
    };
    for entry in root {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name
            .strip_prefix("renderD")
            .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        let device_root = entry.path().join("device");
        let actual_device = join_pci_words(
            read_pci_word(&device_root.join("vendor")),
            read_pci_word(&device_root.join("device")),
        );
        let actual_subsystem = join_pci_words(
            read_pci_word(&device_root.join("subsystem_vendor")),
            read_pci_word(&device_root.join("subsystem_device")),
        );
        if actual_device.as_deref() == Some(expected_device)
            && device
                .pci_subsystem_id
                .as_deref()
                .is_none_or(|expected| actual_subsystem.as_deref() == Some(expected))
        {
            nodes.push(Path::new("/dev/dri").join(name));
        }
    }
    nodes.sort();
    Ok(nodes)
}

fn vulkan_driver_libraries() -> std::io::Result<Vec<PathBuf>> {
    let mut libraries = BTreeSet::new();
    for directory in [
        Path::new("/run/opengl-driver/share/vulkan/icd.d"),
        Path::new("/etc/vulkan/icd.d"),
        Path::new("/usr/share/vulkan/icd.d"),
    ] {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries.take(32) {
            let path = entry?.path();
            let metadata = std::fs::metadata(&path)?;
            if !metadata.is_file() || metadata.len() > 64 * 1024 {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid Vulkan ICD manifest {}: {error}", path.display()),
                )
            })?;
            let Some(library) = manifest
                .pointer("/ICD/library_path")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let library = Path::new(library);
            if !library.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Vulkan ICD library path must be absolute",
                ));
            }
            let library = std::fs::canonicalize(library)?;
            if !library.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Vulkan ICD library is not a regular file",
                ));
            }
            libraries.insert(library);
        }
    }
    if libraries.is_empty() || libraries.len() > 32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Vulkan runtime has no bounded ICD library set",
        ));
    }
    Ok(libraries.into_iter().collect())
}

fn read_pci_word(path: &Path) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn join_pci_words(vendor: Option<String>, device: Option<String>) -> Option<String> {
    Some(format!("{}:{}", vendor?, device?))
}

fn set_limits() -> std::io::Result<()> {
    set_limit(libc::RLIMIT_CORE, 0)?;
    clamp_limit(libc::RLIMIT_AS, 512 * 1024 * 1024 * 1024)?;
    clamp_limit(libc::RLIMIT_FSIZE, 512 * 1024 * 1024)?;
    clamp_limit(libc::RLIMIT_NOFILE, 256)?;
    clamp_limit(libc::RLIMIT_NPROC, 4096)?;
    clamp_limit(libc::RLIMIT_STACK, 64 * 1024 * 1024)?;
    clamp_limit(libc::RLIMIT_MEMLOCK, 8 * 1024 * 1024 * 1024)
}

fn set_limit(resource: libc::__rlimit_resource_t, value: libc::rlim_t) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn clamp_limit(resource: libc::__rlimit_resource_t, cap: libc::rlim_t) -> std::io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
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
    set_limit_pair(resource, soft, maximum)
}

fn set_limit_pair(
    resource: libc::__rlimit_resource_t,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn install_seccomp() -> std::io::Result<()> {
    const LD_W_ABS: u16 = 0x20;
    const JMP_JEQ: u16 = 0x15;
    const RET: u16 = 0x06;
    const ALLOW: u32 = 0x7fff_0000;
    const ERRNO: u32 = 0x0005_0000;
    const NUMBER_OFFSET: u32 = 0;
    const ARCH_OFFSET: u32 = 4;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
    let denied = ERRNO | libc::EPERM as u32;
    let mut filters = vec![
        stmt(LD_W_ABS, ARCH_OFFSET),
        jump(JMP_JEQ, AUDIT_ARCH, 1, 0),
        stmt(RET, denied),
        stmt(LD_W_ABS, NUMBER_OFFSET),
    ];
    for syscall in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_open_by_handle_at,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_fork,
        libc::SYS_vfork,
    ] {
        filters.push(jump(JMP_JEQ, syscall as u32, 0, 1));
        filters.push(stmt(RET, denied));
    }
    filters.push(stmt(RET, ALLOW));
    let mut program = libc::sock_fprog {
        len: u16::try_from(filters.len())
            .map_err(|_| std::io::Error::other("seccomp program is too large"))?,
        filter: filters.as_mut_ptr(),
    };
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &mut program,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
