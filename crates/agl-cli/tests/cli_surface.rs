use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const AGL_BIN: &str = env!("CARGO_BIN_EXE_agl");
const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000001";

static TEMP_HOME_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn agl_help_lists_public_commands() {
    let output = run_agl(&["--help"]);

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_no_noncanonical_product_spelling(&stdout);
    assert_contains(&stdout, "Usage: agl");
    assert_contains(&stdout, "Basics:");
    assert_contains(
        &stdout,
        ".agl/workspace.toml lists the repo's agentLIBRE folders",
    );
    assert_contains(&stdout, "Workspace skills need .agl/skills.lock");
    assert_contains(&stdout, "run");
    assert_contains(&stdout, "init");
    assert_contains(&stdout, "chat");
    assert_contains(&stdout, "serve");
    assert_contains(&stdout, "status");
    assert_contains(&stdout, "function");
    assert_contains(&stdout, "inference");
    assert_contains(&stdout, "skill");
    assert_contains(&stdout, "cron");
    assert_contains(&stdout, "store");
    assert_contains(&stdout, "memory");
    assert_contains(&stdout, "notes");
    assert_contains(&stdout, "install-hooks");
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

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "▒▒████████▒▒███████ ███████████");
    assert!(
        !stdout.contains("\x1b[35m"),
        "captured help output should not include ANSI color:\n{stdout}"
    );
    assert_contains(&stdout, "Usage: agl");
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
        &["completion", "--help"][..],
        &["config", "--help"][..],
        &["config", "paths", "--help"][..],
        &["config", "init", "--help"][..],
        &["init", "--help"][..],
        &["install-hooks", "--help"][..],
        &["run", "--help"][..],
        &["serve", "--help"][..],
        &["status", "--help"][..],
        &["function", "--help"][..],
        &["function", "list", "--help"][..],
        &["function", "show", "--help"][..],
        &["function", "status", "--help"][..],
        &["function", "init", "--help"][..],
        &["function", "doctor", "--help"][..],
        &["inference", "--help"][..],
        &["inference", "run", "--help"][..],
        &["inference", "chat", "--help"][..],
        &["inference", "serve", "--help"][..],
        &["skill", "--help"][..],
        &["skill", "init", "--help"][..],
        &["skill", "list", "--help"][..],
        &["skill", "inspect", "--help"][..],
        &["skill", "status", "--help"][..],
        &["skill", "verify", "--help"][..],
        &["skill", "sync-folders", "--help"][..],
        &["skill", "lock", "--help"][..],
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
        &["store", "--help"][..],
        &["store", "status", "--help"][..],
        &["store", "migrate", "--help"][..],
        &["store", "export", "--help"][..],
        &["memory", "--help"][..],
        &["memory", "add", "--help"][..],
        &["memory", "list", "--help"][..],
        &["memory", "search", "--help"][..],
        &["memory", "show", "--help"][..],
        &["memory", "delete", "--help"][..],
        &["memory", "suggest", "--help"][..],
        &["memory", "list-suggestions", "--help"][..],
        &["memory", "approve", "--help"][..],
        &["memory", "reject", "--help"][..],
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

        assert_success_no_stderr(&output);
        let stdout = stdout(&output);
        assert_contains(&stdout, "Usage: agl");
        assert_no_noncanonical_product_spelling(&stdout);
    }
}

