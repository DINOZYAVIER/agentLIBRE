//! Race-safe creation of exact private runtime directories on Linux.
//!
//! Callers use these roots for host-owned inference state.  Traversal starts
//! from an already-open `/` descriptor and every component is opened with
//! `O_NOFOLLOW`, so a symlink cannot redirect creation between inspection and
//! use.

#![cfg(target_os = "linux")]

use std::error::Error;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// Opens or creates an exact private directory without following any path
/// component through a symlink.
///
/// Missing intermediate components are created with mode 0700. Existing
/// intermediates may be ordinary system/user directories, but the final
/// component must be owned by the effective UID and have exact mode 0700.
pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), PrivateDirectoryError> {
    let components = validated_absolute_components(path)?;
    let mut directory = open_filesystem_root(path)?;

    for (index, component) in components.iter().enumerate() {
        let final_component = index + 1 == components.len();
        let (next, created) = match open_directory_at(&directory, component) {
            Ok(next) => (next, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let created = create_directory_at(&directory, component, path)?;
                let next = open_directory_at(&directory, component).map_err(|error| {
                    PrivateDirectoryError::path_io(path, "open newly created component", error)
                })?;
                (next, created)
            }
            Err(error) => {
                return Err(classify_component_error(path, error));
            }
        };

        if created {
            set_exact_private_mode(&next, path)?;
            validate_private_component(&next, path)?;
        }
        if final_component {
            validate_private_component(&next, path)?;
        }
        directory = next;
    }
    Ok(())
}

fn validated_absolute_components(path: &Path) -> Result<Vec<OsString>, PrivateDirectoryError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes.first() != Some(&b'/')
        || bytes.starts_with(b"//")
        || bytes.ends_with(b"/")
    {
        return Err(PrivateDirectoryError::invalid(path));
    }

    let mut components = Vec::new();
    for component in bytes[1..].split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." || component.contains(&0)
        {
            return Err(PrivateDirectoryError::invalid(path));
        }
        components.push(OsString::from_vec(component.to_vec()));
    }
    Ok(components)
}

fn open_filesystem_root(path: &Path) -> Result<File, PrivateDirectoryError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|error| PrivateDirectoryError::path_io(path, "open filesystem root", error))
}

fn open_directory_at(parent: &File, component: &OsStr) -> io::Result<File> {
    let component = CString::new(component.as_bytes())?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn create_directory_at(
    parent: &File,
    component: &OsStr,
    path: &Path,
) -> Result<bool, PrivateDirectoryError> {
    let component = CString::new(component.as_bytes())
        .map_err(|error| PrivateDirectoryError::path_io(path, "encode component", error.into()))?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        return Ok(false);
    }
    Err(PrivateDirectoryError::path_io(
        path,
        "create private directory component",
        error,
    ))
}

fn set_exact_private_mode(directory: &File, path: &Path) -> Result<(), PrivateDirectoryError> {
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(PrivateDirectoryError::path_io(
            path,
            "set private directory mode",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn validate_private_component(directory: &File, path: &Path) -> Result<(), PrivateDirectoryError> {
    let metadata = directory.metadata().map_err(|error| {
        PrivateDirectoryError::path_io(path, "inspect private directory", error)
    })?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(PrivateDirectoryError::security(path));
    }
    Ok(())
}

fn classify_component_error(path: &Path, error: io::Error) -> PrivateDirectoryError {
    if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
        PrivateDirectoryError::security(path)
    } else {
        PrivateDirectoryError::path_io(path, "open private directory component", error)
    }
}

#[derive(Debug)]
pub(crate) struct PrivateDirectoryError {
    path: PathBuf,
    kind: PrivateDirectoryErrorKind,
}

#[derive(Debug)]
enum PrivateDirectoryErrorKind {
    InvalidPath,
    SecurityViolation,
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl PrivateDirectoryError {
    fn invalid(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            kind: PrivateDirectoryErrorKind::InvalidPath,
        }
    }

    fn security(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            kind: PrivateDirectoryErrorKind::SecurityViolation,
        }
    }

    fn path_io(path: &Path, operation: &'static str, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            kind: PrivateDirectoryErrorKind::Io { operation, source },
        }
    }
}

impl fmt::Display for PrivateDirectoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            PrivateDirectoryErrorKind::InvalidPath => write!(
                formatter,
                "private directory must be an exact non-root absolute path: {}",
                self.path.display()
            ),
            PrivateDirectoryErrorKind::SecurityViolation => write!(
                formatter,
                "private directory path contains a symlink/non-directory or its final component is not same-UID mode 0700: {}",
                self.path.display()
            ),
            PrivateDirectoryErrorKind::Io { operation, source } => {
                write!(formatter, "{operation} {}: {source}", self.path.display())
            }
        }
    }
}

impl Error for PrivateDirectoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            PrivateDirectoryErrorKind::Io { source, .. } => Some(source),
            PrivateDirectoryErrorKind::InvalidPath
            | PrivateDirectoryErrorKind::SecurityViolation => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = PathBuf::from(format!(
                "/tmp/agl-private-directory-{label}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create fixture root");
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .expect("restrict fixture root");
            Self { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn creates_missing_intermediates_and_exact_private_final() {
        let fixture = Fixture::new("create");
        let target = fixture.root.join("missing/parents/private");
        ensure_private_directory(&target).unwrap();

        for path in [
            fixture.root.join("missing"),
            fixture.root.join("missing/parents"),
            target,
        ] {
            let metadata = fs::symlink_metadata(path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
            assert_eq!(metadata.mode() & 0o7777, 0o700);
        }
    }

    #[test]
    fn rejects_intermediate_and_final_symlinks_without_writing_through_them() {
        let fixture = Fixture::new("symlink");
        let outside = fixture.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();

        symlink(&outside, fixture.root.join("middle")).unwrap();
        assert!(ensure_private_directory(&fixture.root.join("middle/private")).is_err());
        assert!(!outside.join("private").exists());

        symlink(&outside, fixture.root.join("final")).unwrap();
        assert!(ensure_private_directory(&fixture.root.join("final")).is_err());
    }

    #[test]
    fn rejects_existing_final_with_non_private_mode() {
        let fixture = Fixture::new("mode");
        let target = fixture.root.join("private");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(ensure_private_directory(&target).is_err());
        assert_eq!(fs::metadata(target).unwrap().mode() & 0o7777, 0o755);
    }

    #[test]
    fn rejects_aliasing_and_non_exact_paths() {
        for path in [
            "relative",
            "/",
            "//tmp/private",
            "/tmp/../private",
            "/tmp/private/",
        ] {
            assert!(ensure_private_directory(Path::new(path)).is_err(), "{path}");
        }
    }
}
