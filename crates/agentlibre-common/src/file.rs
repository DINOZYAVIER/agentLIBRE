//! Files written whole or not at all (AGL-121, D78).

use std::fs;
use std::path::Path;

use crate::error::Error;

/// Writes `text` to `path`: a temp file in the same directory plus a
/// rename, so a crash leaves no half-written file.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), Error> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.{}.{}.tmp", std::process::id(), n));
    fs::write(&tmp, text).map_err(|source| Error::Write {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| Error::Write { path: path.into(), source })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn write_atomic_leaves_no_temp_files() {
        let dir = TempDir::new();
        let path = dir.path().join("x.txt");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        for entry in fs::read_dir(dir.path()).unwrap() {
            assert_eq!(entry.unwrap().file_name().to_string_lossy().as_ref(), "x.txt");
        }
    }
}
