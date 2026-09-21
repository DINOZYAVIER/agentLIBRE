use std::fs;
use std::path::{Component, Path, PathBuf};

pub use agl_core::package::*;

const MAX_PACKAGE_FILES: usize = 10_000;
const MAX_PACKAGE_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DirectoryPackageView {
    root: PathBuf,
}

impl DirectoryPackageView {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PackageError> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root).map_err(|error| io(&root, error))?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::Symlink(root.display().to_string()));
        }
        if !metadata.is_dir() {
            return Err(PackageError::NotRegular(root.display().to_string()));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl PackageView for DirectoryPackageView {
    fn files(&self) -> Result<Vec<PackageRelativePath>, PackageError> {
        let mut output = Vec::new();
        collect(&self.root, "", &mut output)?;
        if output.len() > MAX_PACKAGE_FILES {
            return Err(PackageError::TreeTooLarge);
        }
        output.sort();
        Ok(output)
    }

    fn read_file(&self, path: &PackageRelativePath) -> Result<Vec<u8>, PackageError> {
        let mut target = self.root.clone();
        for component in Path::new(path.as_str()).components() {
            let Component::Normal(component) = component else {
                return Err(PackageError::InvalidRelativePath(path.to_string()));
            };
            target.push(component);
            let metadata = fs::symlink_metadata(&target).map_err(|error| io(&target, error))?;
            if metadata.file_type().is_symlink() {
                return Err(PackageError::Symlink(path.to_string()));
            }
        }
        let metadata = fs::metadata(&target).map_err(|error| io(&target, error))?;
        if !metadata.is_file() || metadata.len() > MAX_PACKAGE_FILE_BYTES {
            return Err(PackageError::NotRegular(path.to_string()));
        }
        fs::read(&target).map_err(|error| io(&target, error))
    }
}

fn collect(
    directory: &Path,
    prefix: &str,
    output: &mut Vec<PackageRelativePath>,
) -> Result<(), PackageError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| io(directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io(directory, error))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| PackageError::InvalidRelativePath("non-UTF-8".into()))?;
        if prefix.is_empty() && name == ".git" {
            continue;
        }
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| io(&entry.path(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::Symlink(relative));
        }
        if metadata.is_dir() {
            collect(&entry.path(), &relative, output)?;
        } else if metadata.is_file() {
            output.push(PackageRelativePath::new(relative)?);
        } else {
            return Err(PackageError::NotRegular(relative));
        }
    }
    Ok(())
}

fn io(path: &Path, error: std::io::Error) -> PackageError {
    PackageError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_digest_excludes_only_root_git_metadata() {
        let root = std::env::temp_dir().join(format!(
            "agl-runtime-package-git-metadata-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "private checkout metadata").unwrap();
        fs::write(root.join("MODEL.toml"), "model").unwrap();
        let with_metadata =
            compute_package_digest(&DirectoryPackageView::new(root.clone()).unwrap()).unwrap();
        fs::remove_dir_all(root.join(".git")).unwrap();
        let without_metadata =
            compute_package_digest(&DirectoryPackageView::new(root.clone()).unwrap()).unwrap();
        assert_eq!(with_metadata, without_metadata);
        fs::remove_dir_all(root).unwrap();
    }
}
