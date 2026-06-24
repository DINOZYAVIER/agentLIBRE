use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const BRIDGE_BIN: &str = env!("CARGO_BIN_EXE_agl-matrix-bridge");
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn check_config_passes_without_printing_secrets() {
    let temp = TempDir::new("valid-config");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
access_token = "secret-token"

[agl]
socket_path = "/tmp/agl.sock"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@user:example"]

[bindings]
path = "/tmp/agl-matrix-bindings.json"
"#,
    );

    let output = run_bridge(&["check-config", "--config", &config.display().to_string()]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "config=ok");
    assert_contains(&stdout, "store_path_configured=false");
    assert_contains(&stdout, "allowed_rooms=1");
    assert_contains(&stdout, "allowed_users=1");
    assert!(
        !stdout.contains("secret-token"),
        "check-config must not print Matrix access token:\n{stdout}"
    );
}

#[test]
fn check_config_fails_closed_without_access_policy() {
    let temp = TempDir::new("empty-access");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
access_token = "secret-token"
"#,
    );

    let output = run_bridge(&["check-config", "--config", &config.display().to_string()]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "MissingAccessPolicy");
}

#[test]
fn check_config_rejects_unknown_fields() {
    let temp = TempDir::new("unknown-field");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
access_token = "secret-token"
surprise = true

[access]
allowed_rooms = ["!room:example"]
"#,
    );

    let output = run_bridge(&["check-config", "--config", &config.display().to_string()]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "unknown field");
}

#[test]
fn check_config_requires_store_for_encrypted_room_allow_policy() {
    let temp = TempDir::new("encrypted-without-store");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
access_token = "secret-token"
encrypted_rooms = "allow-decrypted"

[access]
allowed_rooms = ["!room:example"]
"#,
    );

    let output = run_bridge(&["check-config", "--config", &config.display().to_string()]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "MissingStorePathForEncryptedRooms");
}

#[test]
fn handle_test_event_denies_before_daemon_connect() {
    let temp = TempDir::new("handle-denied");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
access_token = "secret-token"

[agl]
socket_path = "/tmp/agl-matrix-bridge-test-missing.sock"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@allowed:example"]
"#,
    );

    let output = run_bridge(&[
        "handle-test-event",
        "--config",
        &config.display().to_string(),
        "--room",
        "!room:example",
        "--sender",
        "@denied:example",
        "--event",
        "$event",
        "--thread",
        "$thread",
        "--body",
        "!agl send hello",
    ]);

    assert_success(&output);
    assert_contains(&stdout(&output), "action=ignore reason=user is not allowed");
}

#[test]
fn sync_requires_device_id_before_daemon_connect() {
    let temp = TempDir::new("sync-missing-device");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
access_token = "secret-token"

[agl]
socket_path = "/tmp/agl-matrix-bridge-test-missing.sock"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@user:example"]
"#,
    );

    let output = run_bridge(&["sync", "--config", &config.display().to_string()]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "matrix.device_id is required");
}

#[test]
fn sync_requires_session_or_access_token_before_network() {
    let temp = TempDir::new("sync-missing-session-and-token");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"

[agl]
socket_path = "/tmp/agl-matrix-bridge-test-missing.sock"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@user:example"]
"#,
    );

    let output = run_bridge(&["sync", "--config", &config.display().to_string()]);

    assert_failure(&output);
    assert_contains(
        &stderr(&output),
        "matrix.access_token is required when matrix.session_path is not set",
    );
}

#[test]
fn login_password_requires_env_before_network() {
    let temp = TempDir::new("login-missing-env");
    let session = temp.path.join("session.json");
    let config = temp.write(
        "bridge.toml",
        &format!(
            r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
session_path = "{}"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@user:example"]
"#,
            session.display()
        ),
    );

    let output = run_bridge_without_matrix_env(&[
        "login-password",
        "--config",
        &config.display().to_string(),
    ]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "AGL_MATRIX_USERNAME is required");
}

#[test]
fn verify_device_fails_closed_until_interactive_verification_exists() {
    let temp = TempDir::new("verify-device-placeholder");
    let store = temp.path.join("matrix-store");
    let config = temp.write(
        "bridge.toml",
        &format!(
            r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"
store_path = "{}"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@user:example"]
"#,
            store.display()
        ),
    );

    let output = run_bridge(&[
        "verify-device",
        "--config",
        &config.display().to_string(),
        "--user-id",
        "@user:example",
        "--device-id",
        "DEVICE",
    ]);

    assert_failure(&output);
    assert_contains(
        &stderr(&output),
        "Matrix device verification is not implemented in this alpha",
    );
}

#[test]
fn verify_device_requires_store_path_before_interactive_work() {
    let temp = TempDir::new("verify-device-missing-store");
    let config = temp.write(
        "bridge.toml",
        r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@agl:example"

[access]
allowed_rooms = ["!room:example"]
allowed_users = ["@user:example"]
"#,
    );

    let output = run_bridge(&[
        "verify-device",
        "--config",
        &config.display().to_string(),
        "--user-id",
        "@user:example",
        "--device-id",
        "DEVICE",
    ]);

    assert_failure(&output);
    assert_contains(
        &stderr(&output),
        "matrix.store_path is required for Matrix device verification",
    );
}

fn run_bridge(args: &[&str]) -> Output {
    Command::new(BRIDGE_BIN)
        .args(args)
        .output()
        .expect("failed to run agl-matrix-bridge")
}

fn run_bridge_without_matrix_env(args: &[&str]) -> Output {
    Command::new(BRIDGE_BIN)
        .args(args)
        .env_remove("AGL_MATRIX_USERNAME")
        .env_remove("AGL_MATRIX_PASSWORD")
        .env_remove("AGL_MATRIX_DEVICE_DISPLAY_NAME")
        .output()
        .expect("failed to run agl-matrix-bridge")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "expected output to contain {needle:?}\noutput:\n{haystack}"
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-test-{}-{id}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create temp dir");
        Self { path }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, content).expect("failed to write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
