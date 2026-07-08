use super::*;

fn parse_command(args: impl IntoIterator<Item = &'static str>) -> CliCommand {
    parse_cli(args.into_iter().map(str::to_string))
        .unwrap()
        .command
}

fn assert_command(args: impl IntoIterator<Item = &'static str>, expected: CliCommand) {
    assert_eq!(parse_command(args), expected);
}

fn parse_error(args: impl IntoIterator<Item = &'static str>) -> String {
    parse_cli(args.into_iter().map(str::to_string))
        .unwrap_err()
        .to_string()
}

fn assert_parse_error_contains(args: impl IntoIterator<Item = &'static str>, needle: &str) {
    assert!(
        parse_error(args).contains(needle),
        "expected parse error to contain {needle:?}"
    );
}

fn visible_subcommand_names(command: clap::Command) -> Vec<String> {
    command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| command.get_name().to_string())
        .collect()
}

#[test]
fn completion_surface_matches_visible_cli_commands() {
    assert_eq!(
        visible_subcommand_names(PublicCompletionCli::command()),
        visible_subcommand_names(Cli::command())
    );
}

#[test]
fn parse_run_command_with_options() {
    assert_command(
        [
            "agl",
            "run",
            "--config",
            "local.toml",
            "--artifact-root",
            "artifacts",
            "--prompt",
            "hello",
            "--run-id",
            "manual-test",
            "--workspace-root",
            "/tmp/workspace",
            "--max-output-tokens",
            "32",
            "--skill",
            "task-spec",
            "--tool-mode",
            "write",
        ],
        CliCommand::Infer(RunOptions {
            config: Some(PathBuf::from("local.toml")),
            artifact_root: Some(PathBuf::from("artifacts")),
            run_id: Some("manual-test".to_string()),
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            session_id: None,
            no_history: false,
            new_session: false,
            max_output_tokens: 32,
            tool_mode: ToolAccessMode::Write,
            skills: vec!["task-spec".to_string()],
            memory: false,
            prompt: Some("hello".to_string()),
        }),
    );
}

#[test]
fn parse_run_rejects_invalid_skill_id() {
    assert_parse_error_contains(
        ["agl", "run", "--skill", "Bad Skill", "--prompt", "hello"],
        "--skill is invalid",
    );
}

#[test]
fn parse_retired_infer_command_rejects_with_run_guidance() {
    let message = parse_error([
        "agl",
        "infer",
        "--config",
        "local.toml",
        "--prompt",
        "hello",
    ]);
    assert!(message.contains("agl infer"));
    assert!(message.contains("Use `agl run --config PATH PROMPT`"));
}

#[test]
fn parse_run_prompt_argument() {
    assert_command(
        ["agl", "run", "hello", "world"],
        CliCommand::Infer(RunOptions {
            prompt: Some("hello world".to_string()),
            ..RunOptions::default()
        }),
    );
}

#[test]
fn parse_generate_alias() {
    assert_command(
        ["agl", "generate", "--prompt", "hello"],
        CliCommand::Infer(RunOptions {
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        }),
    );
}

#[test]
fn parse_run_command_with_memory_context() {
    assert_command(
        ["agl", "run", "--memory", "--prompt", "hello"],
        CliCommand::Infer(RunOptions {
            memory: true,
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        }),
    );
}

#[test]
fn parse_serve_command_with_daemon_options() {
    assert_command(
        [
            "agl",
            "serve",
            "--socket",
            "/tmp/agl.sock",
            "--config",
            "local.toml",
            "--artifact-root",
            "artifacts",
            "--workspace-root",
            "/tmp/workspace",
            "--max-output-tokens",
            "33",
            "--tool-mode",
            "write",
            "--skill",
            "tool-smoke",
        ],
        CliCommand::Serve(ServeOptions {
            socket_path: Some(PathBuf::from("/tmp/agl.sock")),
            config: Some(PathBuf::from("local.toml")),
            artifact_root: Some(PathBuf::from("artifacts")),
            run_id: None,
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            max_output_tokens: 33,
            tool_mode: ToolAccessMode::Write,
            skills: vec!["tool-smoke".to_string()],
            memory: false,
        }),
    );
}

