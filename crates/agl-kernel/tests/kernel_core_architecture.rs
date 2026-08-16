use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-kernel is under <workspace>/crates")
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

fn packages(metadata: &Value) -> &[Value] {
    metadata["packages"].as_array().unwrap()
}

fn normal_dependencies(metadata: &Value, name: &str) -> BTreeSet<String> {
    packages(metadata)
        .iter()
        .find(|package| package["name"] == name)
        .unwrap_or_else(|| panic!("package {name} exists"))["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|dependency| dependency["kind"].as_str().unwrap_or("normal") == "normal")
        .filter_map(|dependency| dependency["name"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn kernel_dependency_direction_remains_runtime_neutral() {
    let metadata = metadata();
    let kernel = packages(&metadata)
        .iter()
        .find(|package| package["name"] == "agl-kernel")
        .expect("agl-kernel package exists");
    let dependency_kinds = kernel["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dependency| {
            (
                dependency["name"].as_str().unwrap().to_owned(),
                dependency["kind"].as_str().unwrap_or("normal").to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    for forbidden in [
        "agl-app",
        "agl-chat",
        "agl-cli",
        "agl-core-tools",
        "agl-daemon",
        "agl-extension",
        "agl-host-tools",
        "agl-inference",
        "agl-runtime",
        "agl-session",
        "agl-store",
        "agl-supervisor",
        "agl-terminal",
        "agl-terminal-client",
        "agl-terminal-protocol",
        "tokio",
    ] {
        assert!(
            dependency_kinds
                .get(forbidden)
                .is_none_or(|kind| kind == "dev"),
            "agl-kernel has runtime dependency {forbidden}"
        );
    }
}

#[test]
fn package_artifact_and_runtime_dependencies_have_one_direction() {
    let metadata = metadata();
    let names = packages(&metadata)
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();

    for removed in ["agl-turn", "agl-loop", "agl-hooks", "agl-workspace"] {
        assert!(
            !names.contains(removed),
            "removed package remains: {removed}"
        );
    }
    for required in ["agl-kernel", "agl-extension", "agl-package", "agl-artifact"] {
        assert!(
            names.contains(required),
            "required package is absent: {required}"
        );
    }

    assert!(normal_dependencies(&metadata, "agl-extension").contains("agl-kernel"));
    assert!(normal_dependencies(&metadata, "agl-artifact").contains("agl-kernel"));
    assert!(!normal_dependencies(&metadata, "agl-kernel").contains("agl-artifact"));
    assert!(!normal_dependencies(&metadata, "agl-package").contains("agl-artifact"));
    assert!(!normal_dependencies(&metadata, "agl-artifact").contains("agl-repo"));
    assert!(normal_dependencies(&metadata, "agl-repo").contains("agl-artifact"));
    assert!(normal_dependencies(&metadata, "agl-runtime").contains("agl-artifact"));
    assert!(normal_dependencies(&metadata, "agl-runtime").contains("agl-package"));
    assert!(!normal_dependencies(&metadata, "agl-runtime").contains("agl-repo"));
}
