use super::*;

#[test]
fn daemon_connection_errors_distinguish_missing_and_incompatible_servers() {
    let socket = Path::new("/tmp/agentlibre-test.sock");
    let missing =
        interactive_connect_error(socket, ClientError::DaemonUnavailable("refused".to_owned()));
    assert!(missing.to_string().contains("daemon is unavailable"));

    let incompatible = interactive_connect_error(
        socket,
        ClientError::SchemaMismatch {
            expected: "agentlibre.event.v8alpha",
        },
    );
    assert!(incompatible.to_string().contains("incompatible protocol"));
    assert!(format!("{incompatible:#}").contains("v8alpha"));
}

#[test]
fn interactive_unavailable_never_falls_back_to_a_process_local_worker() {
    let root = std::env::temp_dir().join(format!(
        "agl-terminal-no-fallback-{}",
        agl_ids::RequestId::generate()
    ));
    let runtime = UiRuntimeConfig {
        agent_state_dir: root.join("home/state"),
        ui_state_dir: root.join("home/terminal-state"),
        terminal_runtime_dir: root.join("home/terminal-runtime"),
        shell_program: PathBuf::from("bash"),
    };
    let options = InteractiveOptions {
        resume: None,
        input_history: false,
        socket_path: Some(root.join("missing-daemon.sock")),
        workspace_root: None,
        function_ref: None,
        model_id: None,
        operation_mode: None,
        skills: Vec::new(),
    };

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_interactive_async(options, &runtime))
        .expect_err("interactive mode must require its daemon");

    assert!(
        format!("{error:#}").contains("daemon is unavailable"),
        "unexpected connection failure: {error:#}"
    );
    assert!(
        !runtime
            .agent_state_dir
            .join("inference/worker-tmp")
            .exists()
    );
    let _ = std::fs::remove_dir_all(root);
}