#[test]
fn parse_init_command() {
    assert_command(
        ["agl", "init", "--dry-run"],
        CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
            profile: "repo-workflow".to_string(),
            profile_file: None,
            artifact_sources: Vec::new(),
            skills_url: None,
            skills_rev: None,
            tasks_url: None,
            tasks_rev: None,
            dry_run: true,
            force: false,
        })),
    );
}

#[test]
fn parse_repo_init_hidden_alias() {
    assert_command(
        [
            "agl",
            "repo",
            "init",
            "--force",
            "--profile-file",
            "profiles/custom.toml",
        ],
        CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
            profile: "repo-workflow".to_string(),
            profile_file: Some(PathBuf::from("profiles/custom.toml")),
            artifact_sources: Vec::new(),
            skills_url: None,
            skills_rev: None,
            tasks_url: None,
            tasks_rev: None,
            dry_run: false,
            force: true,
        })),
    );
}

#[test]
fn parse_init_command_with_external_artifacts() {
    assert_command(
        [
            "agl",
            "init",
            "--skills-url",
            "git@example.com:agentlibre/skills.git",
            "--skills-rev",
            "v1",
            "--tasks-url",
            "git@example.com:private/specs.git",
            "--tasks-rev",
            "main",
        ],
        CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
            profile: "repo-workflow".to_string(),
            profile_file: None,
            artifact_sources: Vec::new(),
            skills_url: Some("git@example.com:agentlibre/skills.git".to_string()),
            skills_rev: Some("v1".to_string()),
            tasks_url: Some("git@example.com:private/specs.git".to_string()),
            tasks_rev: Some("main".to_string()),
            dry_run: false,
            force: false,
        })),
    );
}

#[test]
fn parse_init_command_with_generic_artifact_sources() {
    assert_command(
        [
            "agl",
            "init",
            "--artifact-source",
            "tasks=rpi:/home/dinozyavier/git/agl-specs.git@main",
            "--artifact-source",
            "reviews=git@example.com:agentlibre/reviews.git",
        ],
        CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
            profile: "repo-workflow".to_string(),
            profile_file: None,
            artifact_sources: vec![
                RepoArtifactSourceOverride {
                    name: "tasks".to_string(),
                    url: "rpi:/home/dinozyavier/git/agl-specs.git".to_string(),
                    rev: Some("main".to_string()),
                },
                RepoArtifactSourceOverride {
                    name: "reviews".to_string(),
                    url: "git@example.com:agentlibre/reviews.git".to_string(),
                    rev: None,
                },
            ],
            skills_url: None,
            skills_rev: None,
            tasks_url: None,
            tasks_rev: None,
            dry_run: false,
            force: false,
        })),
    );
}

#[test]
fn parse_status_command_with_repo_options() {
    assert_command(
        [
            "agl",
            "status",
            "--json",
            "--component",
            "skills",
            "--strict",
        ],
        CliCommand::Repo(RepoCommand::Status(RepoStatusOptions {
            json: true,
            component: Some("skills".to_string()),
            strict: true,
        })),
    );
}

#[test]
fn parse_repo_status_hidden_alias() {
    assert_command(
        ["agl", "repo", "status", "--json"],
        CliCommand::Repo(RepoCommand::Status(RepoStatusOptions {
            json: true,
            component: None,
            strict: false,
        })),
    );
}

#[test]
fn parse_repo_verify_tasks_hidden_command() {
    assert_command(
        ["agl", "repo", "verify-tasks", "--json", "--strict"],
        CliCommand::Repo(RepoCommand::VerifyTasks(TaskSpecVerifyOptions {
            json: true,
            strict: true,
        })),
    );
}

