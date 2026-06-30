use super::*;

fn parse_command(args: impl IntoIterator<Item = &'static str>) -> CliCommand {
    parse_cli(args.into_iter().map(str::to_string))
        .unwrap()
        .command
}

#[test]
fn parse_run_command_with_options() {
    let command = parse_command([
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
    ]);

    assert_eq!(
        command,
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
        })
    );
}

#[test]
fn parse_run_rejects_invalid_skill_id() {
    let error = parse_cli([
        "agl".to_string(),
        "run".to_string(),
        "--skill".to_string(),
        "Bad Skill".to_string(),
        "--prompt".to_string(),
        "hello".to_string(),
    ])
    .unwrap_err();

    assert!(error.to_string().contains("--skill is invalid"));
}

#[test]
fn parse_retired_infer_command_rejects_with_run_guidance() {
    let error = parse_cli([
        "agl".to_string(),
        "infer".to_string(),
        "--config".to_string(),
        "local.toml".to_string(),
        "--prompt".to_string(),
        "hello".to_string(),
    ])
    .unwrap_err();

    let message = error.to_string();
    assert!(message.contains("agl infer"));
    assert!(message.contains("Use `agl run --config PATH PROMPT`"));
}

#[test]
fn parse_run_prompt_argument() {
    let command = parse_command(["agl", "run", "hello", "world"]);

    assert_eq!(
        command,
        CliCommand::Infer(RunOptions {
            prompt: Some("hello world".to_string()),
            ..RunOptions::default()
        })
    );
}

#[test]
fn parse_generate_alias() {
    let command = parse_command(["agl", "generate", "--prompt", "hello"]);

    assert_eq!(
        command,
        CliCommand::Infer(RunOptions {
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        })
    );
}

#[test]
fn parse_run_command_with_memory_context() {
    let command = parse_command(["agl", "run", "--memory", "--prompt", "hello"]);

    assert_eq!(
        command,
        CliCommand::Infer(RunOptions {
            memory: true,
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        })
    );
}

#[test]
fn parse_serve_command_with_daemon_options() {
    let command = parse_command([
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
    ]);

    assert_eq!(
        command,
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
        })
    );
}

#[test]
fn parse_init_command() {
    let command = parse_command(["agl", "init", "--dry-run"]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
            profile: "repo-workflow".to_string(),
            profile_file: None,
            dry_run: true,
            force: false,
        }))
    );
}

#[test]
fn parse_repo_init_hidden_alias() {
    let command = parse_command([
        "agl",
        "repo",
        "init",
        "--force",
        "--profile-file",
        "profiles/custom.toml",
    ]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
            profile: "repo-workflow".to_string(),
            profile_file: Some(PathBuf::from("profiles/custom.toml")),
            dry_run: false,
            force: true,
        }))
    );
}

#[test]
fn parse_status_command_with_repo_options() {
    let command = parse_command([
        "agl",
        "status",
        "--json",
        "--component",
        "skills",
        "--strict",
    ]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::Status(RepoStatusOptions {
            json: true,
            component: Some("skills".to_string()),
            strict: true,
        }))
    );
}

#[test]
fn parse_repo_status_hidden_alias() {
    let command = parse_command(["agl", "repo", "status", "--json"]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::Status(RepoStatusOptions {
            json: true,
            component: None,
            strict: false,
        }))
    );
}

#[test]
fn parse_repo_export_profile_hidden_command() {
    let command = parse_command([
        "agl",
        "repo",
        "export-profile",
        "--out",
        "repo-workflow.toml",
        "--force",
        "--json",
    ]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::ExportProfile(RepoExportProfileOptions {
            out: PathBuf::from("repo-workflow.toml"),
            force: true,
            json: true,
        }))
    );
}

#[test]
fn parse_repo_import_profile_hidden_command() {
    let command = parse_command([
        "agl",
        "repo",
        "import-profile",
        "--profile-file",
        "repo-workflow.toml",
        "--dry-run",
        "--force",
    ]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::ImportProfile(RepoImportProfileOptions {
            profile_file: PathBuf::from("repo-workflow.toml"),
            dry_run: true,
            force: true,
        }))
    );
}