#[test]
fn command_help_exits_successfully_for_advanced_commands() {
    for args in [
        &["repo", "--help"][..],
        &["repo", "init", "--help"][..],
        &["repo", "init-component", "--help"][..],
        &["repo", "import-profile", "--help"][..],
        &["repo", "status", "--help"][..],
        &["repo", "verify-tasks", "--help"][..],
        &["repo", "artifact", "--help"][..],
        &["repo", "artifact", "status", "--help"][..],
        &["repo", "artifact", "verify", "--help"][..],
        &["repo", "artifact", "sync", "--help"][..],
        &["repo", "artifact", "lock", "--help"][..],
        &["repo", "install-hooks", "--help"][..],
        &["repo", "export-profile", "--help"][..],
        &["daemon", "--help"][..],
        &["daemon", "status", "--help"][..],
        &["cron", "tick", "--help"][..],
    ] {
        let output = run_agl(args);

        assert_success_no_stderr(&output);
        let stdout = stdout(&output);
        assert_contains(&stdout, "Usage: agl");
        assert_no_noncanonical_product_spelling(&stdout);
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
    let id = id_from_output(&add_stdout, "memory");

    let list = run_agl(&["--home", &home_arg, "memory", "list"]);
    assert_success_stdout_contains(&list, &id);

    let search = run_agl(&["--home", &home_arg, "memory", "search", "imperative"]);
    assert_success_stdout_contains(&search, &id);

    let show = run_agl(&["--home", &home_arg, "memory", "show", &id]);
    assert_success(&show);
    assert_contains(&stdout(&show), "memory.");
    assert_contains(&stdout(&show), "Use imperative subjects.");

    let delete = run_agl(&["--home", &home_arg, "memory", "delete", &id]);
    assert_success_stdout_contains(&delete, "memory.deleted=true");

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

    let keyed = run_agl(&[
        "--home",
        &home_arg,
        "memory",
        "add",
        "--scope",
        "user",
        "--scope-key",
        "profile-a",
        "--title",
        "Keyed user memory",
        "--body",
        "Only profile-a should see this.",
    ]);
    assert_success(&keyed);
    let keyed_id = id_from_output(&stdout(&keyed), "memory");
    let default_user = run_agl(&["--home", &home_arg, "memory", "list", "--scope", "user"]);
    assert_success(&default_user);
    assert!(
        !stdout(&default_user).contains(&keyed_id),
        "explicit user scope keys must not mix with user/default"
    );
    let profile_user = run_agl(&[
        "--home",
        &home_arg,
        "memory",
        "list",
        "--scope",
        "user",
        "--scope-key",
        "profile-a",
    ]);
    assert_success_stdout_contains(&profile_user, &keyed_id);
}

#[test]
fn memory_suggestion_commands_require_approval() {
    let home = TempHome::new("memory-suggestion-commands");
    let home_arg = home.path_string();

    let suggest = run_agl(&[
        "--home",
        &home_arg,
        "memory",
        "suggest",
        "--kind",
        "decision",
        "--title",
        "Memory policy",
        "--body",
        "Use pending suggestions before durable writes.",
        "--source-ref",
        "chat:turn-1",
    ]);

    assert_success(&suggest);
    let suggest_stdout = stdout(&suggest);
    assert_contains(&suggest_stdout, "memory_suggestion id=");
    assert_contains(&suggest_stdout, "status=pending");
    let suggestion_id = id_from_output(&suggest_stdout, "memory suggestion");

    let empty_memory = run_agl(&["--home", &home_arg, "memory", "search", "pending"]);
    assert_success(&empty_memory);
    assert!(
        !stdout(&empty_memory).contains("memory id="),
        "pending suggestion should not be durable memory yet"
    );

    let list = run_agl(&["--home", &home_arg, "memory", "list-suggestions"]);
    assert_success_stdout_contains(&list, &suggestion_id);

    let approve = run_agl(&["--home", &home_arg, "memory", "approve", &suggestion_id]);
    assert_success(&approve);
    assert_contains(&stdout(&approve), "memory_suggestion.approved=true");
    assert_contains(&stdout(&approve), "memory id=");

    let memory = run_agl(&["--home", &home_arg, "memory", "search", "pending"]);
    assert_success_stdout_contains(&memory, "kind=decision");

    let pending = run_agl(&["--home", &home_arg, "memory", "list-suggestions"]);
    assert_success(&pending);
    assert!(
        !stdout(&pending).contains(&suggestion_id),
        "approved suggestion should leave the pending list"
    );
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
    let id = id_from_output(&add_stdout, "note");

    let search = run_agl(&["--home", &home_arg, "notes", "search", "pinned"]);
    assert_success_stdout_contains(&search, &id);

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
    assert_success_stdout_contains(&show, "Use pinned trusted workspace skills.");

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

    let post_remember_update = run_agl(&[
        "--home",
        &home_arg,
        "notes",
        "update",
        &id,
        "--body",
        "Changed after promotion.",
    ]);
    assert_success(&post_remember_update);
    let snapshot_search = run_agl(&["--home", &home_arg, "memory", "search", "Changed"]);
    assert_success(&snapshot_search);
    assert_eq!(
        stdout(&snapshot_search).trim(),
        "",
        "notes remember must snapshot memory instead of live-syncing later note updates"
    );

    let delete = run_agl(&["--home", &home_arg, "notes", "delete", &id]);
    assert_success_stdout_contains(&delete, "note.deleted=true");

    let audit_show = run_agl(&["--home", &home_arg, "notes", "show", &id]);
    assert_success(&audit_show);
    assert_contains(&stdout(&audit_show), "audit=tombstoned");
    assert_contains(&stdout(&audit_show), "Changed after promotion.");

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
    let id = id_from_output(&add_stdout, "cron");

    let list = run_agl(&["--home", &home_arg, "cron", "list"]);
    assert_success_stdout_contains(&list, &id);

    let show = run_agl(&["--home", &home_arg, "cron", "show", &id]);
    assert_success_stdout_contains(&show, "notify_ref=matrix-room:!status");

    let disable = run_agl(&["--home", &home_arg, "cron", "disable", &id]);
    assert_success_stdout_contains(&disable, "enabled=false");

    let enable = run_agl(&["--home", &home_arg, "cron", "enable", &id]);
    assert_success_stdout_contains(&enable, "enabled=true");

    let run = run_agl(&["--home", &home_arg, "cron", "run", &id, "--now"]);
    assert_success(&run);
    let run_stdout = stdout(&run);
    assert_contains(&run_stdout, "cron_run id=");
    assert_contains(&run_stdout, "status=succeeded");
    assert_contains(&run_stdout, "result_ref=builtin:store-status:schema:");
    assert_contains(&run_stdout, "idempotency.final_status=completed");

    let run_json = run_agl(&["--home", &home_arg, "cron", "run", &id, "--now", "--json"]);
    assert_success(&run_json);
    let run_json_stdout = stdout(&run_json);
    assert_contains(&run_json_stdout, "\"admission\":");
    assert_contains(&run_json_stdout, "\"final_status\": \"completed\"");
    assert!(
        !run_json_stdout.contains("IdempotencyRecord"),
        "cron JSON must not expose Rust debug formatting:\n{run_json_stdout}"
    );

    let preflight = run_agl(&[
        "--home",
        &home_arg,
        "cron",
        "run",
        &id,
        "--preflight",
        "--json",
    ]);
    assert_success(&preflight);
    assert_contains(&stdout(&preflight), "\"records_run\": false");

    let history = run_agl(&["--home", &home_arg, "cron", "history", &id]);
    assert_success_stdout_contains(&history, "status=succeeded");

    let tick = run_agl(&[
        "--home", &home_arg, "cron", "tick", "--at", "32400", "--json",
    ]);
    assert_success(&tick);
    let tick_stdout = stdout(&tick);
    assert_contains(&tick_stdout, "\"due_jobs\": 1");
    assert_contains(&tick_stdout, "\"notifications\": 1");

    let cron_out_path = home.path().join("cron-export.jsonl");
    let cron_out_arg = cron_out_path.display().to_string();
    let cron_export = run_agl(&[
        "--home",
        &home_arg,
        "store",
        "export",
        "--domain",
        "cron",
        "--out",
        &cron_out_arg,
    ]);
    assert_success(&cron_export);
    assert_contains(
        &stdout(&cron_export),
        "store.export.record_type.matrix_notification_outbox=1",
    );
    let cron_exported = fs::read_to_string(&cron_out_path).unwrap_or_else(|err| {
        panic!(
            "failed to read cron export {}: {err}",
            cron_out_path.display()
        )
    });
    assert_contains(
        &cron_exported,
        "\"record_type\":\"matrix_notification_outbox\"",
    );

    let delete = run_agl(&["--home", &home_arg, "cron", "delete", &id]);
    assert_success_stdout_contains(&delete, "cron.deleted=true");

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

    assert_failure_stderr_contains(&output, "invalid cron schedule_expr value");
}

#[test]
fn store_commands_report_status_and_export_jsonl() {
    let home = TempHome::new("store-commands");
    let home_arg = home.path_string();
    let matrix_store = home.path().join("data/matrix-bridge/store");
    fs::create_dir_all(&matrix_store).unwrap_or_else(|err| {
        panic!(
            "failed to create fake Matrix store {}: {err}",
            matrix_store.display()
        )
    });
    fs::write(
        matrix_store.join("session.json"),
        r#"{"access_token":"SECRET_MATRIX_TOKEN","store_path":"/tmp/matrix-crypto"}"#,
    )
    .unwrap_or_else(|err| panic!("failed to write fake Matrix session: {err}"));

    let add = run_agl(&[
        "--home",
        &home_arg,
        "memory",
        "add",
        "--title",
        "Export me",
        "--body",
        "Store export smoke.",
    ]);
    assert_success(&add);

    let status = run_agl(&["--home", &home_arg, "store", "status"]);
    assert_success(&status);
    let status_stdout = stdout(&status);
    assert_contains(&status_stdout, "store.path=");
    assert_contains(&status_stdout, "store.schema_version=");
    assert_contains(&status_stdout, "store.domain.memory=ok");
    assert_contains(&status_stdout, "active_rows=1");
    assert_contains(&status_stdout, "store.domain.notes=ok");
    assert_contains(&status_stdout, "store.domain.cron=ok");
    assert_contains(&status_stdout, "store.idempotency.in_progress=0");
    assert_contains(&status_stdout, "store.idempotency.stale_in_progress=0");

    let out_path = home.path().join("memory-export.jsonl");
    let out_arg = out_path.display().to_string();
    let export = run_agl(&[
        "--home", &home_arg, "store", "export", "--domain", "memory", "--out", &out_arg,
    ]);
    assert_success(&export);
    assert_contains(&stdout(&export), "store.exported=true");
    assert_contains(&stdout(&export), "store.export.records=1");
    assert_contains(&stdout(&export), "store.export.record_type.memory_entry=1");
    let exported = fs::read_to_string(&out_path)
        .unwrap_or_else(|err| panic!("failed to read export {}: {err}", out_path.display()));
    assert_contains(&exported, "\"domain\":\"memory\"");
    assert_contains(&exported, "\"title\":\"Export me\"");
    assert!(
        !exported.contains("SECRET_MATRIX_TOKEN"),
        "store export must not read Matrix crypto/session files:\n{exported}"
    );
    assert!(
        !exported.contains("/tmp/matrix-crypto"),
        "store export must not include Matrix crypto paths:\n{exported}"
    );

    let overwrite = run_agl(&[
        "--home", &home_arg, "store", "export", "--domain", "memory", "--out", &out_arg,
    ]);
    assert_failure_stderr_contains(&overwrite, "pass --force to overwrite");

    let forced = run_agl(&[
        "--home", &home_arg, "store", "export", "--domain", "memory", "--out", &out_arg, "--force",
    ]);
    assert_success(&forced);

    let matrix_out = home.path().join("matrix-export.jsonl");
    let matrix_out_arg = matrix_out.display().to_string();
    let matrix_export = run_agl(&[
        "--home",
        &home_arg,
        "store",
        "export",
        "--domain",
        "matrix",
        "--out",
        &matrix_out_arg,
    ]);
    assert_failure_stderr_contains(&matrix_export, "invalid value 'matrix'");
    assert!(
        !matrix_out.exists(),
        "unknown domain must not create export file"
    );
}

#[test]
fn store_status_does_not_create_database_before_explicit_migrate() {
    let home = TempHome::new("store-explicit-migrate");
    let home_arg = home.path_string();
    let database_path = home.path().join("data/store/agentlibre.sqlite3");

    let status = run_agl(&["--home", &home_arg, "store", "status"]);
    assert_success(&status);
    let status_stdout = stdout(&status);
    assert_contains(&status_stdout, "store.schema_version=none");
    assert_contains(&status_stdout, "store.database_exists=false");
    assert_contains(&status_stdout, "store.migration_required=true");
    assert_contains(&status_stdout, "next_step=agl store migrate");
    assert!(
        !database_path.exists(),
        "store status should not create {}",
        database_path.display()
    );

    let out_path = home.path().join("memory-export.jsonl");
    let out_arg = out_path.display().to_string();
    let export = run_agl(&[
        "--home", &home_arg, "store", "export", "--domain", "memory", "--out", &out_arg,
    ]);
    assert_failure_stderr_contains(&export, "run store.migrate first");

    let migrate = run_agl(&["--home", &home_arg, "store", "migrate"]);
    assert_success(&migrate);
    assert_contains(&stdout(&migrate), "store.migrated=true");
    assert!(database_path.exists());
}

#[test]
fn run_help_describes_trusted_workspace_skills() {
    let output = run_agl(&["run", "--help"]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "Core or trusted workspace skill id");
}

#[test]
fn hidden_repo_help_remains_available_for_advanced_usage() {
    let output = run_agl(&["repo", "--help"]);

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "Usage: agl repo");
    assert_contains(&stdout, "Repo workspace:");
    assert_contains(&stdout, "each opt-in root is declared once");
    assert_contains(
        &stdout,
        "undeclared roots are not created, inspected, or reported as missing",
    );
    assert_contains(&stdout, "init");
    assert_contains(&stdout, "status");
    assert_contains(&stdout, "install-hooks");
    assert!(
        !stdout.contains("import-profile"),
        "script-only import-profile command should stay hidden:\n{stdout}"
    );
}