#[test]
fn parse_repo_artifact_commands() {
    assert_command(
        [
            "agl",
            "repo",
            "artifact",
            "status",
            "--json",
            "--artifact",
            "tasks",
            "--strict",
        ],
        CliCommand::Repo(RepoCommand::Artifact(ArtifactCommand::Status(
            ArtifactStatusOptions {
                json: true,
                artifact: Some("tasks".to_string()),
                strict: true,
            },
        ))),
    );

    assert_command(
        ["agl", "repo", "artifact", "sync", "--dry-run", "--json"],
        CliCommand::Repo(RepoCommand::Artifact(ArtifactCommand::Sync(
            ArtifactSyncOptions {
                json: true,
                dry_run: true,
                strict: false,
            },
        ))),
    );

    assert_command(
        ["agl", "repo", "artifact", "lock", "--dry-run"],
        CliCommand::Repo(RepoCommand::Artifact(ArtifactCommand::Lock(
            ArtifactLockOptions {
                json: false,
                dry_run: true,
                strict: false,
            },
        ))),
    );
}

#[test]
fn parse_repo_init_component_hidden_command() {
    assert_command(
        [
            "agl",
            "repo",
            "init-component",
            "tasks",
            "--dry-run",
            "--json",
        ],
        CliCommand::Repo(RepoCommand::InitComponent(RepoComponentInitOptions {
            component: "tasks".to_string(),
            dry_run: true,
            json: true,
        })),
    );
}

#[test]
fn parse_repo_export_profile_hidden_command() {
    assert_command(
        [
            "agl",
            "repo",
            "export-profile",
            "--out",
            "repo-workflow.toml",
            "--force",
            "--json",
        ],
        CliCommand::Repo(RepoCommand::ExportProfile(RepoExportProfileOptions {
            out: PathBuf::from("repo-workflow.toml"),
            force: true,
            json: true,
        })),
    );
}

#[test]
fn parse_repo_import_profile_hidden_command() {
    assert_command(
        [
            "agl",
            "repo",
            "import-profile",
            "--profile-file",
            "repo-workflow.toml",
            "--dry-run",
            "--force",
        ],
        CliCommand::Repo(RepoCommand::ImportProfile(RepoImportProfileOptions {
            profile_file: PathBuf::from("repo-workflow.toml"),
            dry_run: true,
            force: true,
        })),
    );
}

#[test]
fn parse_install_hooks_command() {
    assert_command(
        ["agl", "install-hooks", "--dry-run"],
        CliCommand::Repo(RepoCommand::InstallHooks(RepoHooksOptions {
            dry_run: true,
            force: false,
        })),
    );
}

#[test]
fn parse_skill_commands() {
    assert_command(
        ["agl", "skill", "init", "--dry-run", "--json"],
        CliCommand::Skill(SkillCommand::Init(SkillInitOptions {
            dry_run: true,
            json: true,
        })),
    );
    assert_command(
        [
            "agl",
            "skill",
            "list",
            "--json",
            "--source",
            "builtin",
            "--trusted-only",
            "--limit",
            "5",
        ],
        CliCommand::Skill(SkillCommand::List(SkillListOptions {
            json: true,
            source: SkillListSourceArg::Builtin,
            trusted_only: true,
            limit: Some(5),
        })),
    );
    assert_command(
        ["agl", "skill", "list", "--source", "core"],
        CliCommand::Skill(SkillCommand::List(SkillListOptions {
            json: false,
            source: SkillListSourceArg::Core,
            trusted_only: false,
            limit: None,
        })),
    );
    assert_command(
        ["agl", "skill", "inspect", "repo-change", "--json"],
        CliCommand::Skill(SkillCommand::Inspect(SkillInspectOptions {
            name: "repo-change".to_string(),
            json: true,
            runtime: false,
        })),
    );
    assert_command(
        ["agl", "skill", "inspect", "repo-change", "--runtime"],
        CliCommand::Skill(SkillCommand::Inspect(SkillInspectOptions {
            name: "repo-change".to_string(),
            json: false,
            runtime: true,
        })),
    );
    assert_command(
        ["agl", "skill", "status", "--strict"],
        CliCommand::Skill(SkillCommand::Status(SkillStatusOptions {
            json: false,
            strict: true,
        })),
    );
    assert_command(
        ["agl", "skill", "verify", "--json"],
        CliCommand::Skill(SkillCommand::Verify(SkillVerifyOptions { json: true })),
    );
    assert_command(
        ["agl", "skill", "sync-folders", "--dry-run", "--json"],
        CliCommand::Skill(SkillCommand::SyncFolders(SkillFolderSyncOptions {
            json: true,
            dry_run: true,
            when: SkillFolderSyncSituationArg::SkillSync,
        })),
    );
    assert_command(
        ["agl", "skill", "lock", "--dry-run"],
        CliCommand::Skill(SkillCommand::Lock(SkillLockOptions {
            json: false,
            dry_run: true,
        })),
    );
    assert_command(
        ["agl", "skill", "trust", "repo-change", "--yes"],
        CliCommand::Skill(SkillCommand::Trust(SkillTrustOptions {
            name: "repo-change".to_string(),
            json: false,
            yes: true,
        })),
    );
    assert_command(
        ["agl", "skill", "revoke", "repo-change", "--json"],
        CliCommand::Skill(SkillCommand::Revoke(SkillRevokeOptions {
            name: "repo-change".to_string(),
            json: true,
        })),
    );
}

