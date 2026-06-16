use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::AgentLibreSessionId;

const APP_DIR: &str = "agentLIBRE";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLibrePaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl AgentLibrePaths {
    pub fn from_env() -> Result<Self> {
        if let Some(home) = env::var_os("AGL_HOME") {
            return Ok(Self::from_agl_home(home));
        }

        let project_dirs = ProjectDirs::from("", "", APP_DIR)
            .context("failed to resolve agentLIBRE project directories")?;
        Ok(Self {
            config_dir: env_path("XDG_CONFIG_HOME")
                .map(|path| path.join(APP_DIR))
                .unwrap_or_else(|| project_dirs.config_dir().to_path_buf()),
            data_dir: env_path("XDG_DATA_HOME")
                .map(|path| path.join(APP_DIR))
                .unwrap_or_else(|| project_dirs.data_dir().to_path_buf()),
            state_dir: env_path("XDG_STATE_HOME")
                .map(|path| path.join(APP_DIR))
                .or_else(|| project_dirs.state_dir().map(Path::to_path_buf))
                .unwrap_or_else(|| fallback_home_dir().join(".local/state").join(APP_DIR)),
            cache_dir: env_path("XDG_CACHE_HOME")
                .map(|path| path.join(APP_DIR))
                .unwrap_or_else(|| project_dirs.cache_dir().to_path_buf()),
        })
    }

    pub fn from_agl_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            config_dir: home.join("config"),
            data_dir: home.join("data"),
            state_dir: home.join("state"),
            cache_dir: home.join("cache"),
        }
    }

    pub fn default_local_inference_config(&self) -> PathBuf {
        self.config_dir.join("inference").join("local.toml")
    }

    pub fn runtime_config_path(&self) -> PathBuf {
        self.config_dir.join("agentlibre.toml")
    }

    pub fn default_artifact_root(&self) -> PathBuf {
        self.data_dir.join("runs")
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn session_dir(&self, session_id: &AgentLibreSessionId) -> PathBuf {
        self.sessions_root().join(session_id.as_str())
    }

    pub fn session_run_artifact_root(
        &self,
        session_id: &AgentLibreSessionId,
        run_id: &str,
    ) -> PathBuf {
        self.session_dir(session_id).join("runs").join(run_id)
    }

    pub fn app_log_path(&self) -> PathBuf {
        self.state_dir.join("logs").join("agentLIBRE.log")
    }

    pub fn inference_log_path(&self) -> PathBuf {
        self.state_dir.join("logs").join("inference.log")
    }

    pub fn llama_cpp_cache_root(&self) -> PathBuf {
        self.cache_dir.join("llama-cpp")
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn fallback_home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agl_home_paths_are_self_contained() {
        let paths = AgentLibrePaths::from_agl_home("/tmp/agl-home");

        assert_eq!(paths.config_dir, PathBuf::from("/tmp/agl-home/config"));
        assert_eq!(paths.data_dir, PathBuf::from("/tmp/agl-home/data"));
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/agl-home/state"));
        assert_eq!(paths.cache_dir, PathBuf::from("/tmp/agl-home/cache"));
    }

    #[test]
    fn derived_paths_match_layout() {
        let paths = AgentLibrePaths::from_agl_home("/tmp/agl-home");
        let session_id = AgentLibreSessionId::new("session-001").unwrap();

        assert_eq!(
            paths.default_local_inference_config(),
            PathBuf::from("/tmp/agl-home/config/inference/local.toml")
        );
        assert_eq!(
            paths.runtime_config_path(),
            PathBuf::from("/tmp/agl-home/config/agentlibre.toml")
        );
        assert_eq!(
            paths.default_artifact_root(),
            PathBuf::from("/tmp/agl-home/data/runs")
        );
        assert_eq!(
            paths.session_run_artifact_root(&session_id, "run-001"),
            PathBuf::from("/tmp/agl-home/data/sessions/session-001/runs/run-001")
        );
        assert_eq!(
            paths.app_log_path(),
            PathBuf::from("/tmp/agl-home/state/logs/agentLIBRE.log")
        );
    }
}
