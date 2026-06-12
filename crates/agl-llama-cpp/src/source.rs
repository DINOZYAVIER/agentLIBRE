use std::path::{Path, PathBuf};

use anyhow::{ensure, Result};

pub const MANAGED_LLAMA_CPP_DIR: &str = "vendor/llama.cpp";
pub const DEFAULT_LLAMA_CPP_BUILD_DIR: &str = "target/llama-cpp/build";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlamaCppSourceTree {
    root: PathBuf,
}

impl LlamaCppSourceTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn managed_from_workspace(workspace_root: impl AsRef<Path>) -> Self {
        Self::new(workspace_root.as_ref().join(MANAGED_LLAMA_CPP_DIR))
    }

    pub fn managed_from_current_crate() -> Self {
        Self::managed_from_workspace(current_workspace_root())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn validate_checkout(&self) -> Result<()> {
        ensure!(
            self.root.join("CMakeLists.txt").is_file(),
            "llama.cpp source tree missing CMakeLists.txt at {}",
            self.root.display()
        );
        ensure!(
            self.root.join("include/llama.h").is_file(),
            "llama.cpp source tree missing include/llama.h at {}",
            self.root.display()
        );
        ensure!(
            self.root.join("tools/completion/CMakeLists.txt").is_file(),
            "llama.cpp source tree missing tools/completion at {}",
            self.root.display()
        );
        Ok(())
    }
}

pub fn current_workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn default_build_dir(workspace_root: impl AsRef<Path>) -> PathBuf {
    workspace_root.as_ref().join(DEFAULT_LLAMA_CPP_BUILD_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_source_tree_points_at_vendor_llama_cpp() {
        let source = LlamaCppSourceTree::managed_from_current_crate();

        assert!(source.root().ends_with(MANAGED_LLAMA_CPP_DIR));
        source.validate_checkout().unwrap();
    }
}