#[test]
fn parse_memory_commands() {
    assert_command(
        [
            "agl",
            "memory",
            "add",
            "--scope",
            "repo",
            "--scope-key",
            "/tmp/repo",
            "--kind",
            "decision",
            "--title",
            "Trust",
            "--body",
            "Use local approval.",
            "--source-ref",
            "manual",
            "--confidence",
            "90",
            "--json",
        ],
        CliCommand::Memory(MemoryCommand::Add(MemoryAddOptions {
            scope: MemoryScopeArg::Repo,
            scope_key: Some("/tmp/repo".to_string()),
            kind: MemoryKindArg::Decision,
            title: "Trust".to_string(),
            body: "Use local approval.".to_string(),
            source_ref: Some("manual".to_string()),
            confidence: 90,
            json: true,
        })),
    );
    assert_command(
        [
            "agl", "memory", "search", "--scope", "user", "--limit", "10", "approval",
        ],
        CliCommand::Memory(MemoryCommand::Search(MemorySearchOptions {
            query: "approval".to_string(),
            scope: MemoryScopeArg::User,
            scope_key: None,
            include_deleted: false,
            limit: 10,
            json: false,
        })),
    );
    assert_command(
        ["agl", "memory", "delete", "mem_1"],
        CliCommand::Memory(MemoryCommand::Delete(MemoryDeleteOptions {
            id: "mem_1".to_string(),
            json: false,
        })),
    );
}

#[test]
fn parse_notes_commands() {
    assert_command(
        [
            "agl",
            "notes",
            "add",
            "--title",
            "Workflow",
            "--body",
            "Use pinned skills.",
            "--json",
        ],
        CliCommand::Notes(NotesCommand::Add(NotesAddOptions {
            title: "Workflow".to_string(),
            body: "Use pinned skills.".to_string(),
            json: true,
        })),
    );
    assert_command(
        [
            "agl",
            "notes",
            "remember",
            "note_1",
            "--scope",
            "repo",
            "--scope-key",
            "/tmp/repo",
            "--kind",
            "decision",
        ],
        CliCommand::Notes(NotesCommand::Remember(NotesRememberOptions {
            id: "note_1".to_string(),
            scope: MemoryScopeArg::Repo,
            scope_key: Some("/tmp/repo".to_string()),
            kind: MemoryKindArg::Decision,
            json: false,
        })),
    );
    assert_command(
        [
            "agl",
            "notes",
            "link",
            "note_1",
            "--to",
            "task:AGL-084",
            "--label",
            "spec",
        ],
        CliCommand::Notes(NotesCommand::Link(NotesLinkOptions {
            id: "note_1".to_string(),
            target_ref: "task:AGL-084".to_string(),
            label: Some("spec".to_string()),
            json: false,
        })),
    );
}

