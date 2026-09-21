mod activation;
mod agent;
mod implementation;
mod plans;
mod protocol;
mod store;
mod tools;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::{AgentDependencies, AgentHandle, AgentService};
use crate::store::StoreHandle;
use agl_core::agent::ExactPackageRef;
use agl_daemon_api::{
    AgentCommand, AgentProtocolRequest, AgentProtocolResponse, AgentProtocolStreamFrame,
    AgentResponse, AgentSubscriptionFrame, FunctionActivationView, MAX_JSONL_FRAME_BYTES,
    PlanArtifactView, ProtocolError, ProtocolErrorCode,
};
use agl_execution_api::ExecutionClient;
use agl_runtime::extension::ExtensionBindings;
use agl_runtime::inference::InferenceConfig;
use agl_runtime::{RuntimeConfig, RuntimeHandle, RuntimeService};
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub use activation::ListenerSource;
pub use plans::{PlanArtifactStore, StoredPlan};
pub use protocol::serve_connection;
pub use store::rotation::StoreRotation;
pub use tools::searxng::SearxngConfig;

pub fn rotate_store(data_root: &Path) -> Result<StoreRotation> {
    store::rotation::rotate(&data_root.join("store"))
}

pub fn unfinished_agent_runs(data_root: impl AsRef<Path>) -> Result<Vec<agl_core::AgentRunId>> {
    store::unfinished_agent_runs_at(data_root.as_ref().join("store")).map_err(Into::into)
}

/// Validate an existing Store without creating or changing it.
pub fn preflight_store(data_root: impl AsRef<Path>) -> Result<()> {
    store::preflight_at(data_root.as_ref().join("store")).map_err(Into::into)
}

pub fn is_permanent_startup_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PermanentStartupError>().is_some()
        || matches!(
            error.downcast_ref::<store::StoreError>(),
            Some(store::StoreError::IncompatibleDatabase { .. })
        )
}

#[derive(Debug)]
struct PermanentStartupError {
    binding: &'static str,
    source: anyhow::Error,
}

impl std::fmt::Display for PermanentStartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "required integration {} is invalid: {}",
            self.binding, self.source
        )
    }
}

impl std::error::Error for PermanentStartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub const DEFAULT_SOCKET_FILE: &str = "agl.sock";

pub fn default_socket_path(state_dir: impl AsRef<Path>) -> PathBuf {
    state_dir.as_ref().join("daemon").join(DEFAULT_SOCKET_FILE)
}

pub struct DaemonConfig {
    pub data_root: PathBuf,
    pub execution_socket: PathBuf,
    pub inference: InferenceConfig,
    pub extensions: Vec<ExtensionBindings>,
    pub searxng: Option<IntegrationConfig<SearxngConfig>>,
    pub max_resident_bytes: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationConfig<T> {
    pub required: bool,
    pub binding: T,
}

#[derive(Clone)]
pub struct DaemonHandle {
    agent: AgentHandle,
    store: StoreHandle,
    runtime: RuntimeHandle,
    execution: ExecutionClient,
    plans: PlanArtifactStore,
}

pub struct DaemonServer {
    handle: DaemonHandle,
    agent_service: Option<AgentService>,
    runtime_service: Option<RuntimeService>,
}

impl DaemonServer {
    pub fn start(config: DaemonConfig) -> Result<Self> {
        let store = StoreHandle::open_at(config.data_root.join("store"))
            .context("failed to open the Agent Store")?;
        let plans = PlanArtifactStore::new(&config.data_root)
            .context("failed to open plan artifact store")?;
        let restored_health = store
            .inference_health()
            .context("failed to restore inference health")?;
        let execution = ExecutionClient::new(config.execution_socket);
        let mut extensions = config.extensions;
        extensions.push(crate::tools::filesystem_bindings());
        extensions.push(crate::tools::execution::bindings(execution.clone()));
        if let Some(search) = config.searxng {
            match crate::tools::searxng::bindings(search.binding) {
                Ok(binding) => extensions.push(binding),
                Err(error) if search.required => {
                    return Err(PermanentStartupError {
                        binding: "search",
                        source: error,
                    }
                    .into());
                }
                Err(error) => {
                    tracing::warn!(
                        health = "degraded",
                        binding = "search",
                        required = false,
                        reason = %error,
                        "optional integration is disabled"
                    );
                }
            }
        }
        let (runtime_service, runtime) = RuntimeService::start(
            RuntimeConfig {
                data_root: config.data_root.join("runtime"),
                inference: config.inference,
                extensions: extensions.clone(),
                max_resident_bytes: config.max_resident_bytes,
                health: {
                    let store = store.clone();
                    agl_runtime::inference::InferenceHealthSink::new(move |updates| {
                        store.put_inference_health_updates(&updates).map_err(|_| ())
                    })
                },
            },
            restored_health,
        )
        .context("failed to start Function runtime")?;
        let (agent_service, agent) = AgentService::start(
            AgentDependencies {
                store: store.clone(),
                inference: Arc::new(runtime.clone()),
            },
            extensions,
        )
        .context("failed to start Agent service")?;
        let handle = DaemonHandle {
            agent,
            store,
            runtime,
            execution,
            plans,
        };
        tracing::info!(
            store_root=%config.data_root.join("store").display(),
            "Agent daemon services started"
        );
        Ok(Self {
            handle,
            agent_service: Some(agent_service),
            runtime_service: Some(runtime_service),
        })
    }

    pub async fn serve(self, source: ListenerSource) -> Result<()> {
        let listener = match source {
            ListenerSource::Bind(path) => bind_listener(&path).await?,
            ListenerSource::Systemd => activation::claim_systemd_listener()?,
        };
        loop {
            let (stream, _) = listener.accept().await?;
            let handle = self.handle.clone();
            tokio::spawn(async move {
                let _ = serve_connection(stream, handle).await;
            });
        }
    }
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        if let Some(agent_service) = self.agent_service.take() {
            agent_service.shutdown();
        }
        if let Some(runtime_service) = self.runtime_service.take() {
            runtime_service.shutdown();
        }
    }
}

async fn bind_listener(path: &Path) -> Result<UnixListener> {
    prepare_socket_path(path)?;
    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(path)?;
        // SAFETY: geteuid has no preconditions and does not mutate process state.
        let expected_uid = unsafe { libc::geteuid() };
        anyhow::ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == expected_uid
                && metadata.mode() & 0o777 == 0o600,
            "daemon socket is not one private same-UID Unix socket"
        );
    }
    Ok(listener)
}

fn prepare_socket_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::Component;

    anyhow::ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|component| !matches!(component, Component::CurDir | Component::ParentDir)),
        "daemon socket path must be one normalized absolute path"
    );
    let parent = path.parent().context("daemon socket path has no parent")?;
    let parent_existed = parent.try_exists()?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    if !parent_existed {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let expected_uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == expected_uid
            && parent_metadata.mode() & 0o777 == 0o700
            && parent.canonicalize()? == parent,
        "daemon socket parent must be canonical, same-UID, and mode 0700"
    );
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_socket()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == expected_uid,
                "existing daemon socket target is not one same-UID Unix socket"
            );
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => anyhow::bail!("daemon socket is already owned by a live process"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path).with_context(|| {
                        format!("failed to remove stale socket {}", path.display())
                    })?;
                }
                Err(error) => return Err(error).context("failed to probe daemon socket owner"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect daemon socket path"),
    }
    Ok(())
}

include!("lib_tests.rs");
