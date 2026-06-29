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
    assert_contains(&stdout, "init");
    assert_contains(&stdout, "chat");
    assert_contains(&stdout, "serve");
    assert_contains(&stdout, "status");
    assert_contains(&stdout, "skill");
    assert_contains(&stdout, "cron");
    assert_contains(&stdout, "memory");
    assert_contains(&stdout, "notes");
    assert_contains(&stdout, "install-hooks");
    assert!(
        !stdout.contains("Compatibility"),
        "help should not describe a second binary:\n{stdout}"
    );
    assert!(
        !stdout.contains("\n  infer"),
        "hidden infer command should not appear in top-level help:\n{stdout}"
    );
    for hidden_command in ["repo", "daemon"] {
        assert!(
            !stdout.contains(&format!("\n  {hidden_command}")),
            "hidden command should not appear in top-level help:\n{stdout}"
        );
    }
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
        &["init", "--help"][..],
        &["install-hooks", "--help"][..],
        &["run", "--help"][..],
        &["generate", "--help"][..],
        &["serve", "--help"][..],
        &["status", "--help"][..],
        &["skill", "--help"][..],
        &["skill", "list", "--help"][..],
        &["skill", "inspect", "--help"][..],
        &["skill", "verify", "--help"][..],
        &["skill", "trust", "--help"][..],
        &["skill", "revoke", "--help"][..],
        &["cron", "--help"][..],
        &["cron", "add", "--help"][..],
        &["cron", "list", "--help"][..],
        &["cron", "show", "--help"][..],
        &["cron", "enable", "--help"][..],
        &["cron", "disable", "--help"][..],
        &["cron", "run", "--help"][..],
        &["cron", "history", "--help"][..],
        &["cron", "delete", "--help"][..],
        &["memory", "--help"][..],
        &["memory", "add", "--help"][..],
        &["memory", "list", "--help"][..],
        &["memory", "search", "--help"][..],
        &["memory", "show", "--help"][..],
        &["memory", "delete", "--help"][..],
        &["notes", "--help"][..],
        &["notes", "add", "--help"][..],
        &["notes", "list", "--help"][..],
        &["notes", "search", "--help"][..],
        &["notes", "show", "--help"][..],
        &["notes", "update", "--help"][..],
        &["notes", "delete", "--help"][..],
        &["notes", "link", "--help"][..],
        &["notes", "remember", "--help"][..],
    ] {
        let output = run_agl(args);

        assert_success(&output);
        assert_empty_stderr(&output);
        assert_contains(&stdout(&output), "Usage: agl");
    }
}

#[test]
fn memory_commands_manage_explicit_user_memory() {
    let home = TempHome::new("memory-commands");
    let home_arg = home.path_string();

    let add = run_agl(&[
        "--home",
        &home_arg,
        "memory",
        "add",
        "--kind",
        "preference",
        "--title",
        "Commit style",
        "--body",
        "Use imperative subjects.",
    ]);

    assert_success(&add);
    let add_stdout = stdout(&add);
    assert_contains(&add_stdout, "memory id=");
    assert_contains(&add_stdout, "scope=user");
    assert_contains(&add_stdout, "kind=preference");
    let id = memory_id_from_output(&add_stdout);

    let list = run_agl(&["--home", &home_arg, "memory", "list"]);
    assert_success(&list);
    assert_contains(&stdout(&list), &id);

    let search = run_agl(&["--home", &home_arg, "memory", "search", "imperative"]);
    assert_success(&search);
    assert_contains(&stdout(&search), &id);

    let show = run_agl(&["--home", &home_arg, "memory", "show", &id]);
    assert_success(&show);
    assert_contains(&stdout(&show), "memory.");
    assert_contains(&stdout(&show), "Use imperative subjects.");

    let delete = run_agl(&["--home", &home_arg, "memory", "delete", &id]);
    assert_success(&delete);
    assert_contains(&stdout(&delete), "memory.deleted=true");

    let hidden = run_agl(&["--home", &home_arg, "memory", "list"]);
    assert_success(&hidden);
    assert!(
        !stdout(&hidden).contains(&id),
        "deleted memory should be hidden by default"
    );

    let include_deleted = run_agl(&["--home", &home_arg, "memory", "list", "--include-deleted"]);
    assert_success(&include_deleted);
    assert_contains(&stdout(&include_deleted), &id);
    assert_contains(&stdout(&include_deleted), "deleted=true");
}

