use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-inference must live under crates/")
        .to_path_buf()
}

fn metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root())
        .output()
        .expect("cargo metadata starts");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata returns JSON")
}

fn normal_dependencies(metadata: &Value, package: &str) -> BTreeSet<String> {
    metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["name"] == package)
        .unwrap_or_else(|| panic!("package {package} exists"))["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|dependency| dependency["kind"].as_str().unwrap_or("normal") == "normal")
        .filter_map(|dependency| dependency["name"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn product_graph_excludes_deleted_native_worker_packages() {
    let metadata = metadata();
    let packages = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();

    for removed in ["agl-llama-cpp-sys", "agl-inference-worker"] {
        assert!(
            !packages.contains(removed),
            "removed package remains: {removed}"
        );
    }

    for product in ["agl-cli", "agl-chat", "agl-daemon", "agl-inference"] {
        let dependencies = normal_dependencies(&metadata, product);
        for removed in ["agl-llama-cpp-sys", "agl-inference-worker"] {
            assert!(
                !dependencies.contains(removed),
                "{product} depends on removed package {removed}"
            );
        }
    }
}
