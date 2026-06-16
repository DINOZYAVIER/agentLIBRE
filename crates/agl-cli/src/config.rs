use std::path::PathBuf;

use agl_runtime::{AgentLibreRuntimeConfig, write_default_runtime_config};
use anyhow::Result;

use crate::args::ConfigCommand;

pub(crate) fn run_config(command: ConfigCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        ConfigCommand::Paths => {
            for (name, path) in config_paths(runtime) {
                println!("{name}={}", path.display());
            }
            Ok(())
        }
        ConfigCommand::Init { force } => {
            let path = runtime.paths.runtime_config_path();
            write_default_runtime_config(&path, force)?;
            println!("wrote {}", path.display());
            Ok(())
        }
    }
}

fn config_paths(runtime: &AgentLibreRuntimeConfig) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("config_dir", runtime.paths.config_dir.clone()),
        ("data_dir", runtime.paths.data_dir.clone()),
        ("state_dir", runtime.paths.state_dir.clone()),
        ("cache_dir", runtime.paths.cache_dir.clone()),
        ("runtime_config", runtime.paths.runtime_config_path()),
        (
            "local_inference_config",
            runtime.paths.default_local_inference_config(),
        ),
        ("app_log", runtime.paths.app_log_path()),
        ("inference_log", runtime.paths.inference_log_path()),
        ("sessions_root", runtime.paths.sessions_root()),
    ]
}

#[cfg(test)]
mod tests {
    use agl_runtime::AgentLibrePaths;

    use super::*;

    #[test]
    fn config_paths_include_runtime_files() {
        let runtime =
            AgentLibreRuntimeConfig::from_paths(AgentLibrePaths::from_agl_home("/tmp/agl-home"))
                .unwrap();

        let paths = config_paths(&runtime);

        assert!(paths.iter().any(|(name, _)| *name == "runtime_config"));
        assert!(paths.iter().any(|(name, _)| *name == "app_log"));
        assert!(paths.iter().any(|(name, _)| *name == "sessions_root"));
    }
}
