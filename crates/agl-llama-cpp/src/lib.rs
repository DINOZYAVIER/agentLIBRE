use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Result};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlamaCppSwitch {
    On,
    Off,
    Auto,
}

impl LlamaCppSwitch {
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlamaCppCliInvocation {
    pub model: PathBuf,
    pub prompt: String,
    pub max_output_tokens: u32,
    pub context_tokens: u32,
    pub gpu_layers: u32,
    pub threads: u32,
    pub device: Option<String>,
    pub batch_size: Option<u32>,
    pub ubatch_size: Option<u32>,
    pub flash_attention: Option<LlamaCppSwitch>,
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    pub mmap: Option<bool>,
    pub jinja: Option<bool>,
    pub conversation: Option<bool>,
    pub simple_io: bool,
    pub display_prompt: Option<bool>,
}

impl LlamaCppCliInvocation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.as_os_str().is_empty(),
            "llama.cpp model path cannot be empty"
        );
        ensure!(!self.prompt.is_empty(), "llama.cpp prompt cannot be empty");
        if let Some(device) = &self.device {
            ensure!(
                !device.trim().is_empty(),
                "llama.cpp device cannot be empty"
            );
        }
        for (name, value) in [
            ("max_output_tokens", Some(self.max_output_tokens)),
            ("context_tokens", Some(self.context_tokens)),
            ("threads", Some(self.threads)),
            ("batch_size", self.batch_size),
            ("ubatch_size", self.ubatch_size),
        ] {
            if value == Some(0) {
                bail!("llama.cpp {name} cannot be zero");
            }
        }
        Ok(())
    }

    pub fn command_args(&self) -> Result<Vec<OsString>> {
        self.validate()?;

        let mut args = vec![
            "-m".into(),
            self.model.as_os_str().to_owned(),
            "-p".into(),
            self.prompt.clone().into(),
            "-n".into(),
            self.max_output_tokens.to_string().into(),
            "-c".into(),
            self.context_tokens.to_string().into(),
            "-ngl".into(),
            self.gpu_layers.to_string().into(),
            "-t".into(),
            self.threads.to_string().into(),
        ];

        if let Some(device) = &self.device {
            args.push("--device".into());
            args.push(device.as_str().into());
        }
        if let Some(batch_size) = self.batch_size {
            args.push("-b".into());
            args.push(batch_size.to_string().into());
        }
        if let Some(ubatch_size) = self.ubatch_size {
            args.push("-ub".into());
            args.push(ubatch_size.to_string().into());
        }
        if let Some(flash_attention) = self.flash_attention {
            args.push("-fa".into());
            args.push(flash_attention.as_arg().into());
        }
        if let Some(cache_type_k) = &self.cache_type_k {
            args.push("-ctk".into());
            args.push(cache_type_k.as_str().into());
        }
        if let Some(cache_type_v) = &self.cache_type_v {
            args.push("-ctv".into());
            args.push(cache_type_v.as_str().into());
        }
        if let Some(mmap) = self.mmap {
            args.push(if mmap { "--mmap" } else { "--no-mmap" }.into());
        }
        if let Some(jinja) = self.jinja {
            args.push(if jinja { "--jinja" } else { "--no-jinja" }.into());
        }
        if let Some(conversation) = self.conversation {
            args.push(if conversation { "-cnv" } else { "-no-cnv" }.into());
        }
        if self.simple_io {
            args.push("--simple-io".into());
        }
        if let Some(display_prompt) = self.display_prompt {
            args.push(
                if display_prompt {
                    "--display-prompt"
                } else {
                    "--no-display-prompt"
                }
                .into(),
            );
        }

        Ok(args)
    }
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

    #[test]
    fn program_resolves_build_dir_binary_paths() {
        let build_dir = PathBuf::from("/repo/target/llama-cpp/build");

        assert_eq!(
            LlamaCppProgram::Completion.binary_in_build_dir(&build_dir),
            PathBuf::from("/repo/target/llama-cpp/build/bin/llama-completion")
        );
    }

    #[test]
    fn invocation_builds_explicit_gpu_completion_args() {
        let invocation = LlamaCppCliInvocation {
            model: "/models/qwen.gguf".into(),
            prompt: "User:\nhello\n\nAssistant:\n".to_string(),
            max_output_tokens: 64,
            context_tokens: 2048,
            gpu_layers: 999,
            threads: 8,
            device: Some("Vulkan0".to_string()),
            batch_size: Some(1024),
            ubatch_size: Some(256),
            flash_attention: Some(LlamaCppSwitch::On),
            cache_type_k: Some("q8_0".to_string()),
            cache_type_v: Some("q8_0".to_string()),
            mmap: Some(false),
            jinja: Some(true),
            conversation: Some(false),
            simple_io: true,
            display_prompt: Some(false),
        };

        let args = invocation
            .command_args()
            .unwrap()
            .into_iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "-m",
                "/models/qwen.gguf",
                "-p",
                "User:\nhello\n\nAssistant:\n",
                "-n",
                "64",
                "-c",
                "2048",
                "-ngl",
                "999",
                "-t",
                "8",
                "--device",
                "Vulkan0",
                "-b",
                "1024",
                "-ub",
                "256",
                "-fa",
                "on",
                "-ctk",
                "q8_0",
                "-ctv",
                "q8_0",
                "--no-mmap",
                "--jinja",
                "-no-cnv",
                "--simple-io",
                "--no-display-prompt",
            ]
        );
    }
}
