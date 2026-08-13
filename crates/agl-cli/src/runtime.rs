use std::env;
use std::path::{Path, PathBuf};

use agl_client::{AgentLibreClient, ClientError};
use agl_inference::{
    ENGINE_BINARY_NAME, ENGINE_PROTOCOL_ID, EngineExecutable, InferenceHost, InferenceHostConfig,
};
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
const INTERNAL_TERMINAL_GENERATION: &str = "AGL_INTERNAL_TERMINAL_GENERATION";

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
        let engine_path = directory.join(ENGINE_BINARY_NAME);
        let expected = runtime_identity
            .manifest
            .as_ref()
            .and_then(|manifest| {
                manifest
                    .content
                    .executables
                    .iter()
                    .find(|file| file.path == ENGINE_BINARY_NAME)
            })
            .map(|file| engine_host_sha256(&file.sha256))
            .transpose()?
            .context("runtime manifest has no exact llama-server executable")?;
        let host = InferenceHost::start(InferenceHostConfig {
            executable: EngineExecutable {
                path: engine_path,
                sha256: expected,
            },
            queue_capacity: 1,
            external_host_reserve_bytes: 0,
            authority_root: runtime
                .paths
                .inference_state_root()
                .join("identity-authority"),
            context_idle_duration: std::time::Duration::from_secs(
                runtime.inference.residency.context_idle_seconds,
            ),
            model_idle_duration: std::time::Duration::from_secs(
                runtime.inference.residency.model_idle_seconds,
            ),
            evidence_root: runtime.paths.default_artifact_root(),
        })
        .context("inference engine identity verification failed")?;
        if runtime_identity.sealed() {
            ensure!(
                runtime_identity.engine_protocol_id() == Some(ENGINE_PROTOCOL_ID),
                "sealed runtime manifest engine protocol ID does not match this host"
            );
        }
        host.shutdown();
    }
    Ok(())
}

fn engine_host_sha256(canonical: &str) -> Result<String> {
    let digest = canonical
        .strip_prefix("sha256:")
        .context("engine digest is not a canonical sha256 identity")?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "engine digest is not a canonical lowercase SHA-256 identity"
    );
    Ok(digest.to_owned())
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
    let terminal_directory = PathBuf::from(required_environment(INTERNAL_TERMINAL_GENERATION)?);
    let terminal =
        agl_terminal_protocol::VerifiedTerminalGeneration::load_installed(&terminal_directory)
            .context(
                "failed to verify the selected terminal generation while sealing agent runtime",
            )?;
    let manifest = seal_runtime_manifest(
        directory,
        source,
        ENGINE_PROTOCOL_ID,
        terminal.identity().clone(),
    )?;
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

#[cfg(test)]
mod tests {
    use super::engine_host_sha256;

    #[test]
    fn sealed_digest_conversion_is_checked_and_removes_only_the_identity_prefix() {
        let hex = "a".repeat(64);
        assert_eq!(engine_host_sha256(&format!("sha256:{hex}")).unwrap(), hex);
        assert!(engine_host_sha256(&"a".repeat(64)).is_err());
        assert!(engine_host_sha256(&format!("sha256:{}", "A".repeat(64))).is_err());
    }
}