#[test]
fn daemon_help_explains_process_boundaries() {
    let output = run_agl(&["daemon", "--help"]);

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(
        &stdout,
        "The daemon serves long-running clients such as the Matrix bridge.",
    );
    assert_contains(
        &stdout,
        "Commands such as `agl run` and `agl chat` execute in the invoking process.",
    );
    assert_contains(
        &stdout,
        "`agl serve` starts the daemon, and `agl daemon status` inspects it.",
    );
}

#[test]
fn skill_help_explains_workspace_skill_use() {
    let output = run_agl(&["skill", "--help"]);

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "Skill use:");
    assert_contains(&stdout, "SKILL.md lists the skill name");
    assert_contains(
        &stdout,
        ".agl/skills.lock records the current workspace skill git commit",
    );
    assert_contains(&stdout, "state/skill-trust.toml approves that exact commit");
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
fn batch_logging_init_failure_is_quiet_without_panicking() {
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
    assert!(
        !stderr.contains("panicked at"),
        "logging fallback should not panic:\n{stderr}"
    );
    assert!(
        !stderr.contains("warning: failed to initialize logging"),
        "batch commands should not print noisy logging warnings:\n{stderr}"
    );
}

#[test]
fn init_dry_run_includes_local_bootstrap_steps() {
    let repo = TempRepo::new("init-dry-run-bootstrap");
    let init = run_agl_in(repo.path(), &["init", "--dry-run"]);
    let repo_init = run_agl_in(repo.path(), &["repo", "init", "--dry-run"]);

    assert_success_no_stderr(&init);
    assert_success_no_stderr(&repo_init);
    let init_stdout = stdout(&init);
    let repo_stdout = stdout(&repo_init);
    assert_contains(
        &init_stdout,
        "change path=.agl/workspace.toml action=would_write_file",
    );
    assert_contains(&init_stdout, "bootstrap.functions_root.path=.agl/functions");
    assert_contains(
        &init_stdout,
        "bootstrap.functions_root.action=would_create_dir",
    );
    assert_contains(&init_stdout, "bootstrap.default_function=gemma4-12b");
    assert_contains(&init_stdout, "bootstrap.builtin_function=gemma4-12b");
    assert_contains(&init_stdout, "next_step=agl run --prompt");
    assert!(
        !repo_stdout.contains("bootstrap.functions_root"),
        "repo init should remain manifest-only:\n{repo_stdout}"
    );
    assert!(
        !repo.path().join(".agl/functions").exists(),
        "dry-run bootstrap must not create functions root"
    );
}

