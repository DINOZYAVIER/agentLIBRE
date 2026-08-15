use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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

fn normal_dependencies(package: &str) -> BTreeSet<String> {
    let metadata = metadata();
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

fn production_rs_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
            .map(|entry| entry.expect("directory entry is readable").path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                if entry.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                visit(&entry, output);
            } else if entry.extension().is_some_and(|extension| extension == "rs")
                && entry.file_name().is_none_or(|name| name != "tests.rs")
                && !entry
                    .components()
                    .any(|component| component.as_os_str() == "tests")
            {
                output.push(entry);
            }
        }
    }

    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

fn source_matches(root: &Path, needles: &[&str]) -> Vec<String> {
    let mut matches = Vec::new();
    for path in production_rs_files(root) {
        let source = fs::read_to_string(&path).unwrap();
        for (line_number, line) in source.lines().enumerate() {
            if let Some(needle) = needles.iter().find(|needle| line.contains(**needle)) {
                matches.push(format!(
                    "{}:{}:{needle}",
                    path.strip_prefix(workspace_root()).unwrap().display(),
                    line_number + 1
                ));
            }
        }
    }
    matches
}

#[test]
fn domain_crates_own_contracts_without_sqlite_or_store_dependencies() {
    let metadata = metadata();
    let packages = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();
    for expected in [
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
            packages.contains(expected),
            "missing domain package {expected}"
        );
    }
    assert!(
        !packages.contains("agl-notes"),
        "plural domain package survived"
    );

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
        let dependencies = normal_dependencies(package);
        for forbidden in ["agl-store", "rusqlite"] {
            assert!(
                !dependencies.contains(forbidden),
                "{package} depends on forbidden persistence package {forbidden}"
            );
        }
    }
}

#[test]
fn store_does_not_absorb_independent_persistence_owners() {
    let dependencies = normal_dependencies("agl-store");
    for forbidden in [
        "agl-inference",
        "agl-matrix-bridge",
        "agl-model",
        "agl-session",
        "agl-terminald",
        "matrix-sdk",
    ] {
        assert!(
            !dependencies.contains(forbidden),
            "agl-store absorbed independent owner {forbidden}"
        );
    }
}

#[test]
fn raw_store_construction_has_one_production_owner() {
    let root = workspace_root().join("crates");
    let mut violations = BTreeMap::<String, Vec<String>>::new();
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        let package = path.file_name().unwrap().to_string_lossy().to_string();
        if matches!(package.as_str(), "agl-store" | "agl-runtime") {
            continue;
        }
        let matches = source_matches(
            &path.join("src"),
            &[
                "AglStore::open_at",
                "AglStore::open_current_at",
                "AglStore::open_current_read_only_at",
                "AglStore::migrate_at",
            ],
        );
        if !matches.is_empty() {
            violations.insert(package, matches);
        }
    }
    assert!(
        violations.is_empty(),
        "production crates open concrete store outside agl-runtime: {violations:#?}"
    );
}

#[test]
fn consumers_do_not_depend_on_concrete_store() {
    for package in [
        "agl-chat",
        "agl-cli",
        "agl-core-tools",
        "agl-daemon",
        "agl-host-tools",
        "agl-matrix-bridge",
        "agl-supervisor",
    ] {
        let dependencies = normal_dependencies(package);
        assert!(
            !dependencies.contains("agl-store"),
            "{package} still depends on concrete agl-store"
        );
    }
    assert!(
        !normal_dependencies("agl-inference").contains("agl-store"),
        "inference regained store authority"
    );
}

#[test]
fn store_public_surface_contains_no_permission_or_matrix_domain_records() {
    let source = fs::read_to_string(workspace_root().join("crates/agl-store/src/types.rs"))
        .expect("store types source exists");
    for forbidden in [
        "pub enum PermissionRequestStatus",
        "pub enum PermissionGrantStatus",
        "pub struct PermissionRequestRecord",
        "pub struct PermissionGrantRecord",
        "pub enum MatrixNotificationOutboxStatus",
        "pub struct MatrixNotificationOutboxItem",
    ] {
        assert!(!source.contains(forbidden), "store still owns {forbidden}");
    }
}

#[test]
fn raw_connection_and_transaction_are_not_public() {
    let source = fs::read_to_string(workspace_root().join("crates/agl-store/src/connection.rs"))
        .expect("store connection source exists");
    assert!(!source.contains("pub fn connection("));
    assert!(!source.contains("pub fn transaction<"));
}
