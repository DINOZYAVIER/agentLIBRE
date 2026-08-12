use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn adapter_source() -> String {
    ["process.rs", "request.rs", "transport.rs"]
        .into_iter()
        .map(|file| {
            fs::read_to_string(root().join("crates/agl-inference/src/engine").join(file)).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// MIW-ENG-001, MIW-ENG-007 and MIW-ENG-015.
#[test]
fn private_server_has_only_inherited_agl_routes_and_closed_environment() {
    let server =
        fs::read_to_string(root().join("vendor/llama.cpp/tools/server/server.cpp")).unwrap();
    let http =
        fs::read_to_string(root().join("vendor/llama.cpp/tools/server/server-http.cpp")).unwrap();
    let adapter = adapter_source();
    for route in [
        "/agl/v1/readiness",
        "/agl/v1/inventory",
        "/agl/v1/generate",
        "/agl/v1/control",
        "/agl/v1/slot/:id_slot",
    ] {
        assert!(server.contains(route));
    }
    assert!(server.contains("if (ctx_http.is_private_inherited())"));
    assert!(http.contains("AGL_LLAMA_SERVER_LISTEN_FD"));
    assert!(adapter.contains(".env_clear()"));
    assert!(!adapter.contains("LLAMA_ARG_"));
}

// MIW-ENG-003, MIW-ENG-004, MIW-ENG-010 and MIW-ENG-011.
#[test]
fn adapter_forces_exact_runtime_shape_and_typed_readiness() {
    let source = adapter_source();
    for exact in [
        "--ctx-size",
        "--batch-size",
        "--ubatch-size",
        "--gpu-layers",
        "--fit",
        "--parallel",
        "--no-context-shift",
        "--reasoning-format",
        "agentlibre.llama-readiness/v1",
    ] {
        assert!(source.contains(exact), "missing exact engine field {exact}");
    }
}

// MIW-ENG-005, MIW-ENG-008, MIW-ENG-009 and MIW-OBS-001.
#[test]
fn private_generation_is_bounded_identity_checked_streaming() {
    let adapter = adapter_source();
    let server =
        fs::read_to_string(root().join("vendor/llama.cpp/tools/server/server-context.cpp"))
            .unwrap();
    let http =
        fs::read_to_string(root().join("vendor/llama.cpp/tools/server/server-http.h")).unwrap();
    for contract in [
        "agentlibre.llama-stream/v1",
        "application/x-ndjson",
        "MAX_STREAM_FRAME_BYTES",
        "MAX_STREAM_WIRE_BYTES",
        "cancel_attempt",
        "configured_batch_size",
        "prefill_chunks",
    ] {
        assert!(
            adapter.contains(contract) || server.contains(contract),
            "missing private stream contract {contract}"
        );
    }
    assert!(
        http.contains("std::function<bool()> should_stop;"),
        "stream cancellation callback must be owned by the request"
    );
    assert!(!http.contains("std::function<bool()> & should_stop"));
}

// MIW-TOOL-001 and MIW-PROMPT-001. The private adapter supplies one ordered
// engine-neutral message/Tool input and forces llama.cpp's pinned template and
// sequential automatic Tool grammar path.
#[test]
fn prompt_and_tool_policy_have_one_native_render_boundary() {
    let adapter = adapter_source();
    for contract in [
        "\"messages\"",
        "\"tools\"",
        "\"tool_choice\"",
        "\"parallel_tool_calls\"",
        "--jinja",
        "--reasoning",
    ] {
        assert!(
            adapter.contains(contract),
            "missing prompt/Tool contract {contract}"
        );
    }
}

// MIW-FD-001, MIW-FD-002, MIW-FD-003, MIW-FD-004, MIW-FD-005 and MIW-ENG-002.
#[test]
fn adapter_maps_verified_open_descriptions_not_source_paths() {
    let source = adapter_source();
    assert!(source.contains("/proc/self/fd/"));
    assert!(source.contains("VerifiedDescriptorSet"));
    assert!(
        source.contains("O_NOFOLLOW")
            || fs::read_to_string(root().join("crates/agl-inference/src/host/descriptors.rs"))
                .unwrap()
                .contains("O_NOFOLLOW")
    );
}

// MIW-SEC-001, MIW-ENG-001 and MIW-ENG-015.
#[test]
fn launcher_applies_kernel_sandbox_before_exec() {
    let process =
        fs::read_to_string(root().join("crates/agl-inference/src/engine/process.rs")).unwrap();
    let sandbox =
        fs::read_to_string(root().join("crates/agl-inference/src/engine/sandbox.rs")).unwrap();
    assert!(process.contains("sandbox.enter()?"));
    for mechanism in [
        "PR_SET_PDEATHSIG",
        "PR_SET_NO_NEW_PRIVS",
        "landlock_restrict_self",
        "SECCOMP_MODE_FILTER",
        "RLIMIT_NOFILE",
        "RLIMIT_NPROC",
        "dynamic_runtime_files",
    ] {
        assert!(
            process.contains(mechanism) || sandbox.contains(mechanism),
            "missing engine isolation mechanism {mechanism}"
        );
    }
    assert!(!process.contains("Stdio::inherit"));
}