#[test]
fn init_creates_functions_root_and_keeps_repo_init_manifest_behavior() {
    let repo = TempRepo::new("init-bootstrap");
    let init = run_agl_in(repo.path(), &["init"]);

    assert_success_no_stderr(&init);
    let init_stdout = stdout(&init);
    assert_contains(&init_stdout, "state=initialized");
    assert_contains(&init_stdout, "bootstrap.functions_root.action=created_dir");
    assert_contains(&init_stdout, "bootstrap.default_function=gemma4-12b");
    assert_contains(&init_stdout, "bootstrap.builtin_function=gemma4-12b");
    assert!(repo.path().join(".agl/workspace.toml").is_file());
    let manifest = fs::read_to_string(repo.path().join(".agl/workspace.toml")).unwrap();
    assert_contains(&manifest, "[functions]");
    assert_contains(&manifest, "default = \"gemma4-12b\"");
    assert!(repo.path().join(".agl/functions").is_dir());

    let second = run_agl_in(repo.path(), &["init"]);
    assert_success_no_stderr(&second);
    assert_contains(&stdout(&second), "bootstrap.functions_root.action=exists");
}

#[test]
fn init_accepts_local_workspace_profile_file() {
    let repo = TempRepo::new("init-profile-file");
    let profile_path = repo.path().join("profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "portable-repo-workflow"

[artifacts.skills]
kind = "git"
path = ".agl/skills"
url = "ssh://git@example.invalid/agentlibre/agl-skills.git"
rev = "v0.2.0"
required = true
access = "read"

[artifacts.tasks]
kind = "git"
path = ".agl/tasks"
url = "ssh://git@example.invalid/agentlibre/tasks.git"
rev = "main"
required = true
access = "read_write"
validation = "agl.task_spec.v1"

[artifacts.state]
kind = "ignored"
path = ".agl/state"
required = false
access = "read_write"
create = ["."]
"#,
    )
    .unwrap_or_else(|err| panic!("failed to write profile {}: {err}", profile_path.display()));
    let profile_arg = profile_path.display().to_string();

    let output = run_agl_in(
        repo.path(),
        &["init", "--profile-file", &profile_arg, "--dry-run"],
    );

    assert_success(&output);
    let stdout = stdout(&output);
    assert_contains(
        &stdout,
        "change path=.agl/tasks action=declared_git_component",
    );
    assert_contains(
        &stdout,
        "change path=.agl/skills action=declared_git_component",
    );
}