#[test]
fn parse_install_hooks_command() {
    let command = parse_command(["agl", "install-hooks", "--dry-run"]);

    assert_eq!(
        command,
        CliCommand::Repo(RepoCommand::InstallHooks(RepoHooksOptions {
            dry_run: true,
            force: false,
        }))
    );
}

#[test]
fn parse_skill_commands() {
    assert_eq!(
        parse_command([
            "agl",
            "skill",
            "list",
            "--json",
            "--source",
            "builtin",
            "--trusted-only",
            "--limit",
            "5",
        ]),
        CliCommand::Skill(SkillCommand::List(SkillListOptions {
            json: true,
            source: SkillListSourceArg::Builtin,
            trusted_only: true,
            limit: Some(5),
        }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "inspect", "repo-change", "--json"]),
        CliCommand::Skill(SkillCommand::Inspect(SkillInspectOptions {
            name: "repo-change".to_string(),
            json: true,
            runtime: false,
        }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "inspect", "repo-change", "--runtime"]),
        CliCommand::Skill(SkillCommand::Inspect(SkillInspectOptions {
            name: "repo-change".to_string(),
            json: false,
            runtime: true,
        }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "status", "--strict"]),
        CliCommand::Skill(SkillCommand::Status(SkillStatusOptions {
            json: false,
            strict: true,
        }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "verify", "--json"]),
        CliCommand::Skill(SkillCommand::Verify(SkillVerifyOptions { json: true }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "lock", "--dry-run"]),
        CliCommand::Skill(SkillCommand::Lock(SkillLockOptions {
            json: false,
            dry_run: true,
        }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "trust", "repo-change", "--yes"]),
        CliCommand::Skill(SkillCommand::Trust(SkillTrustOptions {
            name: "repo-change".to_string(),
            json: false,
            yes: true,
        }))
    );
    assert_eq!(
        parse_command(["agl", "skill", "revoke", "repo-change", "--json"]),
        CliCommand::Skill(SkillCommand::Revoke(SkillRevokeOptions {
            name: "repo-change".to_string(),
            json: true,
        }))
    );
}

#[test]
fn parse_memory_commands() {
    assert_eq!(
        parse_command([
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
        ]),
        CliCommand::Memory(MemoryCommand::Add(MemoryAddOptions {
            scope: MemoryScopeArg::Repo,
            scope_key: Some("/tmp/repo".to_string()),
            kind: MemoryKindArg::Decision,
            title: "Trust".to_string(),
            body: "Use local approval.".to_string(),
            source_ref: Some("manual".to_string()),
            confidence: 90,
            json: true,
        }))
    );
    assert_eq!(
        parse_command([
            "agl", "memory", "search", "--scope", "user", "--limit", "10", "approval",
        ]),
        CliCommand::Memory(MemoryCommand::Search(MemorySearchOptions {
            query: "approval".to_string(),
            scope: MemoryScopeArg::User,
            scope_key: None,
            include_deleted: false,
            limit: 10,
            json: false,
        }))
    );
    assert_eq!(
        parse_command(["agl", "memory", "delete", "mem_1"]),
        CliCommand::Memory(MemoryCommand::Delete(MemoryDeleteOptions {
            id: "mem_1".to_string(),
            json: false,
        }))
    );
}

#[test]
fn parse_notes_commands() {
    assert_eq!(
        parse_command([
            "agl",
            "notes",
            "add",
            "--title",
            "Workflow",
            "--body",
            "Use pinned skills.",
            "--json",
        ]),
        CliCommand::Notes(NotesCommand::Add(NotesAddOptions {
            title: "Workflow".to_string(),
            body: "Use pinned skills.".to_string(),
            json: true,
        }))
    );
    assert_eq!(
        parse_command([
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
        ]),
        CliCommand::Notes(NotesCommand::Remember(NotesRememberOptions {
            id: "note_1".to_string(),
            scope: MemoryScopeArg::Repo,
            scope_key: Some("/tmp/repo".to_string()),
            kind: MemoryKindArg::Decision,
            json: false,
        }))
    );
    assert_eq!(
        parse_command([
            "agl",
            "notes",
            "link",
            "note_1",
            "--to",
            "task:AGL-084",
            "--label",
            "spec",
        ]),
        CliCommand::Notes(NotesCommand::Link(NotesLinkOptions {
            id: "note_1".to_string(),
            target_ref: "task:AGL-084".to_string(),
            label: Some("spec".to_string()),
            json: false,
        }))
    );
}

