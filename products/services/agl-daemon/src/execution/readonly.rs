//! Linux/Nix read-only planner view. Host IPC files are never mounted.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use agl_core::execution::{ExecutionId, ExecutionStartRequest};
use anyhow::{Context, Result, ensure};

pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn recover(
        data_root: &Path,
        inactive: impl Fn(ExecutionId) -> Result<bool>,
    ) -> Result<()> {
        let parent = data_root.join("planner-scratch");
        if !parent.try_exists()? {
            return Ok(());
        }
        ensure!(
            !fs::symlink_metadata(&parent)?.file_type().is_symlink(),
            "planner scratch storage is a symlink"
        );
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if let Ok(id) = ExecutionId::parse(&entry.file_name().to_string_lossy())
                && inactive(id)?
            {
                remove_scratch(&entry.path())?;
            }
        }
        Ok(())
    }

    pub(crate) fn create(data_root: &Path, workspace: &Path, id: ExecutionId) -> Result<Self> {
        let data_root = data_root.canonicalize()?;
        ensure!(
            !data_root.starts_with(workspace),
            "planner scratch storage is inside the workspace"
        );
        let parent = data_root.join("planner-scratch");
        fs::create_dir_all(&parent)?;
        ensure!(
            !fs::symlink_metadata(&parent)?.file_type().is_symlink(),
            "planner scratch storage is a symlink"
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        let path = parent.join(id.to_string());
        fs::create_dir(&path)?;
        let scratch = Self(path);
        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700))?;
        fs::create_dir(scratch.0.join("scratch"))?;
        fs::create_dir(scratch.0.join("scratch/cache"))?;
        fs::create_dir(scratch.0.join("view"))?;
        Ok(scratch)
    }

    pub(crate) fn command(
        &self,
        request: &ExecutionStartRequest,
        workspace: &Path,
        bwrap: &Path,
        cache: &super::planner_cache::PlannerCache,
    ) -> Result<(Vec<String>, File)> {
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
        let cwd = Path::new(&request.cwd).canonicalize()?;
        ensure!(
            cwd.starts_with(workspace),
            "planner cwd is outside the workspace"
        );
        let executable = Path::new(&request.argv[0]).canonicalize()?;
        ensure!(
            executable.starts_with("/nix/store") && executable.is_file(),
            "read-only execution requires a Nix store executable"
        );
        ensure!(
            Path::new("/nix/store").is_dir(),
            "read-only execution requires the trusted Nix store"
        );
        let view = self.0.join("view");
        let mut roots = vec![(workspace.to_owned(), workspace.to_owned())];
        // Also retain the declared workspace path when its host path contains symlinks.
        let agl_core::execution::ExecutionIsolation::ReadOnlyWorkspace {
            workspace_root,
            output_directories,
        } = &request.isolation
        else {
            anyhow::bail!("expected read-only workspace isolation");
        };
        let declared = absolute_target(Path::new(workspace_root))?;
        if declared != workspace {
            roots.push((workspace.to_owned(), declared));
        }
        let excluded = output_directories
            .iter()
            .map(|directory| view_path(&view, &workspace.join(directory)))
            .collect::<Vec<_>>();
        git_roots(workspace, &mut roots)?;
        let mut sources = roots
            .iter()
            .map(|(source, _)| source.clone())
            .collect::<Vec<_>>();
        sources.sort_by_key(|path| path.components().count());
        sources.dedup();
        let mut copy_roots: Vec<PathBuf> = Vec::new();
        for source in sources {
            ensure!(
                !self.0.starts_with(&source),
                "execution storage is inside a read-only source tree"
            );
            if !copy_roots.iter().any(|root| source.starts_with(root)) {
                copy_roots.push(source);
            }
        }
        let covered = |path: &Path| copy_roots.iter().any(|root| path.starts_with(root));
        let mut runtime_dirs = Vec::new();
        let paths = request
            .environment
            .get("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| {
                (!request.clear_environment)
                    .then(|| std::env::var_os("PATH"))
                    .flatten()
            });
        for path in paths
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .chain(Path::new(&request.argv[0]).parent().map(Path::to_owned))
        {
            if !path.is_absolute() {
                continue;
            }
            let target = absolute_target(&path)?;
            if target.starts_with("/nix/store") || covered(&target) {
                continue;
            }
            if let Ok(source) = path.canonicalize()
                && source.starts_with("/nix/store")
                && source.is_dir()
            {
                fs::create_dir_all(view_path(&view, &target))?;
                if !runtime_dirs.iter().any(|(_, prior)| prior == &target) {
                    runtime_dirs.push((source, target));
                }
            }
        }
        let executable_target = absolute_target(Path::new(&request.argv[0]))?;
        let bind_executable = !executable_target.starts_with("/nix/store")
            && !covered(&executable_target)
            && !runtime_dirs
                .iter()
                .any(|(_, target)| executable_target.starts_with(target));
        if bind_executable {
            let path = view_path(&view, &executable_target);
            fs::create_dir_all(path.parent().context("executable has no parent")?)?;
            File::create(path)?;
        }
        for (_, target) in &roots {
            if !covered(target) {
                fs::create_dir_all(view_path(&view, target))?;
            }
        }
        for path in ["/nix/store", "/proc", "/dev"] {
            fs::create_dir_all(view_path(&view, Path::new(path)))?;
        }
        let cache_file = cache.pin()?;
        fs::create_dir_all(view_path(&view, &cache.path))?;
        let scratch_path = self.0.join("scratch");
        fs::create_dir_all(view_path(&view, &scratch_path))?;
        for source in &copy_roots {
            let pinned = pin(source)?;
            ensure!(
                pinned.metadata()?.is_dir(),
                "read-only tree source is not a directory"
            );
            clone_pinned(
                &pinned,
                source,
                &view_path(&view, source),
                deadline,
                &excluded,
            )?;
        }
        for name in ["passwd", "group", "nsswitch.conf", "hosts", "localtime"] {
            let source = Path::new("/etc").join(name);
            if !source.try_exists()? {
                continue;
            }
            let canonical = source.canonicalize()?;
            let file = pinned_regular(&canonical)?;
            let destination = view_path(&view, &source);
            fs::create_dir_all(destination.parent().context("config file has no parent")?)?;
            if fs::symlink_metadata(&destination).is_ok() {
                fs::remove_file(&destination)?;
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            std::io::copy(&mut &file, &mut output)?;
            output.set_permissions(fs::Permissions::from_mode(0o444))?;
        }
        ensure!(
            Instant::now() < deadline,
            "read-only view preparation exceeded execution timeout"
        );
        let mut argv = vec![bwrap.to_string_lossy().into_owned()];
        if request.clear_environment {
            argv.push("--clearenv".into());
        }
        for (name, value) in &request.environment {
            argv.extend(["--setenv", name, value].map(str::to_owned));
        }
        argv.extend(
            [
                "--unshare-all",
                "--unshare-user",
                "--disable-userns",
                "--die-with-parent",
                "--new-session",
                "--cap-drop",
                "ALL",
                "--ro-bind",
            ]
            .map(str::to_owned),
        );
        argv.extend([view.to_string_lossy().into_owned(), "/".into()]);
        argv.extend(["--ro-bind", "/nix/store", "/nix/store"].map(str::to_owned));
        for (source, target) in &runtime_dirs {
            bind(&mut argv, source, target);
        }
        if bind_executable {
            bind(&mut argv, &executable, &executable_target);
        }
        for (source, target) in &roots {
            let prepared = view_path(&view, source).canonicalize()?;
            ensure!(
                prepared.starts_with(&view) && prepared.is_dir(),
                "prepared Git/workspace tree escaped the private view"
            );
            bind(&mut argv, &prepared, target);
        }
        argv.extend(
            [
                "--proc",
                "/proc",
                "--remount-ro",
                "/proc",
                "--dev",
                "/dev",
                "--remount-ro",
                "/dev",
                "--bind",
            ]
            .map(str::to_owned),
        );
        let scratch = scratch_path
            .to_str()
            .context("planner scratch path is not UTF-8")?;
        argv.extend([scratch, scratch].map(str::to_owned));
        for name in ["TMPDIR", "TMP", "TEMP"] {
            argv.extend(["--setenv", name, scratch].map(str::to_owned));
        }
        argv.extend([
            "--setenv".into(),
            "XDG_CACHE_HOME".into(),
            cache.path.to_string_lossy().into_owned(),
        ]);
        argv.extend([
            "--bind-fd".into(),
            "187".into(),
            cache.path.to_string_lossy().into_owned(),
        ]);
        let cargo = cache.path.join("cargo");
        for (source, name) in [(&cache.registry, "registry"), (&cache.git, "git")] {
            if let Some(source) = source {
                bind(&mut argv, source, &cargo.join(name));
            }
        }
        argv.extend([
            "--setenv".into(),
            "CARGO_HOME".into(),
            cargo.to_string_lossy().into_owned(),
        ]);
        argv.extend(["--setenv", "CARGO_NET_OFFLINE", "true"].map(str::to_owned));
        argv.extend(["--seccomp", "188", "--chdir"].map(str::to_owned));
        argv.push(cwd.to_string_lossy().into_owned());
        argv.push("--".into());
        argv.extend(request.argv.clone());
        Ok((argv, cache_file))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = remove_scratch(&self.0) {
            tracing::warn!(path = %self.0.display(), %error, "failed to remove planner scratch/view");
        }
    }
}

