use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agl_client::{AgentLibreClient, ClientError};
use agl_inference::worker_protocol::WORKER_BUILD_ID;
use agl_protocol::{RuntimeGenerationIdentity, RuntimeGenerationKind};
use agl_runtime::{
    RuntimeSourceEvidence, RuntimeSourceState, current_executable_is_in, current_runtime_identity,
    seal_runtime_manifest,
};
use anyhow::{Context, Result, bail, ensure};

const INTERNAL_SEAL_DIRECTORY: &str = "AGL_INTERNAL_SEAL_RUNTIME_MANIFEST";
const INTERNAL_SOURCE_STATE: &str = "AGL_INTERNAL_RUNTIME_SOURCE_STATE";
const INTERNAL_SOURCE_REVISION: &str = "AGL_INTERNAL_RUNTIME_SOURCE_REVISION";
const INTERNAL_SOURCE_TREE: &str = "AGL_INTERNAL_RUNTIME_SOURCE_TREE";

pub(crate) fn internal_runtime_action() -> Option<Result<()>> {
    env::var_os(INTERNAL_SEAL_DIRECTORY).map(|directory| {
        let directory = PathBuf::from(directory);
        seal_staged_runtime(&directory)
    })
}

pub(crate) fn print_runtime_identity() -> Result<()> {
    crate::print_json(&current_runtime_identity().context("failed to verify runtime identity")?)
}

pub(crate) async fn connect_daemon(socket_path: &Path) -> Result<AgentLibreClient, ClientError> {
    let identity = current_runtime_identity().map_err(|_| {
        ClientError::IdentityMismatch("local agentLIBRE runtime identity could not be verified")
    })?;
    AgentLibreClient::connect_first_party(socket_path, protocol_identity(&identity)).await
}

pub(crate) fn verify_runtime_bundle_identity() -> Result<()> {
    let runtime_identity = agl_runtime::current_runtime_identity()
        .context("runtime manifest identity verification failed")?;
    let runtime = agl_runtime::AgentLibreRuntimeConfig::from_env()?;
    runtime
        .execution
        .terminal_endpoint(&runtime.paths)?
        .read_identity()
        .context("terminal service identity verification failed")?;

    #[cfg(target_os = "linux")]
    {
        let executable = std::env::current_exe().context("failed to resolve current executable")?;
        let parent = executable
            .parent()
            .context("current executable has no parent directory")?;
        let directory = if parent.file_name().is_some_and(|name| name == "deps") {
            parent
                .parent()
                .context("test executable directory has no target parent")?
        } else {
            parent
        };
        let worker_path = directory.join(agl_inference::worker_protocol::WORKER_BINARY_NAME);
        let worker = agl_inference::worker_protocol::WorkerExecutable::open_exact(&worker_path)
            .map_err(anyhow::Error::from)
            .context("inference worker trust verification failed")?;
        let process =
            agl_inference::worker_protocol::WorkerProcess::spawn(&worker, Duration::from_secs(5))
                .map_err(anyhow::Error::from)
                .context("inference worker identity verification failed")?;
        if runtime_identity.sealed() {
            ensure!(
                runtime_identity.worker_build_id() == Some(process.identity().build_id()),
                "sealed runtime manifest worker build ID does not match the exact worker"
            );
            ensure!(
                runtime_identity.native_bundle_id() == Some(process.native_bundle_id()),
                "sealed runtime manifest native bundle ID does not match the exact worker"
            );
        }
        process
            .shutdown(
                agl_inference::worker_protocol::ShutdownReason::Requested,
                Duration::from_secs(5),
            )
            .map_err(anyhow::Error::from)
            .context("inference worker identity verification shutdown failed")?;
    }
    Ok(())
}

fn protocol_identity(identity: &agl_runtime::CurrentRuntimeIdentity) -> RuntimeGenerationIdentity {
    RuntimeGenerationIdentity {
        kind: match identity.kind {
            agl_runtime::RuntimeIdentityKind::Sealed => RuntimeGenerationKind::Sealed,
            agl_runtime::RuntimeIdentityKind::Development => RuntimeGenerationKind::Development,
        },
        generation_id: identity.generation_id.clone(),
        builtin_catalog_digest: identity.builtin_catalog_digest.clone(),
        executable_digest: identity.executable_digest.clone(),
    }
}

fn seal_staged_runtime(directory: &Path) -> Result<()> {
    current_executable_is_in(directory)?;
    let source = source_evidence_from_environment()?;
    let manifest = seal_runtime_manifest(directory, source, WORKER_BUILD_ID)?;
    crate::print_json(&manifest)
}

fn source_evidence_from_environment() -> Result<RuntimeSourceEvidence> {
    let state = required_environment(INTERNAL_SOURCE_STATE)?;
    let state = match state.as_str() {
        "clean" => RuntimeSourceState::Clean,
        "dirty" => RuntimeSourceState::Dirty,
        "unavailable" => RuntimeSourceState::Unavailable,
        _ => bail!("{INTERNAL_SOURCE_STATE} must be one of clean, dirty, or unavailable"),
    };
    let evidence = RuntimeSourceEvidence {
        state,
        revision: optional_environment(INTERNAL_SOURCE_REVISION)?,
        tree: optional_environment(INTERNAL_SOURCE_TREE)?,
    };
    evidence.validate()?;
    Ok(evidence)
}

fn required_environment(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is required for runtime manifest sealing"))
}

fn optional_environment(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}
