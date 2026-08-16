use std::env;
use std::path::{Path, PathBuf};

use agl_ids::{RunId, SessionId};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "agentLIBRE";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLibrePaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
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
            runtime_dir: env_path("XDG_RUNTIME_DIR")
                .unwrap_or_else(default_runtime_home)
                .join(APP_DIR),
        })
    }

    pub fn from_agl_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            config_dir: home.join("config"),
            data_dir: home.join("data"),
            state_dir: home.join("state"),
            cache_dir: home.join("cache"),
            runtime_dir: home.join("runtime").join(APP_DIR),
        }
    }

    pub fn runtime_config_path(&self) -> PathBuf {
        self.config_dir.join("agentlibre.toml")
    }

    pub fn default_artifact_root(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn store_root(&self) -> PathBuf {
        self.data_dir.join("store")
    }

    pub fn model_install_root(&self) -> PathBuf {
        self.data_dir.join("models").join("installed")
    }

    pub fn setup_state_root(&self) -> PathBuf {
        self.state_dir.join("setup")
    }

    pub fn model_lease_root(&self) -> PathBuf {
        self.state_dir.join("models").join("leases")
    }

    pub fn inference_state_root(&self) -> PathBuf {
        self.state_dir.join("inference")
    }

    /// State owned by the independently installed `agl-terminal` product.
    ///
    /// agentLIBRE may address this root through the terminal client API,
    /// but it must never place terminal state beneath its own application root.
    pub fn terminal_state_root(&self) -> PathBuf {
        self.state_dir
            .parent()
            .unwrap_or(&self.state_dir)
            .join("agl-terminal")
    }

    pub fn terminal_runtime_root(&self) -> PathBuf {
        self.runtime_dir
            .parent()
            .unwrap_or(&self.runtime_dir)
            .join("agl-terminal")
    }

    pub fn inference_worker_temp_root(&self) -> PathBuf {
        self.inference_state_root().join("worker-tmp")
    }

    pub fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.sessions_root().join(session_id.as_str())
    }

    pub fn session_run_artifact_root(&self, session_id: &SessionId, run_id: &RunId) -> PathBuf {
        self.session_dir(session_id)
            .join("runs")
            .join(run_id.as_str())
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

fn default_runtime_home() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` has no preconditions and does not mutate memory.
        let uid = unsafe { libc::geteuid() };
        PathBuf::from(format!("/run/user/{uid}"))
    }
    #[cfg(not(unix))]
    fallback_home_dir().join(".local/state")
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
        assert_eq!(
            paths.runtime_dir,
            PathBuf::from("/tmp/agl-home/runtime/agentLIBRE")
        );
    }

    #[test]
    fn derived_paths_match_layout() {
        let paths = AgentLibrePaths::from_agl_home("/tmp/agl-home");
        let session_id = SessionId::parse("ses_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31").unwrap();
        let run_id = RunId::parse("run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32").unwrap();

        assert_eq!(
            paths.runtime_config_path(),
            PathBuf::from("/tmp/agl-home/config/agentlibre.toml")
        );
        assert_eq!(
            paths.default_artifact_root(),
            PathBuf::from("/tmp/agl-home/data")
        );
        assert_eq!(
            paths.session_run_artifact_root(&session_id, &run_id),
            PathBuf::from(format!(
                "/tmp/agl-home/data/sessions/{session_id}/runs/{run_id}"
            ))
        );
        assert_eq!(
            paths.store_root(),
            PathBuf::from("/tmp/agl-home/data/store")
        );
        assert_eq!(
            paths.model_install_root(),
            PathBuf::from("/tmp/agl-home/data/models/installed")
        );
        assert_eq!(
            paths.setup_state_root(),
            PathBuf::from("/tmp/agl-home/state/setup")
        );
        assert_eq!(
            paths.model_lease_root(),
            PathBuf::from("/tmp/agl-home/state/models/leases")
        );
        assert_eq!(
            paths.terminal_state_root(),
            PathBuf::from("/tmp/agl-home/agl-terminal")
        );
        assert_eq!(
            paths.terminal_runtime_root(),
            PathBuf::from("/tmp/agl-home/runtime/agl-terminal")
        );
        assert_eq!(
            paths.inference_worker_temp_root(),
            PathBuf::from("/tmp/agl-home/state/inference/worker-tmp")
        );
        assert_eq!(
            paths.app_log_path(),
            PathBuf::from("/tmp/agl-home/state/logs/agentLIBRE.log")
        );
    }
}
