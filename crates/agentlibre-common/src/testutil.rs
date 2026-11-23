//! Helpers for the tests of the crates above: one counter for all, so the
//! temporary directories never collide (AGL-121, D78).

use std::sync::atomic::{AtomicUsize, Ordering};

/// A directory removed when dropped.
#[derive(Debug)]
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    /// There is no `Default`: every `new()` takes the next number of
    /// the counter.
    #[allow(clippy::new_without_default)]
    pub fn new() -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("agentLIBRE-forge-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// The directory.
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    /// A file `name` with `text` and `mode`.
    pub fn file(&self, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
