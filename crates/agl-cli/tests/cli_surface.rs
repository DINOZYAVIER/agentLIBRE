use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const AGL_BIN: &str = env!("CARGO_BIN_EXE_agl");

static TEMP_HOME_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn agl_help_uses_public_alias_and_hides_infer() {
    let output = run_agl(&["--help"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "Usage: agl");
    assert_contains(&stdout, "run");
    assert_contains(&stdout, "generate");
    assert_contains(&stdout, "chat");
    assert!(
        !stdout.contains("Compatibility"),
        "help should not describe a second binary:\n{stdout}"
    );
    assert!(
        !stdout.contains("\n  infer"),
        "hidden infer command should not appear in top-level help:\n{stdout}"
    );
}

#[test]
fn agl_no_arg_help_uses_public_alias() {
    let output = run_agl(&[]);

    assert_success(&output);
    assert_empty_stderr(&output);
    assert_contains(&stdout(&output), "Usage: agl");
}

#[test]
fn version_output_uses_public_alias() {
    let output = run_agl(&["--version"]);

    assert_success(&output);

    assert_eq!(
        version_from_stdout("agl", &stdout(&output)),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn command_help_exits_successfully_for_public_commands() {
    for args in [
        &["chat", "--help"][..],
        &["run", "--help"][..],
        &["generate", "--help"][..],
    ] {
        let output = run_agl(args);

        assert_success(&output);
        assert_empty_stderr(&output);
        assert_contains(&stdout(&output), "Usage: agl");
    }
}

#[test]
fn retired_infer_command_fails_with_run_guidance() {
    let output = run_agl(&["infer", "--config", "local.toml", "--prompt", "hello"]);

    assert_failure(&output);
    assert!(stdout(&output).is_empty(), "stdout should be empty");
    let stderr = stderr(&output);
    assert_contains(&stderr, "agl infer");
    assert_contains(&stderr, "Use `agl run --config PATH PROMPT`");
}

#[test]
fn completion_bash_emits_agl_completion_function() {
    let output = run_agl(&["completion", "bash"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "_agl()");
    assert_contains(&stdout, "complete -F _agl");
    for hidden_command in ["infer", "setup", "doctor", "model"] {
        assert!(
            !stdout.contains(hidden_command),
            "completion should not expose hidden command {hidden_command:?}:\n{stdout}"
        );
    }
}

#[test]
fn home_override_roots_config_paths_in_requested_home() {
    let home = TempHome::new("config-paths");
    let home_arg = home.path_string();
    let output = run_agl(&["--home", &home_arg, "config", "paths"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, &format!("config_dir={home_arg}/config"));
    assert_contains(&stdout, &format!("data_dir={home_arg}/data"));
    assert_contains(&stdout, &format!("state_dir={home_arg}/state"));
    assert_contains(
        &stdout,
        &format!("local_inference_config={home_arg}/config/inference/local.toml"),
    );
    assert_contains(&stdout, &format!("sessions_root={home_arg}/data/sessions"));
}

#[test]
fn chat_rejects_prompt_option_with_clap_error() {
    let output = run_agl(&["chat", "--prompt", "hello"]);

    assert_failure(&output);
    assert!(
        stdout(&output).is_empty(),
        "stdout should be empty on parse error"
    );
    let stderr = stderr(&output);
    assert_contains(&stderr, "unexpected argument '--prompt'");
    assert!(
        !stderr.starts_with("error: error:"),
        "clap errors should not be double-prefixed:\n{stderr}"
    );
}

#[test]
fn chat_new_session_conflict_fails_before_inference_path() {
    let output = run_agl(&[
        "chat",
        "--new-session",
        "--session-id",
        "session-001",
        "--run-id",
        "run-001",
    ]);

    assert_failure(&output);
    assert!(
        stdout(&output).is_empty(),
        "stdout should be empty on parse validation error"
    );
    let stderr = stderr(&output);
    assert_contains(&stderr, "--new-session cannot be used with --session-id");
    assert!(
        !stderr.contains("local inference config"),
        "session flag conflict should not run inference path:\n{stderr}"
    );
}

#[test]
fn reserved_future_commands_fail_before_bare_prompt_execution() {
    for args in [
        &["setup"][..],
        &["doctor"][..],
        &["model", "pull", "owner/repo/model.gguf", "--set-default"][..],
    ] {
        let output = run_agl(args);

        assert_failure(&output);
        assert!(stdout(&output).is_empty(), "stdout should be empty");
        let stderr = stderr(&output);
        assert_contains(&stderr, "planned but not implemented");
        assert!(
            !stderr.contains("local inference config"),
            "reserved command should not run inference path:\n{stderr}"
        );
    }
}

#[test]
fn blank_bare_prompt_fails_before_inference_path() {
    let output = run_agl(&["   "]);

    assert_failure(&output);
    assert!(stdout(&output).is_empty(), "stdout should be empty");
    let stderr = stderr(&output);
    assert_contains(&stderr, "prompt cannot be empty");
    assert!(
        !stderr.contains("local inference config"),
        "blank prompt should not run inference path:\n{stderr}"
    );
}

#[test]
fn missing_default_inference_config_points_to_next_steps() {
    let home = TempHome::new("missing-config");
    let home_arg = home.path_string();
    let output = run_agl(&["--home", &home_arg, "run", "hello"]);

    assert_failure(&output);
    let stderr = stderr(&output);
    assert_contains(&stderr, "local inference config not found");
    assert_contains(&stderr, "Create this file or pass --config PATH");
    assert_contains(&stderr, "agl config paths");
    assert_contains(&stderr, "existing local GGUF file");
    assert!(
        !stderr.contains("No such file or directory"),
        "missing config should not expose raw IO as the primary error:\n{stderr}"
    );
}

#[test]
fn chat_model_failure_records_session_failed_and_exits_unsuccessfully() {
    let home = TempHome::new("chat-model-failure");
    let config_path = home.write_local_inference_config(
        "missing-model.toml",
        "/tmp/agl-cli-surface-missing-model.gguf",
    );
    let home_arg = home.path_string();
    let config_arg = config_path.display().to_string();
    let output = run_agl_with_stdin(
        &[
            "--home",
            &home_arg,
            "chat",
            "--config",
            &config_arg,
            "--run-id",
            "failed-chat-run",
            "--session-id",
            "failed-chat-session",
            "--max-output-tokens",
            "1",
        ],
        "hello\n",
    );

    assert_failure(&output);
    assert_contains(&stdout(&output), "session_id=failed-chat-session");
    assert_contains(&stderr(&output), "model request failed");

    let transcript = fs::read_to_string(
        home.path()
            .join("data")
            .join("sessions")
            .join("failed-chat-session")
            .join("transcript.jsonl"),
    )
    .expect("chat failure should write transcript");
    assert_contains(&transcript, "\"kind\":\"session_failed\"");
}

fn run_agl(args: &[&str]) -> Output {
    Command::new(AGL_BIN)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run agl binary at {AGL_BIN}: {err}"))
}

fn run_agl_with_stdin(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(AGL_BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn agl binary at {AGL_BIN}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(input.as_bytes())
        .expect("failed to write agl stdin");
    child
        .wait_with_output()
        .expect("failed to wait for agl process")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success, got status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure, got success\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_empty_stderr(output: &Output) {
    assert!(
        stderr(output).is_empty(),
        "stderr should be empty:\n{}",
        stderr(output)
    );
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "expected output to contain {needle:?}\noutput:\n{haystack}"
    );
}

fn version_from_stdout<'a>(binary: &str, stdout: &'a str) -> &'a str {
    let mut parts = stdout.split_whitespace();
    assert_eq!(
        parts.next(),
        Some(binary),
        "unexpected version output: {stdout}"
    );
    parts
        .next()
        .unwrap_or_else(|| panic!("missing version in output: {stdout}"))
}

struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn new(label: &str) -> Self {
        let id = TEMP_HOME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agl-cli-surface-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)
            .unwrap_or_else(|err| panic!("failed to create temp home {}: {err}", path.display()));
        Self { path }
    }

    fn path_string(&self) -> String {
        self.path.display().to_string()
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn write_local_inference_config(&self, name: &str, model_path: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(
            &path,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{model_path}"

[runtime]
gpu_layers = 0
context_tokens = 128
threads = 1
batch_size = 16
ubatch_size = 16

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#
            ),
        )
        .unwrap_or_else(|err| {
            panic!(
                "failed to write local inference config {}: {err}",
                path.display()
            )
        });
        path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
