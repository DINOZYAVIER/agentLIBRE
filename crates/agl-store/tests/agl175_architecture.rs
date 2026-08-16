use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-store is under <workspace>/crates")
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

fn package_names(metadata: &Value) -> BTreeSet<&str> {
    metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect()
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
fn domain_packages_do_not_depend_on_sqlite_or_the_shared_store() {
    let metadata = metadata();
    let packages = package_names(&metadata);
    for package in [
        "agl-memory",
        "agl-note",
        "agl-cron",
        "agl-permission",
        "agl-matrix",
        "agl-content",
        "agl-artifact",
        "agl-kernel",
    ] {
        assert!(
            packages.contains(package),
            "missing domain package {package}"
        );
        let dependencies = normal_dependencies(&metadata, package);
        for forbidden in ["agl-store", "rusqlite"] {
            assert!(
                !dependencies.contains(forbidden),
                "{package} depends on persistence package {forbidden}"
            );
        }
    }
    assert!(!packages.contains("agl-notes"));
}

#[test]
fn shared_store_and_consumers_keep_the_selected_dependency_direction() {
    let metadata = metadata();
    let store_dependencies = normal_dependencies(&metadata, "agl-store");
    for forbidden in [
        "agl-inference",
        "agl-matrix-bridge",
        "agl-model",
        "agl-session",
        "agl-terminald",
        "matrix-sdk",
    ] {
        assert!(
            !store_dependencies.contains(forbidden),
            "agl-store depends on independent persistence owner {forbidden}"
        );
    }

    for consumer in [
        "agl-chat",
        "agl-cli",
        "agl-core-tools",
        "agl-daemon",
        "agl-host-tools",
        "agl-inference",
        "agl-matrix-bridge",
        "agl-supervisor",
    ] {
        assert!(
            !normal_dependencies(&metadata, consumer).contains("agl-store"),
            "{consumer} depends directly on agl-store"
        );
    }
}
