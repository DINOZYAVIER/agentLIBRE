//! Persistent workspace cache; Cargo dependency directories are read-only.
use super::*;
use std::os::fd::{AsRawFd, FromRawFd};

#[derive(Serialize, Deserialize)]
pub(super) struct PlannerCache {
    pub path: PathBuf,
    pub registry: Option<PathBuf>,
    pub git: Option<PathBuf>,
}
impl PlannerCache {
    pub fn prepare(data: &Path, workspace: &Path) -> Result<Self> {
        let path = cache_path(data, workspace)?;
        private_dir(path.parent().unwrap())?;
        private_dir(&path)?;
        let cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
        let source = |name: &str| -> Result<Option<PathBuf>> {
            let Some(cargo) = &cargo else {
                return Ok(None);
            };
            let directory = cargo.join(name);
            if !directory.try_exists()? {
                return Ok(None);
            }
            let directory = directory.canonicalize()?;
            ensure!(
                directory.is_dir(),
                "Cargo dependency source is not a directory"
            );
            Ok(Some(directory))
        };
        Ok(Self {
            path,
            registry: source("registry")?,
            git: source("git")?,
        })
    }
    pub fn pin(&self) -> Result<File> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        // Commands can write below this mount, so never follow their symlinks
        // while preparing the next launch on the host.
        let cargo = child_directory(&root, c"cargo")?;
        for name in [c"registry", c"git"] {
            child_directory(&cargo, name)?;
        }
        Ok(root)
    }
}
fn child_directory(parent: &File, name: &std::ffi::CStr) -> Result<File> {
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    ensure!(
        fd >= 0,
        "planner cache directory is invalid: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn cache_path(data: &Path, workspace: &Path) -> Result<PathBuf> {
    let workspace = workspace.canonicalize()?;
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(data.join("planner-cache").join(digest))
}
pub fn clear(data: &Path, workspace: &Path) -> Result<()> {
    let path = cache_path(data, workspace)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "planner cache root is not a directory"
    );
    fs::remove_dir_all(path)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planner_cache_symlinks_cannot_modify_an_outside_directory() {
        let root =
            PathBuf::from("/tmp").join(format!("agl-cache-boundary-{}", uuid::Uuid::now_v7()));
        private_dir(&root).unwrap();
        let outside = root.join("outside");
        private_dir(&outside).unwrap();
        let cache = PlannerCache {
            path: root.join("cache"),
            registry: None,
            git: None,
        };
        private_dir(&cache.path).unwrap();
        std::os::unix::fs::symlink(&outside, cache.path.join("cargo")).unwrap();
        assert!(cache.pin().is_err());
        assert!(!outside.join("registry").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