#[test]
fn parse_cron_commands() {
    assert_command(
        [
            "agl",
            "cron",
            "add",
            "--name",
            "Store status",
            "--schedule",
            "0 9 * * *",
            "--builtin",
            "store-status",
            "--notify",
            "matrix-room:!room",
            "--json",
        ],
        CliCommand::Cron(CronCommand::Add(CronAddOptions {
            name: "Store status".to_string(),
            schedule: "0 9 * * *".to_string(),
            target: CronTargetArg {
                kind: CronTargetKindArg::Builtin,
                target_ref: "store-status".to_string(),
            },
            enabled: true,
            timezone: None,
            notify_ref: Some("matrix-room:!room".to_string()),
            prompt: None,
            input: None,
            json: true,
        })),
    );
    assert_command(
        [
            "agl",
            "cron",
            "add",
            "--name",
            "Repo review",
            "--schedule",
            "daily 09:00",
            "--skill",
            "repo-review",
            "--prompt",
            "Review repository changes.",
            "--input",
            "{\"limit\":10}",
            "--disabled",
            "--timezone",
            "UTC-07:00",
        ],
        CliCommand::Cron(CronCommand::Add(CronAddOptions {
            name: "Repo review".to_string(),
            schedule: "daily 09:00".to_string(),
            target: CronTargetArg {
                kind: CronTargetKindArg::Skill,
                target_ref: "repo-review".to_string(),
            },
            enabled: false,
            timezone: Some("UTC-07:00".to_string()),
            notify_ref: None,
            prompt: Some("Review repository changes.".to_string()),
            input: Some("{\"limit\":10}".to_string()),
            json: false,
        })),
    );
    assert_command(
        ["agl", "cron", "run", "cron_1", "--now"],
        CliCommand::Cron(CronCommand::Run(CronRunOptions {
            id: "cron_1".to_string(),
            now: true,
            preflight: false,
            mock_skill_execution: false,
            json: false,
        })),
    );
}

#[test]
fn parse_cron_rejects_missing_target_and_run_without_now() {
    assert_parse_error_contains(
        [
            "agl",
            "cron",
            "add",
            "--name",
            "Store status",
            "--schedule",
            "hourly",
        ],
        "exactly one of --skill or --builtin is required",
    );

    assert_parse_error_contains(
        [
            "agl",
            "cron",
            "add",
            "--name",
            "Repo review",
            "--schedule",
            "hourly",
            "--skill",
            "repo-review",
        ],
        "--prompt is required when --skill is used",
    );

    assert_parse_error_contains(
        ["agl", "cron", "run", "cron_1"],
        "agl cron run requires --now or --preflight",
    );

    assert_command(
        ["agl", "cron", "run", "cron_1", "--preflight", "--json"],
        CliCommand::Cron(CronCommand::Run(CronRunOptions {
            id: "cron_1".to_string(),
            now: false,
            preflight: true,
            mock_skill_execution: false,
            json: true,
        })),
    );
    assert_command(
        [
            "agl",
            "cron",
            "tick",
            "--at",
            "60",
            "--mock-skill-execution",
            "--json",
        ],
        CliCommand::Cron(CronCommand::Tick(CronTickOptions {
            at: Some(60),
            mock_skill_execution: true,
            json: true,
        })),
    );
}

#[test]
fn parse_store_commands() {
    assert_command(
        ["agl", "store", "status", "--json"],
        CliCommand::Store(StoreCommand::Status(StoreStatusOptions { json: true })),
    );
    assert_command(
        ["agl", "store", "migrate", "--json"],
        CliCommand::Store(StoreCommand::Migrate(StoreMigrateOptions { json: true })),
    );
    assert_command(
        [
            "agl",
            "store",
            "export",
            "--domain",
            "memory",
            "--out",
            "memory.jsonl",
            "--include-deleted",
            "--force",
        ],
        CliCommand::Store(StoreCommand::Export(StoreExportCliOptions {
            domain: StoreDomainArg::Memory,
            out: PathBuf::from("memory.jsonl"),
            include_deleted: true,
            force: true,
            json: false,
        })),
    );
}

