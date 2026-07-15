use std::path::{Component, Path, PathBuf};

use crate::{DEFAULT_DATABASE_FILE, Result, StoreError};

pub fn default_database_path(root: impl AsRef<Path>) -> Result<PathBuf> {
    database_path(root.as_ref(), DEFAULT_DATABASE_FILE)
}

pub(crate) fn database_path(root: &Path, file_name: &str) -> Result<PathBuf> {
    validate_database_file_name(file_name)?;
    Ok(root.join(file_name))
}

fn validate_database_file_name(file_name: &str) -> Result<()> {
    let path = Path::new(file_name);
    if path.as_os_str().is_empty() {
        return Err(StoreError::InvalidPath {
            path: path.to_path_buf(),
            reason: "database file name cannot be empty",
        });
    }
    if path.is_absolute() || path.components().count() != 1 {
        return Err(StoreError::InvalidPath {
            path: path.to_path_buf(),
            reason: "database file name must be a single relative path segment",
        });
    }
    match path.components().next() {
        Some(Component::Normal(_)) => Ok(()),
        _ => Err(StoreError::InvalidPath {
            path: path.to_path_buf(),
            reason: "database file name must be normal path segment",
        }),
    }
}

#[cfg(unix)]
pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