fn bind(argv: &mut Vec<String>, source: &Path, target: &Path) {
    argv.extend([
        "--ro-bind".into(),
        source.to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    ]);
}

fn absolute_target(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "sandbox target must be absolute");
    let mut target = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(name) => target.push(name),
            Component::ParentDir => {
                target.pop();
            }
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => anyhow::bail!("unsupported sandbox path prefix"),
        }
    }
    Ok(target)
}

fn view_path(view: &Path, absolute: &Path) -> PathBuf {
    view.join(
        absolute
            .strip_prefix("/")
            .expect("validated absolute view path"),
    )
}

fn pin(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("pinning view source {}", path.display()))
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn pinned_regular(path: &Path) -> Result<File> {
    let pinned = pin(path)?;
    ensure!(
        pinned.metadata()?.is_file(),
        "view source is not a regular file: {}",
        path.display()
    );
    // Opening the retained inode avoids following a replaced source path into a FIFO.
    Ok(File::open(fd_path(&pinned))?)
}

fn clone_tree(
    source: &Path,
    destination: &Path,
    deadline: Instant,
    excluded: &[PathBuf],
) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "read-only view preparation exceeded execution timeout"
    );
    if excluded.iter().any(|root| destination.starts_with(root)) {
        return Ok(());
    }
    let pinned = pin(source)?;
    clone_pinned(&pinned, source, destination, deadline, excluded)
}

