use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-runtime must live below crates/")
        .to_path_buf()
}

fn source(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

// AGL178-AGENT-ARCH-001 / selected 2A. The paired agent manifest advances to
// v3 with a matching hash domain; runtime identity advances to v2 while the
// AGL-173 engine closure remains inside the same manifest.
#[test]
fn agent_manifest_v3_owns_terminal_pair_and_existing_engine_closure() {
    let runtime = source("crates/agl-runtime/src/runtime_manifest.rs");
    for required in [
        "agentlibre.runtime-manifest/v3",
        "agentlibre.runtime-manifest.v3\\0",
        "agentlibre.runtime-identity/v2",
        "pub terminal: TerminalGenerationIdentity",
        "engine_protocol_id",
        "engine_libraries",
    ] {
        assert!(
            runtime.contains(required),
            "runtime manifest is missing `{required}`"
        );
    }
    for obsolete in [
        "agentlibre.runtime-manifest/v2",
        "agentlibre.runtime-manifest.v1\\0",
        "agentlibre.runtime-identity/v1",
    ] {
        assert!(
            !runtime.contains(obsolete),
            "obsolete runtime identity remains: {obsolete}"
        );
    }
}

// AGL178-AGENT-ARCH-002 / selected 1A and 3A. Runtime expectations come from
// the verified current agent manifest and cold connection is live-first; the
// historical service digest and durable state file are not authority.
#[test]
fn terminal_endpoint_uses_manifest_pair_and_runtime_projection() {
    let endpoint = source("crates/agl-process/src/lib.rs");
    assert!(!endpoint.contains("pub const TERMINAL_BUILD_ID"));
    for required in [
        "TerminalGenerationIdentity",
        "expected_generation",
        "bootstrap",
        "process_generation_id",
        "runtime_projection",
    ] {
        assert!(
            endpoint.contains(required),
            "TerminalEndpoint is missing `{required}`"
        );
    }

    let config = source("crates/agl-runtime/src/config.rs");
    assert!(!config.contains("TERMINAL_BUILD_ID"));
    assert!(!config.contains("state_root.join(\"service-identity.json\")"));
    assert!(config.contains("current_runtime_identity"));
    assert!(config.contains("terminal_generation"));
    assert!(config.contains("runtime_root"));
}

// AGL178-AGENT-INSTALL-001 / selected 4A. The exact immutable generation
// directory is mandatory input; no prefix/current/PATH discovery can replace
// it between verification and sealing.
#[test]
fn installer_requires_and_locks_explicit_terminal_generation() {
    let install = source("scripts/install-agl-cargo.sh");
    for required in [
        "--terminal-generation DIR",
        "--terminal-generation)",
        "terminal_generation",
        "terminal_operation_lock",
        "terminal_manifest_digest",
    ] {
        assert!(
            install.contains(required),
            "agent installer is missing `{required}`"
        );
    }
    for forbidden in [
        "--terminal-prefix",
        "command -v agl-terminald",
        "which agl-terminald",
    ] {
        assert!(
            !install.contains(forbidden),
            "installer discovers terminal via `{forbidden}`"
        );
    }
}

// AGL178-AGENT-SYSTEMD-001. A validated generation remains the unit's exact
// executable after later current-pointer changes, and terminal readiness is an
// explicit prerequisite.
#[test]
fn daemon_unit_executes_immutable_agent_generation_after_terminal_readiness() {
    let systemd = source("scripts/agentlibre-daemon-systemd-service.sh");
    assert!(systemd.contains("ExecStart=$(agl_systemd_quote \"$resolved_binary\") serve"));
    assert!(!systemd.contains("ExecStart=$(agl_systemd_quote \"$binary\") serve"));
    assert!(systemd.contains("Requires=$socket_unit agl-terminald.service"));
    assert!(systemd.contains("After=$socket_unit agl-terminald.service"));

    let daemon = source("crates/agl-daemon/src/state.rs");
    assert!(daemon.contains("verify_terminal_handshake"));
    assert!(daemon.contains("terminal_endpoint"));
}

// AGL178-AGENT-ARCH-003. AGL-178 extends the installed identity chain without
// restoring deleted AGL-172/173 worker, launcher, profile or CPU fallback
// surfaces.
#[test]
fn terminal_pairing_preserves_workspace_v3_and_constrained_gpu_engine_cut() {
    let workspace = source("Cargo.toml");
    assert!(!workspace.contains("agl-inference-worker"));
    assert!(!workspace.contains("agl-llama-cpp-sys"));

    let install = source("scripts/install-agl-cargo.sh");
    assert!(install.contains("llama-server"));
    assert!(install.contains("lib*.so"));
    assert!(install.contains("forbidden_public_worker"));
    assert!(install.contains("forbidden_public_launcher"));
}