#[test]
fn parse_memory_rejects_zero_limit() {
    assert_parse_error_contains(
        ["agl", "memory", "list", "--limit", "0"],
        "--limit must be greater than zero",
    );
}

#[test]
fn parse_daemon_status_command_with_socket_override() {
    assert_command(
        ["agl", "daemon", "status", "--socket", "/tmp/agl.sock"],
        CliCommand::DaemonStatus(DaemonStatusOptions {
            socket_path: Some(PathBuf::from("/tmp/agl.sock")),
        }),
    );
}

#[test]
fn parse_bare_prompt_as_run() {
    assert_command(
        ["agl", "hello"],
        CliCommand::Infer(RunOptions {
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        }),
    );
}

#[test]
fn parse_rejects_blank_bare_prompt() {
    assert_parse_error_contains(["agl", "   "], "prompt cannot be empty");
}

#[test]
fn parse_home_override() {
    let invocation = parse_cli([
        "agl".to_string(),
        "--home".to_string(),
        "/tmp/agl-home".to_string(),
        "config".to_string(),
        "paths".to_string(),
    ])
    .unwrap();

    assert_eq!(invocation.home, Some(PathBuf::from("/tmp/agl-home")));
    assert_eq!(invocation.command, CliCommand::Config(ConfigCommand::Paths));
}

#[test]
fn parse_chat_session_options() {
    assert_command(
        [
            "agl",
            "chat",
            "--session-id",
            "session-001",
            "--no-history",
            "--workspace-root",
            "/tmp/workspace",
        ],
        CliCommand::Chat(RunOptions {
            session_id: Some("session-001".to_string()),
            no_history: true,
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            ..RunOptions::default()
        }),
    );
}

#[test]
fn parse_chat_rejects_new_session_with_session_id() {
    assert_parse_error_contains(
        [
            "agl",
            "chat",
            "--new-session",
            "--session-id",
            "session-001",
        ],
        "--new-session cannot be used with --session-id",
    );
}

#[test]
fn parse_chat_rejects_prompt() {
    assert_parse_error_contains(["agl", "chat", "--prompt", "hello"], "unexpected argument");
}

#[test]
fn parse_config_paths_command() {
    assert_command(
        ["agl", "config", "paths"],
        CliCommand::Config(ConfigCommand::Paths),
    );
}

#[test]
fn parse_config_init_command() {
    assert_command(
        ["agl", "config", "init"],
        CliCommand::Config(ConfigCommand::Init { force: false }),
    );
}

#[test]
fn parse_config_init_force_command() {
    assert_command(
        ["agl", "config", "init", "--force"],
        CliCommand::Config(ConfigCommand::Init { force: true }),
    );
}

#[test]
fn parse_config_paths_rejects_force() {
    assert_parse_error_contains(["agl", "config", "paths", "--force"], "unexpected argument");
}

#[test]
fn parse_completion_command() {
    assert_command(
        ["agl", "completion", "bash"],
        CliCommand::Completion { shell: Shell::Bash },
    );
}

#[test]
fn parse_reserved_setup_rejects_before_bare_prompt() {
    assert_parse_error_contains(["agl", "setup"], "planned but not implemented");
}

#[test]
fn parse_reserved_doctor_rejects_before_bare_prompt() {
    assert_parse_error_contains(["agl", "doctor"], "planned but not implemented");
}

#[test]
fn parse_reserved_model_rejects_subcommand_before_bare_prompt() {
    let message = parse_error([
        "agl",
        "model",
        "pull",
        "owner/repo/model.gguf",
        "--set-default",
    ]);

    assert!(message.contains("agl model pull"));
    assert!(message.contains("planned but not implemented"));
}

#[test]
fn display_name_is_fixed_to_agl() {
    assert_eq!(cli_display_name(), "agl");
}