fn clone_pinned(
    pinned: &File,
    source: &Path,
    destination: &Path,
    deadline: Instant,
    excluded: &[PathBuf],
) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "read-only view preparation exceeded execution timeout"
    );
    let metadata = pinned.metadata()?;
    if metadata.file_type().is_symlink() {
        symlink(fs::read_link(source)?, destination)?;
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(fd_path(pinned))? {
            let entry = entry?;
            clone_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                deadline,
                excluded,
            )?;
        }
        File::open(destination)?.set_times(
            fs::FileTimes::new()
                .set_accessed(metadata.accessed()?)
                .set_modified(metadata.modified()?),
        )?;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(metadata.permissions().mode() & 0o777),
        )?;
    } else if metadata.is_file() {
        let input = File::open(fd_path(pinned))?;
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        // SAFETY: both descriptors are retained regular files; FICLONE creates independent file data.
        let result = unsafe {
            libc::ioctl(
                output.as_raw_fd(),
                0x4004_9409_u64 as libc::c_ulong,
                input.as_raw_fd(),
            )
        };
        ensure!(
            result == 0,
            "FICLONE is required for read-only views: {}",
            std::io::Error::last_os_error()
        );
        output.set_permissions(fs::Permissions::from_mode(
            metadata.permissions().mode() & 0o777,
        ))?;
        output.set_times(
            fs::FileTimes::new()
                .set_accessed(metadata.accessed()?)
                .set_modified(metadata.modified()?),
        )?;
    } else {
        anyhow::bail!(
            "unsupported special file in read-only view: {}",
            source.display()
        );
    }
    Ok(())
}

