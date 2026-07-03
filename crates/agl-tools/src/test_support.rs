use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("agl-tools-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn temp_root(label: &str) -> TempRoot {
    TempRoot::new(label)
}

pub(crate) fn value_for(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::to_string)
}