#[test]
fn parse_cron_commands() {
    assert_eq!(
        parse_command([
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
        ]),
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
        }))
    );
    assert_eq!(
        parse_command([
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
        ]),
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
        }))
    );
    assert_eq!(
        parse_command(["agl", "cron", "run", "cron_1", "--now"]),
        CliCommand::Cron(CronCommand::Run(CronRunOptions {
            id: "cron_1".to_string(),
            now: true,
            preflight: false,
            mock_skill_execution: false,
            json: false,
        }))
    );
}

#[test]
fn parse_cron_rejects_missing_target_and_run_without_now() {
    let missing_target = parse_cli([
        "agl".to_string(),
        "cron".to_string(),
        "add".to_string(),
        "--name".to_string(),
        "Store status".to_string(),
        "--schedule".to_string(),
        "hourly".to_string(),
    ])
    .unwrap_err();
    assert!(
        missing_target
            .to_string()
            .contains("exactly one of --skill or --builtin is required")
    );

    let missing_prompt = parse_cli([
        "agl".to_string(),
        "cron".to_string(),
        "add".to_string(),
        "--name".to_string(),
        "Repo review".to_string(),
        "--schedule".to_string(),
        "hourly".to_string(),
        "--skill".to_string(),
        "repo-review".to_string(),
    ])
    .unwrap_err();
    assert!(
        missing_prompt
            .to_string()
            .contains("--prompt is required when --skill is used")
    );

    let missing_now = parse_cli([
        "agl".to_string(),
        "cron".to_string(),
        "run".to_string(),
        "cron_1".to_string(),
    ])
    .unwrap_err();
    assert!(
        missing_now
            .to_string()
            .contains("agl cron run requires --now or --preflight")
    );

    assert_eq!(
        parse_command(["agl", "cron", "run", "cron_1", "--preflight", "--json"]),
        CliCommand::Cron(CronCommand::Run(CronRunOptions {
            id: "cron_1".to_string(),
            now: false,
            preflight: true,
            mock_skill_execution: false,
            json: true,
        }))
    );
    assert_eq!(
        parse_command([
            "agl",
            "cron",
            "tick",
            "--at",
            "60",
            "--mock-skill-execution",
            "--json",
        ]),
        CliCommand::Cron(CronCommand::Tick(CronTickOptions {
            at: Some(60),
            mock_skill_execution: true,
            json: true,
        }))
    );
}

#[test]
fn parse_store_commands() {
    assert_eq!(
        parse_command(["agl", "store", "status", "--json"]),
        CliCommand::Store(StoreCommand::Status(StoreStatusOptions { json: true }))
    );
    assert_eq!(
        parse_command(["agl", "store", "migrate", "--json"]),
        CliCommand::Store(StoreCommand::Migrate(StoreMigrateOptions { json: true }))
    );
    assert_eq!(
        parse_command([
            "agl",
            "store",
            "export",
            "--domain",
            "memory",
            "--out",
            "memory.jsonl",
            "--include-deleted",
            "--force",
        ]),
        CliCommand::Store(StoreCommand::Export(StoreExportCliOptions {
            domain: StoreDomainArg::Memory,
            out: PathBuf::from("memory.jsonl"),
            include_deleted: true,
            force: true,
            json: false,
        }))
    );
}

#[test]
fn parse_memory_rejects_zero_limit() {
    let error = parse_cli([
        "agl".to_string(),
        "memory".to_string(),
        "list".to_string(),
        "--limit".to_string(),
        "0".to_string(),
    ])
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("--limit must be greater than zero")
    );
}