#[test]
fn repo_export_profile_writes_portable_policy_manifest() {
    let repo = TempRepo::new("export-profile");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);
    fs::write(
        repo.path().join(".agl/skill-trust.toml"),
        "SECRET_LOCAL_TRUST_SHOULD_NOT_EXPORT",
    )
    .unwrap();
    let out = repo.path().join("repo-workflow.toml");
    let out_arg = out.display().to_string();

    let export = run_agl_in(repo.path(), &["repo", "export-profile", "--out", &out_arg]);

    assert_success(&export);
    let stdout = stdout(&export);
    assert_contains(&stdout, "profile.exported=true");
    assert_contains(&stdout, "profile.policy.trust.import_local_trust=false");

    let content = fs::read_to_string(&out)
        .unwrap_or_else(|err| panic!("failed to read profile export {}: {err}", out.display()));
    assert_contains(&content, "[policy.hooks]");
    assert!(!content.contains("[artifacts."));
    assert!(
        !content.contains("SECRET_LOCAL_TRUST_SHOULD_NOT_EXPORT"),
        "profile export must not include local trust:\n{content}"
    );

    let overwrite = run_agl_in(repo.path(), &["repo", "export-profile", "--out", &out_arg]);
    assert_failure_stderr_contains(&overwrite, "failed to create profile export");
}

#[test]
fn repo_import_profile_hidden_command_applies_explicit_profile() {
    let repo = TempRepo::new("import-profile");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);
    let out = repo.path().join("repo-workflow.toml");
    let out_arg = out.display().to_string();
    let export = run_agl_in(repo.path(), &["repo", "export-profile", "--out", &out_arg]);
    assert_success(&export);

    let import = run_agl_in(
        repo.path(),
        &[
            "repo",
            "import-profile",
            "--profile-file",
            &out_arg,
            "--dry-run",
        ],
    );

    assert_success(&import);
    let stdout = stdout(&import);
    assert_contains(&stdout, "state=initialized");
    assert_contains(&stdout, "dry_run=true");
    assert_contains(&stdout, "change path=.agl/workspace.toml action=exists");
}

#[test]
fn init_then_status_is_healthy_without_workspace_artifacts() {
    let repo = TempRepo::new("status-after-init");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);

    let output = run_agl_in(repo.path(), &["status"]);

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=ok");
    assert!(!stdout.contains("component.skills"));
}

#[test]
fn status_strict_accepts_workspace_without_optional_artifacts() {
    let repo = TempRepo::new("status-strict");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);

    let output = run_agl_in(repo.path(), &["status", "--strict"]);

    assert_success_no_stderr(&output);
    assert_contains(&stdout(&output), "state=ok");
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
fn skill_list_supports_source_trusted_only_and_limit_filters() {
    let repo = TempRepo::new("skill-list-filters");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);
    write_workspace_skill(repo.path(), "repo-change");

    let core_only = run_agl_in(
        repo.path(),
        &[
            "skill",
            "list",
            "--source",
            "core",
            "--trusted-only",
            "--limit",
            "1",
        ],
    );

    assert_success(&core_only);
    let core_only_stdout = stdout(&core_only);
    assert_contains(&core_only_stdout, "source=core");
    assert!(
        !core_only_stdout.contains("skill name=repo-change"),
        "core-only list should not include local workspace skills:\n{core_only_stdout}"
    );
    assert!(
        !core_only_stdout.contains("component_not_usable"),
        "core-only list should not print workspace warnings:\n{core_only_stdout}"
    );

    let core = run_agl_in(
        repo.path(),
        &["skill", "list", "--source", "core", "--limit", "1"],
    );
    assert_success(&core);
    let core_stdout = stdout(&core);
    assert_contains(&core_stdout, "source=core");

    let local = run_agl_in(repo.path(), &["skill", "list", "--source", "local"]);
    assert_success(&local);
    let local_stdout = stdout(&local);
    assert_contains(&local_stdout, "skill name=repo-change");
    assert!(
        !local_stdout.contains("source=core"),
        "local-only list should not include embedded core skills:\n{local_stdout}"
    );
}

