use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-terminal-protocol must live below crates/")
        .to_path_buf()
}

fn source(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn rust_sources(relative: &str) -> Vec<(PathBuf, String)> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
            .map(|entry| entry.expect("directory entry must be readable").path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                visit(&path, output);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                output.push(path);
            }
        }
    }

    let root = workspace_root().join(relative);
    let mut paths = Vec::new();
    visit(&root, &mut paths);
    paths
        .into_iter()
        .map(|path| {
            let value = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            (path, value)
        })
        .collect()
}

fn declaration_body<'a>(source: &'a str, declaration: &str) -> &'a str {
    let suffix = source
        .split_once(declaration)
        .unwrap_or_else(|| panic!("missing declaration `{declaration}`"))
        .1;
    suffix.split_once('}').map_or(suffix, |(body, _)| body)
}

// AGL178-TERM-ARCH-001. The terminal repository has one shared parser and
// identity owner; the daemon and UI consume it instead of defining local
// manifest shapes.
#[test]
fn terminal_generation_manifest_has_one_shared_contract_owner() {
    let contract = workspace_root().join("crates/agl-terminal-protocol/src/generation_manifest.rs");
    assert!(
        contract.is_file(),
        "terminal generation manifest contract is missing from agl-terminal-protocol"
    );
    let contract_source = fs::read_to_string(&contract).unwrap();
    for required in [
        "agl-terminal.runtime-generation.v2",
        "TerminalGenerationManifest",
        "TerminalGenerationFileRole",
        "VerifiedTerminalGeneration",
        "deny_unknown_fields",
    ] {
        assert!(
            contract_source.contains(required),
            "shared generation contract is missing `{required}`"
        );
    }
    assert!(!contract_source.contains("runtime-generation.v1"));

    for crate_name in ["agl-terminald", "agl-terminal-ui"] {
        for (path, value) in rust_sources(&format!("crates/{crate_name}/src")) {
            assert!(
                !value.contains("struct TerminalGenerationManifest"),
                "{} duplicates the shared terminal manifest",
                path.display()
            );
        }
    }
}

// AGL178-TERM-PROTO-001. The wire cut is singular: Hello authenticates the
// installed generation before a process ID exists, and the response supplies
// the complete live identity.
#[test]
fn terminal_protocol_is_breaking_pair_first_v2() {
    let protocol = source("crates/agl-terminal-protocol/src/lib.rs");
    for required in [
        "agentlibre.terminal.request.v2alpha",
        "agentlibre.terminal.response.v2alpha",
        "agentlibre.terminal.event.v2alpha",
        "pub const TERMINAL_PROTOCOL_VERSION: u32 = 2",
        "TerminalGenerationIdentity",
        "process_generation_id",
        "expected_generation",
    ] {
        assert!(
            protocol.contains(required),
            "pair-first protocol is missing `{required}`"
        );
    }
    let request = declaration_body(&protocol, "pub struct TerminalRequest");
    assert!(!request.contains("expected_service: ServiceIdentity"));
    let identity = declaration_body(&protocol, "pub struct ServiceIdentity");
    assert!(!identity.contains("pub build_id:"));
    assert!(!identity.contains("pub generation_id:"));
    for obsolete in [
        "agentlibre.terminal.request.v1alpha",
        "agentlibre.terminal.response.v1alpha",
        "agentlibre.terminal.event.v1alpha",
    ] {
        assert!(
            !protocol.contains(obsolete),
            "obsolete wire schema remains: {obsolete}"
        );
    }
}

// AGL178-TERM-ACT-001. Installed identity comes from the executable's sealed
// generation; environment variables cannot assert a build or launcher.
#[test]
fn service_derives_identity_and_launcher_from_its_generation() {
    let daemon = source("crates/agl-terminald/src/lib.rs");
    for forbidden in [
        "AGL_TERMINALD_BUILD_ID",
        "AGL_TERMINALD_LAUNCHER",
        "write_service_identity(&state_root",
    ] {
        assert!(
            !daemon.contains(forbidden),
            "terminal service retains environment/state authority `{forbidden}`"
        );
    }
    for required in [
        "VerifiedTerminalGeneration",
        "AGL_TERMINALD_RUNTIME_ROOT",
        "ListenerReady",
        "remove_service_identity",
    ] {
        assert!(
            daemon.contains(required),
            "terminal readiness path is missing `{required}`"
        );
    }
}

// AGL178-TERM-ACT-002. Descriptor 3 is adopted once inside Tokio and validated
// as the configured accepting Unix listener before identity publication.
#[test]
fn activated_descriptor_is_fully_validated_inside_tokio() {
    let daemon = source("crates/agl-terminald/src/lib.rs");
    let runtime = daemon
        .find("runtime.block_on")
        .expect("terminal daemon must enter its Tokio runtime");
    let adoption = daemon
        .find("adopt_systemd_listener")
        .expect("terminal daemon must have one explicit descriptor adopter");
    assert!(
        runtime < adoption,
        "descriptor adoption still occurs before Tokio"
    );
    for required in [
        "SO_ACCEPTCONN",
        "FD_CLOEXEC",
        "getsockname",
        "remove_var(\"LISTEN_PID\")",
        "remove_var(\"LISTEN_FDS\")",
        "remove_var(\"LISTEN_FDNAMES\")",
    ] {
        assert!(
            daemon.contains(required),
            "activated descriptor validation is missing `{required}`"
        );
    }
}

// AGL178-TERM-PKG-001. Installation and unit generation bind the complete
// immutable generation, never independently switched public links.
#[test]
fn packaging_uses_full_manifest_identity_atomic_pointer_and_immutable_execstart() {
    let install = source("scripts/terminal/install.sh");
    for required in [
        "agl-terminal.runtime-generation.v2",
        "manifest_digest",
        "flock",
        "libexec/agl-terminal/current",
    ] {
        assert!(
            install.contains(required),
            "terminal installer is missing `{required}`"
        );
    }
    assert!(!install.contains("runtime-generation.v1"));
    assert!(!install.contains("generation-${source_revision:0:12}-${service_digest#sha256:}"));

    let systemd = source("scripts/terminal/systemd-user-service.sh");
    assert!(systemd.contains("systemd-analyze verify"));
    assert!(!systemd.contains("ExecStart=$(quote_env \"$service_link\")"));
    assert!(!systemd.contains("AGL_TERMINALD_BUILD_ID="));
    assert!(!systemd.contains("AGL_TERMINALD_LAUNCHER="));
}

// AGL178-TERM-UI-001. The UI is a generation consumer and performs the same
// pair-first live check; a decoded durable state file is not its bootstrap.
#[test]
fn terminal_ui_verifies_its_generation_and_never_bootstraps_from_state() {
    let ui = rust_sources("crates/agl-terminal-ui/src")
        .into_iter()
        .map(|(_, value)| value)
        .collect::<String>();
    for required in [
        "VerifiedTerminalGeneration",
        "TerminalGenerationIdentity",
        "bootstrap",
    ] {
        assert!(ui.contains(required), "terminal UI is missing `{required}`");
    }
    assert!(!ui.contains("state_root.join(\"service-identity.json\")"));
}