#[test]
fn parse_daemon_status_command_with_socket_override() {
    let command = parse_command(["agl", "daemon", "status", "--socket", "/tmp/agl.sock"]);

    assert_eq!(
        command,
        CliCommand::DaemonStatus(DaemonStatusOptions {
            socket_path: Some(PathBuf::from("/tmp/agl.sock")),
        })
    );
}

#[test]
fn parse_bare_prompt_as_run() {
    let command = parse_command(["agl", "hello"]);

    assert_eq!(
        command,
        CliCommand::Infer(RunOptions {
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        })
    );
}

#[test]
fn parse_rejects_blank_bare_prompt() {
    let error = parse_cli(["agl".to_string(), "   ".to_string()]).unwrap_err();

    assert!(error.to_string().contains("prompt cannot be empty"));
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
    let command = parse_command([
        "agl",
        "chat",
        "--session-id",
        "session-001",
        "--no-history",
        "--workspace-root",
        "/tmp/workspace",
    ]);

    assert_eq!(
        command,
        CliCommand::Chat(RunOptions {
            session_id: Some("session-001".to_string()),
            no_history: true,
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            ..RunOptions::default()
        })
    );
}

#[test]
fn parse_chat_rejects_new_session_with_session_id() {
    let error = parse_cli([
        "agl".to_string(),
        "chat".to_string(),
        "--new-session".to_string(),
        "--session-id".to_string(),
        "session-001".to_string(),
    ])
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("--new-session cannot be used with --session-id")
    );
}

#[test]
fn parse_chat_rejects_prompt() {
    let error = parse_cli([
        "agl".to_string(),
        "chat".to_string(),
        "--prompt".to_string(),
        "hello".to_string(),
    ])
    .unwrap_err();

    assert!(error.to_string().contains("unexpected argument"));
}

#[test]
fn parse_config_paths_command() {
    let command = parse_command(["agl", "config", "paths"]);

    assert_eq!(command, CliCommand::Config(ConfigCommand::Paths));
}

#[test]
fn parse_config_init_command() {
    let command = parse_command(["agl", "config", "init"]);

    assert_eq!(
        command,
        CliCommand::Config(ConfigCommand::Init { force: false })
    );
}

#[test]
fn parse_config_init_force_command() {
    let command = parse_command(["agl", "config", "init", "--force"]);

    assert_eq!(
        command,
        CliCommand::Config(ConfigCommand::Init { force: true })
    );
}

#[test]
fn parse_config_paths_rejects_force() {
    let error = parse_cli([
        "agl".to_string(),
        "config".to_string(),
        "paths".to_string(),
        "--force".to_string(),
    ])
    .unwrap_err();

    assert!(error.to_string().contains("unexpected argument"));
}

#[test]
fn parse_completion_command() {
    let command = parse_command(["agl", "completion", "bash"]);

    assert_eq!(command, CliCommand::Completion { shell: Shell::Bash });
}

#[test]
fn parse_reserved_setup_rejects_before_bare_prompt() {
    let error = parse_cli(["agl".to_string(), "setup".to_string()]).unwrap_err();

    assert!(error.to_string().contains("planned but not implemented"));
}

#[test]
fn parse_reserved_doctor_rejects_before_bare_prompt() {
    let error = parse_cli(["agl".to_string(), "doctor".to_string()]).unwrap_err();

    assert!(error.to_string().contains("planned but not implemented"));
}

#[test]
fn parse_reserved_model_rejects_subcommand_before_bare_prompt() {
    let error = parse_cli([
        "agl".to_string(),
        "model".to_string(),
        "pull".to_string(),
        "owner/repo/model.gguf".to_string(),
        "--set-default".to_string(),
    ])
    .unwrap_err();

    assert!(error.to_string().contains("agl model pull"));
    assert!(error.to_string().contains("planned but not implemented"));
}

#[test]
fn display_name_prefers_agl_alias() {
    assert_eq!(cli_display_name(Some("agl")), "agl");
    assert_eq!(cli_display_name(Some("/usr/local/bin/agl")), "agl");
    assert_eq!(cli_display_name(Some("agentLIBRE")), "agl");
    assert_eq!(cli_display_name(Some("/usr/local/bin/agentLIBRE")), "agl");
    assert_eq!(cli_display_name(None), "agl");
}