#[test]
fn skill_verify_is_neutral_when_workspace_skills_are_not_configured() {
    let repo = TempRepo::new("skill-verify-missing");
    let init = run_agl_in(repo.path(), &["init"]);
    assert_success(&init);

    let output = run_agl_in(repo.path(), &["skill", "verify"]);

    assert_success_no_stderr(&output);
    assert_contains(&stdout(&output), "state=ok");
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
fn skill_verify_reports_trusted_workspace_skill_as_usable() {
    let (repo, _source, home) = submodule_workspace_with_skill(
        "skill-verify-trust",
        "repo-change",
        r#"---
name: repo-change
description: Review repository changes.
version: 1
source: local
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
"#,
    );
    let home_arg = home.path_string();
    let lock = run_agl_in(repo.path(), &["--home", &home_arg, "skill", "lock"]);
    assert_success(&lock);
    let trust = run_agl_in(
        repo.path(),
        &[
            "--home",
            &home_arg,
            "skill",
            "trust",
            "repo-change",
            "--yes",
        ],
    );
    assert_success(&trust);

    let verify = run_agl_in(repo.path(), &["--home", &home_arg, "skill", "verify"]);

    assert_success(&verify);
    let stdout = stdout(&verify);
    assert_contains(&stdout, "skill name=repo-change");
    assert_contains(&stdout, "usable=true");
    assert_contains(&stdout, "trust_state=TrustedLocal");
}

#[test]
fn skill_status_groups_invalid_duplicate_folder_create_diagnostic() {
    let (repo, _source, home) = submodule_workspace_with_skill(
        "skill-status-duplicate-create",
        "bad-dupe",
        r#"---
name: bad-dupe
description: Bad duplicate folder create rule.
version: 1
source: local
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
requestable_tools: []
context_budget_tokens: 256
references:
  include: []
artifacts:
  - id: bad
    kind: generated
    path: .agl/tasks/bad
    access: read_write
    create:
      - when: runtime_prepare
      - when: runtime_prepare
guarantees:
  - duplicate create rule must fail
---
Bad body.
"#,
    );
    let home_arg = home.path_string();

    let output = run_agl_in(repo.path(), &["--home", &home_arg, "skill", "status"]);

    assert_failure(&output);
    let stdout = stdout(&output);
    assert_contains(
        &stdout,
        "diagnostic severity=error scope=skill_manifest code=duplicate_value",
    );
    assert_contains(&stdout, "skill_path=.agl/skills/agl/bad-dupe");
    assert!(
        !stdout.contains("not_component_git_worktree"),
        "submodule-backed invalid manifest should not rely on component noise:\n{stdout}"
    );
}

#[test]
fn skill_status_json_groups_invalid_artifact_path_diagnostic() {
    let (repo, _source, home) = submodule_workspace_with_skill(
        "skill-status-invalid-path",
        "bad-path",
        r#"---
name: bad-path
description: Bad folder path.
version: 1
source: local
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
requestable_tools: []
context_budget_tokens: 256
references:
  include: []
artifacts:
  - id: bad
    kind: generated
    path: ../outside
    access: read_write
    create:
      - when: artifact_write
guarantees:
  - invalid path must fail
---
Bad body.
"#,
    );
    let home_arg = home.path_string();

    let output = run_agl_in(
        repo.path(),
        &["--home", &home_arg, "skill", "status", "--json"],
    );

    assert_failure(&output);
    let stdout = stdout(&output);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("invalid JSON: {err}\n{stdout}"));
    let diagnostics = json["diagnostics"]
        .as_array()
        .unwrap_or_else(|| panic!("diagnostics missing:\n{stdout}"));
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["severity"] == "error"
            && diagnostic["scope"] == "skill_manifest"
            && diagnostic["code"] == "invalid_artifact_path"
            && diagnostic["skill"] == ".agl/skills/agl/bad-path"
    }));
}

#[test]
fn skill_inspect_runtime_succeeds_for_builtin_skill() {
    let output = run_agl(&["skill", "inspect", "skill", "--runtime"]);

    assert_success_no_stderr(&output);
    assert_contains(&stdout(&output), "skill name=skill");
    assert_contains(&stdout(&output), "usable=true");
}

#[test]
fn skill_inspect_runtime_rejects_non_core_private_skill_without_source() {
    let output = run_agl(&["skill", "inspect", "repo-review", "--runtime"]);

    assert_failure(&output);
    assert_contains(&stderr(&output), "skill not found: repo-review");
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

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "state=not_running");
    assert_contains(
        &stdout,
        &format!("socket_path={home_arg}/state/daemon/agl.sock"),
    );
    assert_contains(&stdout, "next_step=agl serve");
}

#[test]
fn completion_bash_emits_agl_completion_function() {
    let output = run_agl(&["completion", "bash"]);

    assert_success_no_stderr(&output);
    let stdout = stdout(&output);
    assert_contains(&stdout, "_agl()");
    assert_contains(&stdout, "complete -F _agl");
    assert_contains(&stdout, "serve");
    assert_contains(&stdout, "init");
    assert_contains(&stdout, "status");
    assert_contains(&stdout, "skill");
    assert_contains(&stdout, "cron");
    assert_contains(&stdout, "store");
    assert_contains(&stdout, "install-hooks");
}

