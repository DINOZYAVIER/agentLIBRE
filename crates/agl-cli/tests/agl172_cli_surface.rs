use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agl"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// AGL172-049 and AGL172-054.
#[test]
fn package_and_runtime_artifact_have_distinct_exact_cli_surfaces() {
    let top = run(&["--help"]);
    assert!(top.status.success());
    let top = stdout(&top);
    assert!(top.contains("package"));
    assert!(top.contains("artifact"));

    let package = run(&["package", "--help"]);
    assert!(package.status.success());
    let package = stdout(&package);
    for command in ["list", "inspect", "resolve", "graph", "lock", "source"] {
        assert!(
            package.contains(command),
            "missing package command {command}"
        );
    }

    let artifact = run(&["artifact", "--help"]);
    assert!(artifact.status.success());
    let artifact = stdout(&artifact);
    for command in ["status", "verify"] {
        assert!(
            artifact.contains(command),
            "missing Artifact command {command}"
        );
    }
    for forbidden in [
        "add", "remove", "sync", "lock", "list", "inspect", "resolve", "graph",
    ] {
        assert!(
            !artifact.contains(forbidden),
            "obsolete Artifact command {forbidden}"
        );
    }
}

// AGL172-010 and AGL172-052.
#[test]
fn repo_profile_status_and_component_commands_are_rejected_by_parser() {
    for args in [
        vec!["repo", "status"],
        vec!["repo", "import-profile"],
        vec!["repo", "export-profile"],
        vec!["repo", "init-component", "tasks"],
        vec!["repo", "component", "status", "tasks"],
    ] {
        let output = run(&args);
        assert!(!output.status.success(), "removed command parsed: {args:?}");
    }

    let init = run(&["repo", "init", "--help"]);
    assert!(init.status.success());
    let init = stdout(&init);
    for forbidden in [
        "--profile",
        "--profile-file",
        "--artifact",
        "--skills-url",
        "--skills-rev",
        "--tasks-url",
        "--tasks-rev",
    ] {
        assert!(
            !init.contains(forbidden),
            "removed repo init option remains: {forbidden}"
        );
    }
}

// AGL172-052, AGL172-058 and AGL172-063.
#[test]
fn skill_component_folder_commands_are_absent() {
    for args in [vec!["skill", "init"], vec!["skill", "sync-folders"]] {
        let output = run(&args);
        assert!(
            !output.status.success(),
            "removed skill command parsed: {args:?}"
        );
    }
    let help = run(&["skill", "--help"]);
    assert!(help.status.success());
    let help = stdout(&help);
    assert!(!help.contains("sync-folders"));
    assert!(!help.contains("workspace component"));
}

// AGL172-049 and AGL172-060.
#[test]
fn old_artifact_package_commands_and_lock_path_have_no_cli_fallback() {
    for args in [
        vec!["artifact", "list"],
        vec!["artifact", "inspect", "function:example/test@1"],
        vec!["artifact", "resolve", "function:example/test@1"],
        vec!["artifact", "graph", "function:example/test@1"],
        vec!["artifact", "lock"],
    ] {
        let output = run(&args);
        assert!(
            !output.status.success(),
            "old package alias parsed: {args:?}"
        );
        let old_lock = ["artifact", "-lock.toml"].concat();
        assert!(!stderr(&output).contains(&old_lock));
    }
}

// AGL172-062.
#[test]
fn verify_tasks_has_no_path_or_artifact_id_argument() {
    let help = run(&["repo", "verify-tasks", "--help"]);
    assert!(help.status.success());
    let help = stdout(&help);
    assert!(!help.contains("--path"));
    assert!(!help.contains("--artifact"));
    assert!(!help.contains("ARTIFACT_ID"));

    for args in [
        vec!["repo", "verify-tasks", "--path", ".agl/tasks"],
        vec!["repo", "verify-tasks", "core.repo:tasks"],
    ] {
        assert!(!run(&args).status.success());
    }
}
