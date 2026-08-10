use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

// AGL171-025.
#[test]
fn cargo_agl_is_the_second_binary_of_agl_cli_and_uses_the_sdk_owner() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let cli = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "agl-cli")
        .unwrap();
    let bins = cli["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter_map(|target| target["name"].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(bins, BTreeSet::from(["agl", "cargo-agl"]));
    assert!(
        cli["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|dependency| dependency["name"] == "agl-extension")
    );
    assert!(
        metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|package| package["name"] != "cargo-agl"),
        "cargo-agl must be a binary target, not a second package"
    );
}