#[test]
fn home_override_roots_config_paths_in_requested_home() {
    let home = TempHome::new("config-paths");
    let home_arg = home.path_string();
    let output = run_agl(&["--home", &home_arg, "config", "paths"]);

    assert_success_no_stderr(&output);
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
fn function_commands_manage_workspace_function_artifact() {
    let repo = TempRepo::new("function-workspace");
    let home = TempHome::new("function-workspace");
    let home_arg = home.path_string();
    let local_profile = home
        .path()
        .join("config")
        .join("inference")
        .join("local.toml");
    fs::create_dir_all(local_profile.parent().unwrap()).unwrap();
    fs::write(&local_profile, "").unwrap();

    let init = run_agl_in(
        repo.path(),
        &[
            "--home",
            &home_arg,
            "function",
            "init",
            "coding",
            "--workspace",
        ],
    );
    assert_success_no_stderr(&init);
    let init_stdout = stdout(&init);
    assert_contains(&init_stdout, "state=ok");
    assert_contains(&init_stdout, "function.id=coding");
    assert_contains(&init_stdout, ".agl/functions/coding/FUNCTION.md");
    assert_contains(&init_stdout, ".agl/functions/coding/SYSTEM.md");
    let function_manifest =
        fs::read_to_string(repo.path().join(".agl/functions/coding/FUNCTION.md"))
            .expect("read generated function manifest");
    assert!(
        !function_manifest.contains("\nprompt:"),
        "generated FUNCTION.md must use SYSTEM.md convention instead of prompt.system"
    );

    let status = run_agl_in(
        repo.path(),
        &["--home", &home_arg, "function", "status", "coding"],
    );
    assert_success_no_stderr(&status);
    let status_stdout = stdout(&status);
    assert_contains(&status_stdout, "state=ok");
    assert_contains(&status_stdout, ".agl/functions/coding/SYSTEM.md");
    assert_contains(
        &status_stdout,
        &format!("function.model.profile_path={}", local_profile.display()),
    );

    let list = run_agl_in(repo.path(), &["--home", &home_arg, "function", "list"]);
    assert_success_no_stderr(&list);
    assert_contains(&stdout(&list), "function id=coding source=workspace");

    let show = run_agl_in(
        repo.path(),
        &["--home", &home_arg, "function", "show", "coding"],
    );
    assert_success_no_stderr(&show);
    assert_contains(&stdout(&show), "function.runtime.tool_mode=read-only");
    assert_contains(&stdout(&show), "function.system_path=");
    assert_contains(&stdout(&show), "You are the `coding` agentFUNCTION.");

    let doctor = run_agl_in(
        repo.path(),
        &["--home", &home_arg, "function", "doctor", "coding"],
    );
    assert_success_no_stderr(&doctor);
    assert_contains(&stdout(&doctor), "doctor.smoke_prompt=");
    assert_contains(&stdout(&doctor), "next_step=agl run --function coding");
}

#[test]
fn builtin_function_commands_expose_packaged_gemma4_functions() {
    let home = TempHome::new("builtin-functions");
    let home_arg = home.path_string();

    let list = run_agl(&["--home", &home_arg, "function", "list"]);
    assert_success_no_stderr(&list);
    let list_stdout = stdout(&list);
    assert_contains(
        &list_stdout,
        "function id=gemma4-12b source=builtin path=assets/functions/gemma4-12b/FUNCTION.md valid=true",
    );
    assert_contains(&list_stdout, "function id=gemma4-26b source=builtin");
    assert_contains(&list_stdout, "function id=gemma4-31b source=builtin");

    let status = run_agl(&["--home", &home_arg, "function", "status", "gemma4-12b"]);
    assert_success_no_stderr(&status);
    let status_stdout = stdout(&status);
    assert_contains(&status_stdout, "state=");
    assert_contains(&status_stdout, "function.source=builtin");
    assert_contains(
        &status_stdout,
        "function.model.config_path=assets/functions/gemma4-12b/inference.toml",
    );
    assert_contains(&status_stdout, "function.model.config_embedded=true");
    assert_contains(&status_stdout, "function.model.id=gemma4-12b");
    assert_contains(
        &status_stdout,
        "function.model.multimodal_projector_id=gemma4-12b-mmproj",
    );
    assert_contains(&status_stdout, "models.toml");

    let show = run_agl(&["--home", &home_arg, "function", "show", "gemma4-12b"]);
    assert_success_no_stderr(&show);
    let show_stdout = stdout(&show);
    assert_contains(&show_stdout, "function.source=builtin");
    assert_contains(
        &show_stdout,
        "You are an agentLIBRE function running on local Gemma4 12B.",
    );
    assert_contains(&show_stdout, "--- inference.toml ---");
    assert_contains(&show_stdout, "tool_call_format = \"gemma_function_call\"");
}

#[test]
fn function_run_rejects_missing_model_binding_before_admission() {
    let home = TempHome::new("missing-function-model-binding");
    let home_arg = home.path_string();
    let config_dir = home.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("models.toml"), "version = 1\nmodels = {}\n").unwrap();

    let output = run_agl(&[
        "--home",
        &home_arg,
        "run",
        "--function",
        "gemma4-12b",
        "--prompt",
        "This prompt must not reach model admission.",
    ]);

    assert_failure(&output);
    assert_empty_stdout(&output);
    assert_contains(&stderr(&output), "model `gemma4-12b` is not configured");
    assert!(!home.path().join("data/runs").exists());
}

#[test]
fn chat_rejects_prompt_option_with_clap_error() {
    let output = run_agl(&["chat", "--prompt", "hello"]);

    assert_failure(&output);
    assert_empty_stdout(&output);
    let stderr = stderr(&output);
    assert_contains(&stderr, "unexpected argument '--prompt'");
    assert!(
        !stderr.starts_with("error: error:"),
        "clap errors should not be double-prefixed:\n{stderr}"
    );
}

