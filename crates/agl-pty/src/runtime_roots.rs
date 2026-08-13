use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use agl_exec::{ProcessError, ProcessErrorCode, Result};

pub const STANDARD_RUNTIME_ROOTS: &[&str] = &[
    "/bin",
    "/usr/bin",
    "/usr/lib",
    "/usr/lib64",
    "/lib",
    "/lib64",
    "/nix/store",
    "/run/current-system/sw",
];

pub fn standard_runtime_roots() -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    for candidate in STANDARD_RUNTIME_ROOTS {
        let candidate = Path::new(candidate);
        match fs::metadata(candidate) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(ProcessError::new(
                        ProcessErrorCode::SandboxUnavailable,
                        format!(
                            "standard Linux runtime root {} is not a directory",
                            candidate.display()
                        ),
                    ));
                }
                let canonical = candidate.canonicalize().map_err(|error| {
                    runtime_root_io(
                        &format!(
                            "standard Linux runtime root {} cannot be canonicalized",
                            candidate.display()
                        ),
                        error,
                    )
                })?;
                roots.insert(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(runtime_root_io(
                    &format!(
                        "standard Linux runtime root {} cannot be inspected",
                        candidate.display()
                    ),
                    error,
                ));
            }
        }
    }
    Ok(roots.into_iter().collect())
}

fn runtime_root_io(context: &str, error: std::io::Error) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::SandboxUnavailable,
        format!("{context}: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::standard_runtime_roots;

    #[test]
    fn roots_are_existing_canonical_directories_without_alias_duplicates() {
        let roots = standard_runtime_roots().expect("standard runtime roots");
        assert!(!roots.is_empty());
        assert!(roots.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(roots.iter().all(|root| {
            root.is_absolute()
                && root.is_dir()
                && root
                    .canonicalize()
                    .is_ok_and(|canonical| canonical == *root)
        }));
    }
}
