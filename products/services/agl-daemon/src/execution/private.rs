use agl_core::execution::ExecutionStartRequest;
use agl_runtime::inference::PrivateInference;
use anyhow::{Context, Result};
use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
const EXECUTABLE_FD: RawFd = 189;
const LISTEN_FD: RawFd = 190;
const MODEL_FD: RawFd = 200;
pub(super) fn exec(
    mut command: Command,
    request: &ExecutionStartRequest,
    isolation: &PrivateInference,
    directory: &Path,
) -> Result<()> {
    let executable_path = PathBuf::from(&request.argv[0]);
    let executable = File::open(&executable_path).context("failed to open inference executable")?;
    let artifacts = isolation
        .artifacts
        .iter()
        .map(|path| File::open(path).context("failed to open inference artifact"))
        .collect::<Result<Vec<_>>>()?;
    let socket_path = Path::new(&isolation.listen_socket);
    let listener = UnixListener::bind(socket_path).context("failed to bind inference socket")?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let device_paths = isolation
        .device_paths
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let model_files = artifacts.iter().collect::<Vec<_>>();
    let sandbox = super::sandbox::PreparedSandbox::prepare(
        &executable_path,
        &executable,
        &model_files,
        Path::new(&request.cwd),
        &device_paths,
    )?;
    let executable_source = executable.as_raw_fd();
    let listener_source = listener.as_raw_fd();
    let artifact_sources = artifacts
        .iter()
        .enumerate()
        .map(|(index, file)| (file.as_raw_fd(), MODEL_FD + index as RawFd))
        .collect::<Vec<_>>();
    let last_artifact_fd = MODEL_FD + artifact_sources.len() as RawFd - 1;
    let held_resources = (executable, artifacts, listener);
    command.env("AGL_LLAMA_SERVER_LISTEN_FD", LISTEN_FD.to_string());

    let argv = request
        .argv
        .iter()
        .map(|s| std::ffi::CString::new(s.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut environment = if request.clear_environment {
        std::collections::BTreeMap::new()
    } else {
        std::env::vars_os().collect::<std::collections::BTreeMap<_, _>>()
    };
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => {
                environment.insert(name.to_owned(), value.to_owned());
            }
            None => {
                environment.remove(name);
            }
        }
    }
    let environment = environment
        .into_iter()
        .map(|(key, value)| {
            use std::os::unix::ffi::OsStrExt;
            let mut bytes = key.as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend(value.as_bytes());
            std::ffi::CString::new(bytes)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let address_limit = isolation.address_space_limit_bytes;
    super::atomic_json(&directory.join("started.json"), &true)?;
    unsafe {
        command.pre_exec(move || {
            let mut argv_ptrs = argv.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
            argv_ptrs.push(std::ptr::null());
            let mut env_ptrs = environment.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
            env_ptrs.push(std::ptr::null());
            duplicate(executable_source, EXECUTABLE_FD)?;
            duplicate(listener_source, LISTEN_FD)?;
            for (source, target) in &artifact_sources {
                duplicate(*source, *target)?;
            }
            sandbox.enter()?;
            close_other_descriptors(last_artifact_fd)?;
            set_address_limit(address_limit)?;
            install_network_filter()?;
            let _ = &held_resources;
            let target = std::ffi::CString::new(format!("/proc/self/fd/{EXECUTABLE_FD}")).unwrap();
            let result = libc::execve(target.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
            debug_assert_eq!(result, -1);
            Err(std::io::Error::last_os_error())
        });
    }
    Err(command.exec().into())
}
unsafe fn duplicate(source: RawFd, target: RawFd) -> std::io::Result<()> {
    if source != target && unsafe { libc::dup2(source, target) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(target, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn close_other_descriptors(last_artifact_fd: RawFd) -> std::io::Result<()> {
    for (start, end) in [
        (3, (EXECUTABLE_FD - 1) as u32),
        ((LISTEN_FD + 1) as u32, (MODEL_FD - 1) as u32),
        ((last_artifact_fd + 1) as u32, u32::MAX),
    ] {
        if unsafe { libc::syscall(libc::SYS_close_range, start, end, 0u32) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn set_address_limit(bytes: u64) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_network_filter() -> std::io::Result<()> {
    const LD_W_ABS: u16 = 0x20;
    const JMP_JEQ: u16 = 0x15;
    const RET: u16 = 0x06;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private inference seccomp does not support this architecture",
    ));
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
    let denied = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let mut filter = vec![
        stmt(LD_W_ABS, 4),
        jump(JMP_JEQ, AUDIT_ARCH, 1, 0),
        stmt(RET, denied),
        stmt(LD_W_ABS, 0),
    ];
    for syscall in [
        libc::SYS_socket,
        libc::SYS_connect,
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
        filter.push(jump(JMP_JEQ, syscall as u32, 0, 1));
        filter.push(stmt(RET, denied));
    }
    filter.push(stmt(RET, libc::SECCOMP_RET_ALLOW));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_network_filter() -> std::io::Result<()> {
    Ok(())
}

pub(super) fn planner_filter(command: &mut Command, cache: File) -> Result<()> {
    let filter = super::readonly::socket_filter()?;
    let source = filter.as_raw_fd();
    let cache_source = cache.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            duplicate(source, 188)?;
            duplicate(cache_source, 187)?;
            let _ = (&filter, &cache);
            Ok(())
        });
    }
    Ok(())
}