#[test]
fn chat_new_session_conflict_fails_before_inference_path() {
    let output = run_agl(&["chat", "--new-session", "--session-id", SESSION_ID]);

    assert_failure(&output);
    assert_empty_stdout(&output);
    let stderr = stderr(&output);
    assert_contains(&stderr, "--new-session cannot be used with --session-id");
    assert!(
        !stderr.contains("local inference config"),
        "session flag conflict should not run inference path:\n{stderr}"
    );
}

#[test]
fn removed_command_names_fail_before_inference_path() {
    for args in [
        &["infer", "--help"][..],
        &["generate", "--help"][..],
        &["setup"][..],
        &["doctor"][..],
        &["model", "pull", "owner/repo/model.gguf"][..],
    ] {
        let output = run_agl(args);

        assert_failure(&output);
        assert_empty_stdout(&output);
        let stderr = stderr(&output);
        assert_contains(&stderr, "unknown command");
        assert_contains(&stderr, "Use `agl run --prompt TEXT`");
        assert!(
            !stderr.contains("local inference config"),
            "removed command name should not run inference path:\n{stderr}"
        );
    }
}

#[test]
fn blank_bare_prompt_fails_before_inference_path() {
    let output = run_agl(&["   "]);

    assert_failure(&output);
    assert_empty_stdout(&output);
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
    let output = run_agl(&["--home", &home_arg, "inference", "run", "hello"]);

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
    assert_empty_stdout(&output);
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
            "inference",
            "chat",
            "--config",
            &config_arg,
            "--session-id",
            SESSION_ID,
            "--max-output-tokens",
            "1",
        ],
        "hello\n",
    );

    assert_failure(&output);
    assert_contains(&stdout(&output), &format!("session_id={SESSION_ID}"));
    assert_contains(&stderr(&output), "model request failed");

    let transcript = fs::read_to_string(
        home.path()
            .join("data")
            .join("sessions")
            .join(SESSION_ID)
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

fn submodule_workspace_with_skill(
    label: &str,
    skill_name: &str,
    skill_md: &str,
) -> (TempRepo, TempRepo, TempHome) {
    let source = TempRepo::new(&format!("{label}-source"));
    init_git_repo(source.path());
    let skill_dir = source.path().join("agl").join(skill_name);
    fs::create_dir_all(&skill_dir).unwrap_or_else(|err| {
        panic!(
            "failed to create source skill dir {}: {err}",
            skill_dir.display()
        )
    });
    fs::write(skill_dir.join("SKILL.md"), skill_md)
        .unwrap_or_else(|err| panic!("failed to write source skill {skill_name}: {err}"));
    git_run(source.path(), &["add", "."]);
    git_run(
        source.path(),
        &[
            "-c",
            "user.name=agentLIBRE Test",
            "-c",
            "user.email=agentlibre-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "add workspace skill",
        ],
    );

    let repo = TempRepo::new(&format!("{label}-repo"));
    init_git_repo(repo.path());
    let home = TempHome::new(label);
    let home_arg = home.path_string();
    let source_arg = source.path().display().to_string();
    let init = run_agl_in(
        repo.path(),
        &["--home", &home_arg, "init", "--skills-url", &source_arg],
    );
    assert_success(&init);
    git_run(
        repo.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &source_arg,
            ".agl/skills",
        ],
    );

    (repo, source, home)
}

fn init_git_repo(root: &std::path::Path) {
    let _ = fs::remove_dir_all(root.join(".git"));
    git_run(root, &["init", "-q", "."]);
}

fn git_run(root: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git in {}: {err}", root.display()));
    assert!(
        output.status.success(),
        "git failed in {}\nstdout:\n{}\nstderr:\n{}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_workspace_skill(repo: &std::path::Path, name: &str) {
    let manifest_path = repo.join(".agl/workspace.toml");
    let mut manifest = fs::read_to_string(&manifest_path).unwrap();
    if !manifest.contains("[artifacts.skills]") {
        manifest.push_str(
            r#"

[artifacts.skills]
kind = "git"
path = ".agl/skills"
url = "ssh://git@example.invalid/agentlibre/skills.git"
required = true
access = "read"
"#,
        );
        fs::write(&manifest_path, manifest).unwrap();
    }
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
source: local
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

fn assert_empty_stdout(output: &Output) {
    let stdout = stdout(output);
    assert!(stdout.is_empty(), "stdout should be empty:\n{stdout}");
}

fn assert_success_no_stderr(output: &Output) {
    assert_success(output);
    assert_empty_stderr(output);
}

fn assert_success_stdout_contains(output: &Output, needle: &str) {
    assert_success(output);
    assert_contains(&stdout(output), needle);
}

fn assert_failure_stderr_contains(output: &Output, needle: &str) {
    assert_failure(output);
    assert_contains(&stderr(output), needle);
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "expected output to contain {needle:?}\noutput:\n{haystack}"
    );
}

fn assert_no_noncanonical_product_spelling(output: &str) {
    for variant in ["AgentLIBRE", "AGENTLIBRE", "Agent Libre"] {
        assert!(
            !output.contains(variant),
            "rendered CLI text contains noncanonical product spelling `{variant}`:\n{output}"
        );
    }
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

fn id_from_output(stdout: &str, label: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("id="))
        .unwrap_or_else(|| panic!("{label} id missing from output:\n{stdout}"))
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