#[test]
fn notes_commands_manage_notes_and_promote_memory() {
    let home = TempHome::new("notes-commands");
    let home_arg = home.path_string();

    let add = run_agl(&[
        "--home",
        &home_arg,
        "notes",
        "add",
        "--title",
        "Repo workflow",
        "--body",
        "Use pinned workspace skills.",
    ]);

    assert_success(&add);
    let add_stdout = stdout(&add);
    assert_contains(&add_stdout, "note id=");
    let id = note_id_from_output(&add_stdout);

    let search = run_agl(&["--home", &home_arg, "notes", "search", "pinned"]);
    assert_success(&search);
    assert_contains(&stdout(&search), &id);

    let update = run_agl(&[
        "--home",
        &home_arg,
        "notes",
        "update",
        &id,
        "--body",
        "Use pinned trusted workspace skills.",
    ]);
    assert_success(&update);

    let show = run_agl(&["--home", &home_arg, "notes", "show", &id]);
    assert_success(&show);
    assert_contains(&stdout(&show), "Use pinned trusted workspace skills.");

    let remember = run_agl(&["--home", &home_arg, "notes", "remember", &id]);
    assert_success(&remember);
    let remember_stdout = stdout(&remember);
    assert_contains(&remember_stdout, "note.remembered=true");
    assert_contains(&remember_stdout, "memory id=");
    assert_contains(&remember_stdout, "note_link id=");

    let memory = run_agl(&["--home", &home_arg, "memory", "search", "trusted"]);
    assert_success(&memory);
    assert_contains(&stdout(&memory), "scope=user");
    assert_contains(&stdout(&memory), "kind=working_note");

    let delete = run_agl(&["--home", &home_arg, "notes", "delete", &id]);
    assert_success(&delete);
    assert_contains(&stdout(&delete), "note.deleted=true");

    let hidden = run_agl(&["--home", &home_arg, "notes", "list"]);
    assert_success(&hidden);
    assert!(
        !stdout(&hidden).contains(&id),
        "deleted note should be hidden by default"
    );
}

#[test]
fn cron_commands_manage_builtin_jobs_and_run_history() {
    let home = TempHome::new("cron-commands");
    let home_arg = home.path_string();

    let add = run_agl(&[
        "--home",
        &home_arg,
        "cron",
        "add",
        "--name",
        "Store status",
        "--schedule",
        "0 9 * * *",
        "--builtin",
        "store-status",
        "--notify",
        "matrix-room:!status",
    ]);

    assert_success(&add);
    let add_stdout = stdout(&add);
    assert_contains(&add_stdout, "cron id=");
    assert_contains(&add_stdout, "target=builtin:store-status");
    assert_contains(&add_stdout, "enabled=true");
    let id = cron_id_from_output(&add_stdout);

    let list = run_agl(&["--home", &home_arg, "cron", "list"]);
    assert_success(&list);
    assert_contains(&stdout(&list), &id);

    let show = run_agl(&["--home", &home_arg, "cron", "show", &id]);
    assert_success(&show);
    assert_contains(&stdout(&show), "notify_ref=matrix-room:!status");

    let disable = run_agl(&["--home", &home_arg, "cron", "disable", &id]);
    assert_success(&disable);
    assert_contains(&stdout(&disable), "enabled=false");

    let enable = run_agl(&["--home", &home_arg, "cron", "enable", &id]);
    assert_success(&enable);
    assert_contains(&stdout(&enable), "enabled=true");

    let run = run_agl(&["--home", &home_arg, "cron", "run", &id, "--now"]);
    assert_success(&run);
    let run_stdout = stdout(&run);
    assert_contains(&run_stdout, "cron_run id=");
    assert_contains(&run_stdout, "status=succeeded");
    assert_contains(&run_stdout, "result_ref=builtin:store-status:schema:");

    let history = run_agl(&["--home", &home_arg, "cron", "history", &id]);
    assert_success(&history);
    assert_contains(&stdout(&history), "status=succeeded");

    let delete = run_agl(&["--home", &home_arg, "cron", "delete", &id]);
    assert_success(&delete);
    assert_contains(&stdout(&delete), "cron.deleted=true");

    let hidden = run_agl(&["--home", &home_arg, "cron", "list"]);
    assert_success(&hidden);
    assert!(
        !stdout(&hidden).contains(&id),
        "deleted cron job should be hidden by default"
    );
}

