use std::path::PathBuf;
use std::process::Command;

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("agl-runtime is below the repository root")
        .to_path_buf()
}

fn run_verification_script(script: &str) {
    let root = repository_root();
    let output = Command::new(root.join(script))
        .current_dir(&root)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {script}: {error}"));
    assert!(
        output.status.success(),
        "{script} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

// AGL178-AGENT-TXN-001/002/004. This executes the real terminal installer
// against a clean fixture repository, injects a publication fault and holds
// the real operation lock while a competing installation is attempted.
#[test]
fn terminal_publication_faults_and_concurrency_use_the_real_installer() {
    run_verification_script("scripts/terminal/ci/agl178-packaging-verification.sh");
}

// AGL178-AGENT-TXN-003. These checks execute the actual agent installer and
// unit renderers against exact generation and hostile systemd surfaces.
#[test]
fn agent_pairing_and_unit_conflicts_use_the_real_scripts() {
    run_verification_script("scripts/ci/agl178-install-verification.sh");
    run_verification_script("scripts/ci/agl178-systemd-verification.sh");
}

// AGL178-AGENT-UNINSTALL-001. The established manifest-aware uninstaller
// matrix exercises owned-surface removal, active-process refusal, lock
// conflicts, path escapes and retained external state with real filesystem
// operations rather than a modeled return value.
#[test]
fn agent_uninstall_ownership_uses_the_real_uninstaller() {
    run_verification_script("scripts/ci/uninstall-bundle.sh");
}
