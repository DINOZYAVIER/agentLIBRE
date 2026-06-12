use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlamaCppProgram {
    Cli,
    Completion,
    Server,
}

impl LlamaCppProgram {
    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Cli => "llama-cli",
            Self::Completion => "llama-completion",
            Self::Server => "llama-server",
        }
    }

    pub fn binary_in_build_dir(self, build_dir: impl AsRef<Path>) -> PathBuf {
        build_dir.as_ref().join("bin").join(self.executable_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_resolves_build_dir_binary_paths() {
        let build_dir = PathBuf::from("/repo/target/llama-cpp/build");

        assert_eq!(
            LlamaCppProgram::Completion.binary_in_build_dir(&build_dir),
            PathBuf::from("/repo/target/llama-cpp/build/bin/llama-completion")
        );
    }
}
