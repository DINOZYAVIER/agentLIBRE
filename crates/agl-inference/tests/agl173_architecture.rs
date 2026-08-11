use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-inference must live under crates/")
        .to_path_buf()
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        {
            let path = entry.expect("directory entry must be readable").path();
            if path.is_dir() {
                visit(&path, output);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                output.push(path);
            }
        }
    }

    let mut output = Vec::new();
    visit(root, &mut output);
    output.sort();
    output
}

fn production_sources(crate_name: &str) -> Vec<(PathBuf, String)> {
    let root = workspace_root().join("crates").join(crate_name).join("src");
    rust_sources(&root)
        .into_iter()
        .map(|path| {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            (path, source)
        })
        .collect()
}

fn occurrences(crate_name: &str, needle: &str) -> Vec<PathBuf> {
    production_sources(crate_name)
        .into_iter()
        .filter_map(|(path, source)| source.contains(needle).then_some(path))
        .collect()
}

fn assert_absent(crate_name: &str, needles: &[&str]) {
    for needle in needles {
        let hits = occurrences(crate_name, needle);
        assert!(
            hits.is_empty(),
            "`{needle}` remains in production crate `{crate_name}`: {hits:?}"
        );
    }
}

// MIW-ARCH-001. The opaque plan is defined and constructed only by agl-model.
#[test]
fn model_execution_plan_has_one_static_owner() {
    let model_hits = occurrences("agl-model", "ModelExecutionPlan");
    assert!(
        !model_hits.is_empty(),
        "agl-model must define the opaque ModelExecutionPlan"
    );

    for crate_name in [
        "agl-cli",
        "agl-chat",
        "agl-daemon",
        "agl-inference",
        "agl-protocol",
    ] {
        for (path, source) in production_sources(crate_name) {
            assert!(
                !source.contains("ModelExecutionPlan {")
                    && !source.contains("ModelExecutionPlan::new_unchecked")
                    && !source.contains("ModelExecutionPlan::from_runtime_config"),
                "{} forges ModelExecutionPlan outside agl-model",
                path.display()
            );
        }
    }
}

// MIW-ARCH-002 and MIW-ARCH-004. Products use the one host for inventory and
// execution; manager/runtime construction is below that facade.
#[test]
fn product_crates_enter_live_inference_only_through_inference_host() {
    for crate_name in ["agl-cli", "agl-chat", "agl-daemon"] {
        let sources = production_sources(crate_name);
        assert!(
            sources
                .iter()
                .any(|(_, source)| source.contains("InferenceHost")),
            "{crate_name} has not been cut over to InferenceHost"
        );
        for (path, source) in sources {
            for forbidden in [
                "ModelManager::spawn",
                "WorkerModelRuntime::",
                "model_device_info(",
                "--list-devices",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{} bypasses InferenceHost through `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

// MIW-ARCH-003, MIW-MODEL-003 and MIW-WRK-001. The selected adapter receives
// an admitted private input, never a public job, path, profile selector or raw
// runtime override.
#[test]
fn selected_engine_adapter_has_no_second_planning_surface() {
    assert_absent(
        "agl-inference",
        &[
            "known_gpu_profile_shape",
            "gpu_profile::",
            "InferencePresetRuntimeConfig::Fixed",
            "ResolvedInferenceConfig",
            "pub struct InferenceJob",
            "pub struct WorkerModelRuntime",
        ],
    );
    assert_absent(
        "agl-protocol",
        &[
            "ResolvedInferenceConfig",
            "RuntimePlan",
            "SetupSmokeRuntimePlan",
        ],
    );
}

// MIW-ENG-001, MIW-SEC-001 and MIW-ID-001. Native inference lives only in the
// pinned subordinate server generation, never in a product process or the
// deleted custom worker/FFI packages.
#[test]
fn native_inference_is_not_linked_into_product_crates() {
    let manifest = fs::read_to_string(workspace_root().join("Cargo.toml")).unwrap();
    assert!(
        !manifest.contains("agl-llama-cpp-sys") && !manifest.contains("agl-inference-worker"),
        "deleted custom worker/FFI packages remain in the workspace"
    );

    for crate_name in ["agl-cli", "agl-chat", "agl-daemon", "agl-inference"] {
        let manifest_path = workspace_root()
            .join("crates")
            .join(crate_name)
            .join("Cargo.toml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        assert!(
            !manifest.contains("agl-llama-cpp-sys") && !manifest.contains("agl-inference-worker"),
            "{} links the deleted native bridge",
            manifest_path.display()
        );
    }

    for crate_name in ["agl-cli", "agl-chat", "agl-daemon"] {
        assert_absent(crate_name, &["llama_cpp_sys", "agl_llama_cpp_sys"]);
    }
}

// MIW-MODEL-001, MIW-MODEL-006 and MIW-MODEL-007. Alpha cutover is singular:
// v2, the fixed config bypass and path/cache discovery cannot remain.
#[test]
fn package_bound_v3_is_the_only_model_runtime_format() {
    let model = production_sources("agl-model");
    assert!(
        model
            .iter()
            .any(|(_, source)| source.contains("agentlibre.model/v3")),
        "agl-model has no v3 payload authority"
    );
    for (path, source) in model {
        for forbidden in [
            "agentlibre.model/v2",
            "InferencePresetRuntimeConfig::Fixed",
            "cpu_fallback",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} retains legacy `{forbidden}` behavior",
                path.display()
            );
        }
    }
}

// MIW-TXN-005. Records and bindings have one durable transaction owner.
#[test]
fn model_bindings_have_no_independent_commit_bypass() {
    for (path, source) in production_sources("agl-model") {
        assert!(
            !source.contains("impl ModelBindingPatch") || !source.contains("pub fn commit("),
            "{} still exposes ModelBindingPatch::commit",
            path.display()
        );
        assert!(
            !source.contains("pub fn commit_with_bindings("),
            "{} still exposes the pre-journal split commit",
            path.display()
        );
    }
}

// Selected 3A and 13A: downloader terminology is exact and the old catch-all
// worker owner no longer exists.
#[test]
fn model_downloader_name_and_owner_modules_are_unambiguous() {
    let model_src = workspace_root().join("crates/agl-model/src");
    assert!(model_src.join("downloader.rs").is_file());
    assert!(!model_src.join("worker.rs").exists());
    assert_absent("agl-model", &["ModelDownloadWorker", "worker_main"]);
}