#[test]
fn cron_add_rejects_invalid_schedule() {
    let home = TempHome::new("cron-invalid-schedule");
    let home_arg = home.path_string();

    let output = run_agl(&[
        "--home",
        &home_arg,
        "cron",
        "add",
        "--name",
        "Bad schedule",
        "--schedule",
        "daily 99:99",
        "--builtin",
        "store-status",
    ]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "invalid cron schedule_expr value");
}

#[test]
fn run_help_describes_trusted_workspace_skills() {
    let output = run_agl(&["run", "--help"]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "Builtin or trusted workspace skill id");
}

#[test]
fn hidden_repo_help_remains_available_for_advanced_usage() {
    let output = run_agl(&["repo", "--help"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "Usage: agl repo");
    assert_contains(&stdout, "init");
    assert_contains(&stdout, "status");
    assert_contains(&stdout, "install-hooks");
}

#[test]
fn status_without_workspace_manifest_points_to_init() {
    let repo = TempRepo::new("missing-workspace-manifest");
    let output = run_agl_in(repo.path(), &["status"]);

    assert_failure(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=invalid");
    assert_contains(&stdout, "error=workspace_manifest_missing");
    assert_contains(&stdout, "next_step=agl init");
}

#[test]
fn logging_init_failure_warns_without_panicking() {
    let repo = TempRepo::new("bad-log-directory");
    let home = TempHome::new("bad-log-directory");
    let state_dir = home.path().join("state");
    fs::create_dir_all(&state_dir)
        .unwrap_or_else(|err| panic!("failed to create state dir {}: {err}", state_dir.display()));
    fs::write(state_dir.join("logs"), "not a directory").unwrap_or_else(|err| {
        panic!(
            "failed to create invalid logs path in {}: {err}",
            state_dir.display()
        )
    });
    let home_arg = home.path_string();

    let output = run_agl_in(repo.path(), &["--home", &home_arg, "status"]);

    assert_failure(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=invalid");
    assert_contains(&stdout, "error=workspace_manifest_missing");
    let stderr = stderr(&output);
    assert_contains(&stderr, "warning: failed to initialize logging");
    assert_contains(&stderr, "failed to create log directory");
    assert!(
        !stderr.contains("panicked at"),
        "logging fallback should not panic:\n{stderr}"
    );
}

#[test]
fn init_and_repo_init_dry_run_are_equivalent() {
    let repo = TempRepo::new("init-dry-run");
    let init = run_agl_in(repo.path(), &["init", "--dry-run"]);
    let repo_init = run_agl_in(repo.path(), &["repo", "init", "--dry-run"]);

    assert_success(&init);
    assert_success(&repo_init);
    assert_empty_stderr(&init);
    assert_empty_stderr(&repo_init);
    assert_eq!(stdout(&init), stdout(&repo_init));
}

#[test]
fn init_then_status_reports_missing_skills_submodule_warning() {
    let repo = TempRepo::new("status-after-init");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);

    let output = run_agl_in(repo.path(), &["status"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=warning");
    assert_contains(&stdout, "component.skills.warning=missing");
    assert_contains(&stdout, "next_step=initialize .agl/skills submodule");
}

#[test]
fn status_strict_fails_on_missing_skills_submodule_warning() {
    let repo = TempRepo::new("status-strict");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);

    let output = run_agl_in(repo.path(), &["status", "--strict"]);

    assert_failure(&output);
    assert_contains(&stdout(&output), "state=warning");
    assert_contains(&stderr(&output), "repo workspace status is not healthy");
}

#[test]
fn skill_list_reports_workspace_candidates_without_trusting_plain_dir() {
    let repo = TempRepo::new("skill-list-plain-dir");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);
    write_workspace_skill(repo.path(), "repo-change");

    let output = run_agl_in(repo.path(), &["skill", "list"]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "skill name=repo-change");
    assert_contains(&stdout, "valid=true");
    assert_contains(&stdout, "usable=false");
    assert_contains(&stdout, "component_not_usable");
}

#[test]
fn skill_verify_fails_until_skills_component_is_locked_and_healthy() {
    let repo = TempRepo::new("skill-verify-missing");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);

    let output = run_agl_in(repo.path(), &["skill", "verify"]);

    assert_failure(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=warning");
    assert_contains(&stdout, "warning=component.skills.missing");
    assert_contains(&stdout, "warning=skills_lock_missing");
    assert_contains(&stderr(&output), "workspace skill verification failed");
}

#[test]
fn skill_lock_refuses_plain_workspace_skills_directory() {
    let repo = TempRepo::new("skill-lock-plain-dir");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);
    write_workspace_skill(repo.path(), "repo-change");

    let output = run_agl_in(repo.path(), &["skill", "lock"]);

    assert_failure(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=invalid");
    assert_contains(&stdout, "error=skills_component_not_usable");
    assert_contains(&stderr(&output), "workspace skill lock failed");
}

#[test]
fn skill_inspect_runtime_succeeds_for_builtin_skill() {
    let output = run_agl(&["skill", "inspect", "change", "--runtime"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    assert_contains(&stdout(&output), "skill name=change");
    assert_contains(&stdout(&output), "usable=true");
}

#[test]
fn skill_inspect_runtime_rejects_untrusted_workspace_skill() {
    let repo = TempRepo::new("skill-inspect-runtime");
    let home = TempHome::new("skill-inspect-runtime");
    let home_arg = home.path_string();
    let init = run_agl_in(repo.path(), &["--home", &home_arg, "init"]);
    assert_success(&init);
    write_workspace_skill(repo.path(), "repo-change");

    let output = run_agl_in(
        repo.path(),
        &[
            "--home",
            &home_arg,
            "skill",
            "inspect",
            "repo-change",
            "--runtime",
        ],
    );

    assert_failure(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "skill name=repo-change");
    assert_contains(&stdout, "usable=false");
    assert_contains(&stderr(&output), "skill is not runtime usable: repo-change");
    assert!(
        !stderr(&output).contains("local inference config"),
        "skill preflight should not enter inference config resolution"
    );
}

#[test]
fn daemon_status_without_daemon_reports_not_running_without_model_config() {
    let home = TempHome::new("status-no-daemon");
    let home_arg = home.path_string();
    let output = run_agl(&["--home", &home_arg, "daemon", "status"]);

    assert_success(&output);
    assert_empty_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=not_running");
    assert_contains(
        &stdout,
        &format!("socket_path={home_arg}/state/daemon/agl.sock"),
    );
    assert_contains(&stdout, "next_step=agl serve");
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
    assert_contains(&stdout, "serve");
    assert_contains(&stdout, "init");
    assert_contains(&stdout, "status");
    assert_contains(&stdout, "skill");
    assert_contains(&stdout, "cron");
    assert_contains(&stdout, "install-hooks");
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
fn invalid_workspace_root_fails_before_inference_config() {
    let home = TempHome::new("bad-workspace-root");
    let home_arg = home.path_string();
    let missing_workspace = home.path().join("missing-workspace");
    let missing_workspace_arg = missing_workspace.display().to_string();
    let output = run_agl(&[
        "--home",
        &home_arg,
        "run",
        "--workspace-root",
        &missing_workspace_arg,
        "hello",
    ]);

    assert_failure(&output);
    assert!(stdout(&output).is_empty(), "stdout should be empty");
    let stderr = stderr(&output);
    assert_contains(&stderr, "failed to canonicalize workspace root");
    assert!(
        !stderr.contains("local inference config"),
        "invalid workspace root should fail before inference config resolution:\n{stderr}"
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

fn run_agl_in(cwd: &std::path::Path, args: &[&str]) -> Output {
    Command::new(AGL_BIN)
        .current_dir(cwd)
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

fn write_workspace_skill(repo: &std::path::Path, name: &str) {
    let skill_dir = repo.join(".agl/skills/agl").join(name);
    fs::create_dir_all(&skill_dir)
        .unwrap_or_else(|err| panic!("failed to create skill dir {}: {err}", skill_dir.display()));
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            r#"---
name: {name}
description: Review repository changes.
version: 1
source: workspace
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
context_budget_tokens: 256
references:
  include: []
guarantees:
  - repository paths are checked
---
Body.
"#
        ),
    )
    .unwrap_or_else(|err| panic!("failed to write workspace skill {name}: {err}"));
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

fn memory_id_from_output(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("id="))
        .unwrap_or_else(|| panic!("memory id missing from output:\n{stdout}"))
        .to_string()
}

fn note_id_from_output(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("id="))
        .unwrap_or_else(|| panic!("note id missing from output:\n{stdout}"))
        .to_string()
}

fn cron_id_from_output(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("id="))
        .unwrap_or_else(|| panic!("cron id missing from output:\n{stdout}"))
        .to_string()
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

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new(label: &str) -> Self {
        let id = TEMP_HOME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agl-cli-surface-repo-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join(".git"))
            .unwrap_or_else(|err| panic!("failed to create temp repo {}: {err}", path.display()));
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
