use std::path::PathBuf;
use std::process::Command;

// AGL178-LIVE-001. The dedicated runner executes the real installation,
// user-systemd activation, Terminal process effect and locked 32K Vulkan
// Function. There is no modeled success object: every assertion is made from
// installed files, live protocol responses and inference evidence.
#[test]
#[ignore = "requires a real user systemd manager, exact 31B GGUF and discrete Vulkan GPU"]
fn fresh_installed_product_runs_terminal_effect_and_32k_vulkan_inference() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .unwrap();
    let output = Command::new(root.join("scripts/ci/agl178-live-acceptance.sh"))
        .current_dir(root)
        .output()
        .expect("failed to execute AGL-178 live acceptance runner");
    assert!(
        output.status.success(),
        "live acceptance failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