fn control_path(path: &Path) -> Result<String> {
    let file = pinned_regular(&path.canonicalize()?)?;
    let mut value = String::new();
    file.take(32_769).read_to_string(&mut value)?;
    ensure!(value.len() <= 32_768, "Git directory locator is too large");
    Ok(value)
}

fn git_roots(workspace: &Path, roots: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    let dotgit = workspace.join(".git");
    if !dotgit.try_exists()? {
        return Ok(());
    }
    let metadata = fs::metadata(&dotgit)?;
    let logical = if metadata.is_dir() {
        if fs::symlink_metadata(&dotgit)?.file_type().is_symlink() {
            let target = fs::read_link(&dotgit)?;
            absolute_target(&workspace.join(target))?
        } else {
            dotgit
        }
    } else {
        let locator = control_path(&dotgit)?;
        let target = locator
            .trim()
            .strip_prefix("gitdir:")
            .context("invalid .git directory locator")?
            .trim();
        ensure!(!target.is_empty(), "empty Git directory locator");
        absolute_target(&workspace.join(target))?
    };
    let directory = logical.canonicalize()?;
    ensure!(directory.is_dir(), "Git directory is not a directory");
    ensure!(
        directory.join("HEAD").is_file(),
        "Git directory has no HEAD file"
    );
    roots.push((directory.clone(), logical));
    let common = directory.join("commondir");
    if common.try_exists()? {
        let locator = control_path(&common)?;
        ensure!(
            !locator.trim().is_empty(),
            "empty Git common directory locator"
        );
        let logical = absolute_target(&directory.join(locator.trim()))?;
        let canonical = logical.canonicalize()?;
        ensure!(
            canonical.is_dir(),
            "Git common directory is not a directory"
        );
        roots.push((canonical, logical));
    }
    Ok(())
}

fn remove_scratch(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(path)? {
            remove_scratch(&entry?.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

pub(crate) fn socket_filter() -> Result<File> {
    const LD_W_ABS: u16 = 0x20;
    const JMP_JEQ: u16 = 0x15;
    const JMP_JGE: u16 = 0x35;
    const RET: u16 = 0x06;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("planner seccomp requires x86_64 or aarch64");
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
        jump(JMP_JEQ, ARCH, 1, 0),
        stmt(RET, denied),
        stmt(LD_W_ABS, 0),
        jump(JMP_JGE, 0x4000_0000, 0, 1),
        stmt(RET, denied),
    ];
    for syscall in [
        libc::SYS_socket,
        libc::SYS_mknod,
        libc::SYS_mknodat,
        libc::SYS_connect,
        // io_uring can perform socket operations without those syscalls.
        libc::SYS_io_uring_setup,
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
        libc::SYS_open_by_handle_at,
    ] {
        filter.push(jump(JMP_JEQ, syscall as u32, 0, 1));
        filter.push(stmt(RET, denied));
    }
    filter.push(stmt(RET, libc::SECCOMP_RET_ALLOW));
    // SAFETY: the name is NUL-terminated; a successful syscall returns a new FD.
    let fd = unsafe {
        libc::memfd_create(
            c"agl-planner-seccomp".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(
        fd >= 0,
        "failed to create planner seccomp file: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fd was just allocated by memfd_create and has one owner.
    let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // SAFETY: sock_filter is the kernel's fixed-size BPF instruction representation.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            filter.as_ptr().cast::<u8>(),
            std::mem::size_of_val(filter.as_slice()),
        )
    };
    file.write_all(bytes)?;
    file.rewind()?;
    // SAFETY: fd is still owned by file and the memfd supports sealing.
    let sealed = unsafe {
        libc::fcntl(
            fd,
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    };
    ensure!(
        sealed == 0,
        "failed to seal planner seccomp file: {}",
        std::io::Error::last_os_error()
    );
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_reflink_fails_without_copying_source_bytes() {
        let root =
            std::env::temp_dir().join(format!("agl-view-reflink-{}", ExecutionId::generate()));
        fs::create_dir(&root).unwrap();
        let destination = root.join("version");
        let error = clone_tree(
            Path::new("/proc/version"),
            &destination,
            Instant::now() + Duration::from_secs(1),
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("FICLONE is required"));
        assert_eq!(fs::metadata(&destination).unwrap().len(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
