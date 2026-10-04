#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedule_protocol_enable_show_busy_disable_and_restart() {
    // Isolate Forge's XDG cache before any worker starts; never mutate the
    // environment of this parallel test process or use the person's catalog.
    let Ok(location) = std::env::var("AGL_SCHEDULE_PROTOCOL_FIXTURE") else {
        let root =
            PathBuf::from("/tmp").join(format!("agl-sched-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::schedule_protocol_enable_show_busy_disable_and_restart",
                "--nocapture",
            ])
            .env("AGL_SCHEDULE_PROTOCOL_FIXTURE", &root)
            .env("XDG_DATA_HOME", root.join("global-data"))
            .env("XDG_CONFIG_HOME", root.join("global-config"))
            .env("XDG_STATE_HOME", root.join("global-state"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = PathBuf::from(location);
    let source = root.join("source-git");
    let catalog = root.join("catalog");
    let workspace = root.join("workspace");
    for path in [&source, &catalog, &workspace] {
        std::fs::create_dir_all(path).unwrap();
    }
    for path in ["agent", "model", "function"] {
        std::fs::create_dir(source.join(path)).unwrap();
    }
    std::fs::write(source.join("agent/AGENT.md"), "---\nschema: agentlibre.agent/v1\nid: fixture.agent\nversion: 1.0.0\nrequired_tools: []\n---\n").unwrap();
    std::fs::write(source.join("agent/SYSTEM.md"), "Return a short result.\n").unwrap();
    use sha2::Digest as _;
    let model_digest = PackageDigest::from_bytes(sha2::Sha256::digest(b"GGUF").into());
    std::fs::write(source.join("model/MODEL.toml"), format!(
        "schema = 'agentlibre.model/v1'\nid = 'fixture.model'\nversion = '1.0.0'\ndialect = 'generic'\ntool_call_format = 'hermes_json'\n[artifact]\nkind = 'gguf'\nurl = 'https://example.invalid/model.gguf'\nsha256 = '{model_digest}'\nbytes = 4\n")).unwrap();
    let mut function = String::from(
        "schema = 'agentlibre.function/v2'\nid = 'fixture.function'\nversion = '1.0.0'\nagent = { id = 'fixture.agent', version = '1.0.0' }\nmodel = { id = 'fixture.model', version = '1.0.0' }\n[presentation.tool_output]\nlines = 10\nchars = 500\n[presentation.tool]\nframe = false\n[presentation.model_generation]\ndetails = false\n[presentation.colors]\n",
    );
    let colors = serde_json::to_value(agl_core::agent::PresentationColors::default()).unwrap();
    for (name, value) in colors.as_object().unwrap() {
        function.push_str(&format!("{name} = {value}\n"));
    }
    function.push_str("[inference.generation]\nmax_output_tokens = 32\n[inference.load]\naccelerator = 'cpu'\ncontext_tokens = 128\nbatch_size = 8\nubatch_size = 8\nthreads = 1\ngpu_layers = 0\n[schedule]\ninterval_seconds = 1\nmax_run_seconds = 5\nprompt = 'scheduled task'\nevents = ['file:source.txt']\n");
    std::fs::write(source.join("function/FUNCTION.toml"), &function).unwrap();
    agl_runtime::function::FunctionManifest::parse(&function).unwrap();
    let git = |arguments: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "Fixture sources"]);
    let revision = git(&["rev-parse", "HEAD"]);
    let mut declarations = [
        ("fixture.agent", "agent", "agentlibre.agent/v1", "agent"),
        ("fixture.model", "model", "agentlibre.model/v1", "model"),
        (
            "fixture.function",
            "function",
            "agentlibre.function/v2",
            "function",
        ),
    ]
    .map(|(id, kind, schema, path)| agl_runtime::ForgeDeclaration {
        id: id.into(),
        kind: kind.into(),
        schema: schema.into(),
        git: source.to_string_lossy().into_owned(),
        revision: revision.clone(),
        path: path.into(),
    });
    std::fs::write(
        catalog.join("agentLIBRE.toml"),
        agl_runtime::serialize_workspace_forge_declarations(&catalog, declarations.to_vec())
            .unwrap(),
    )
    .unwrap();
    std::fs::write(workspace.join("agentLIBRE.toml"), b"format = 2\n").unwrap();
    let storage = ayeque_forge_core::data_root().unwrap();
    let sources = agl_runtime::workspace_backend_declarations(&declarations).unwrap();
    ayeque_forge_core::ForgeMutation::begin(&storage, &catalog).unwrap().sync(&sources).unwrap();
    ayeque_forge_core::ForgeMutation::begin(&storage, &workspace).unwrap().sync(&[]).unwrap();
    agl_runtime::resolve_workspace_forge(&catalog).unwrap();
    let data = root.join("home/data");
    let models = data.join("runtime/models");
    std::fs::create_dir_all(&models).unwrap();
    let model_path = models.join(format!(
        "{}.gguf",
        model_digest.to_string().strip_prefix("sha256:").unwrap()
    ));
    std::fs::write(&model_path, b"GGUF").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(model_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let socket = root.join("home/state/daemon/agl.sock");
    let create_server = || {
        DaemonServer::start(DaemonConfig { forge_sync_timeout_seconds: 300,
            data_root: data.clone(),
            state_root: root.join("home/state"),
            catalog_root: catalog.clone(),
            service_git: Default::default(),
            inference: InferenceConfig::custom(Arc::new(GatedGenerator {
                release: Arc::new(AtomicBool::new(false)),
            })),
            extensions: vec![],
            searxng: None,
            max_resident_bytes: None,
        })
        .unwrap()
    };
    let id = PackageId::new("fixture.function").unwrap();
    let server = create_server();
    let store = server.handle.store.clone();
    let function_directory = server
        .handle
        .runtime
        .scheduled_function_directory(&id)
        .unwrap();
    let task = tokio::spawn(server.serve(ListenerSource::Bind(socket.clone())));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let client = AgentClient::new(&socket);
    let available = client
        .schedules(Some(workspace.to_string_lossy().into_owned()))
        .await
        .unwrap();
    assert_eq!(available.len(), 1);
    assert!(!available[0].enabled);
    let enabled = client
        .enable_schedule(id.clone(), workspace.to_string_lossy())
        .await
        .unwrap();
    assert!(enabled.enabled);
    assert_eq!(enabled.permissions["files"], "none");
    assert_eq!(enabled.permissions["commands"], serde_json::json!([]));
    let anchor = store.schedules(Some(&workspace)).unwrap()[0].enabled_at_ms;
    let repeated = client
        .enable_schedule(id.clone(), workspace.to_string_lossy())
        .await
        .unwrap();
    assert_eq!(repeated.conversation_id, enabled.conversation_id);
    assert_eq!(
        store.schedules(Some(&workspace)).unwrap()[0].enabled_at_ms,
        anchor
    );
    let run = loop {
        let records = store.schedules(Some(&workspace)).unwrap();
        if let Some(run) = records[0].last_run {
            break run;
        }
        assert!(
            crate::agent::now_ms() - anchor < 4000,
            "timer did not admit Run"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(matches!(
        store.agent_run(run).unwrap().origin,
        AgentRunOrigin::Timer { .. }
    ));
    let manual_id = ConversationId::generate();
    client
        .open_conversation(
            manual_id,
            function_directory.to_string_lossy(),
            workspace.to_string_lossy(),
        )
        .await
        .unwrap();
    let error = client
        .start_run(AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id: manual_id,
                message_id: MessageId::generate(),
            },
            input: Content::text("manual task").unwrap(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            agl_daemon_api::ClientError::Protocol(ProtocolError {
                code: ProtocolErrorCode::Busy,
                ..
            })
        ),
        "{error:?}"
    );
    let disabled = client
        .disable_schedule(id.clone(), workspace.to_string_lossy())
        .await
        .unwrap();
    assert!(!disabled.enabled);
    for _ in 0..200 {
        if store.agent_run_view(run).unwrap().status.is_terminal() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        store.agent_run_view(run).unwrap().status,
        AgentRunStatus::Cancelled
    );
    task.abort();
    let _ = task.await;
    drop(store);
    let server = create_server();
    let store = server.handle.store.clone();
    let task = tokio::spawn(server.serve(ListenerSource::Bind(socket.clone())));
    for _ in 0..200 {
        if client.schedules(None).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let persisted = client
        .schedules(Some(workspace.to_string_lossy().into_owned()))
        .await
        .unwrap();
    assert!(!persisted[0].enabled);
    assert_eq!(persisted[0].last_run, Some(run));
    let resumed = client
        .enable_schedule(id.clone(), workspace.to_string_lossy())
        .await
        .unwrap();
    assert_eq!(resumed.conversation_id, enabled.conversation_id);
    assert!(store.schedules(Some(&workspace)).unwrap()[0].enabled_at_ms > anchor);
    client
        .disable_schedule(id.clone(), workspace.to_string_lossy())
        .await
        .unwrap();
    let previous_binding = store
        .conversation_binding(resumed.conversation_id.unwrap())
        .unwrap();
    std::fs::write(
        source.join("agent/SYSTEM.md"),
        "Changed dependency instructions.\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "Update fixture dependency"]);
    let updated_revision = git(&["rev-parse", "HEAD"]);
    for declaration in &mut declarations {
        declaration.revision = updated_revision.clone();
    }
    std::fs::write(
        catalog.join("agentLIBRE.toml"),
        agl_runtime::serialize_workspace_forge_declarations(&catalog, declarations.to_vec())
            .unwrap(),
    )
    .unwrap();
    let sources = agl_runtime::workspace_backend_declarations(&declarations).unwrap();
    ayeque_forge_core::ForgeMutation::begin(&storage, &catalog).unwrap().sync(&sources).unwrap();
    let updated = client
        .enable_schedule(id.clone(), workspace.to_string_lossy())
        .await
        .unwrap();
    assert_ne!(updated.conversation_id, resumed.conversation_id);
    assert_eq!(
        store
            .conversation_binding(resumed.conversation_id.unwrap())
            .unwrap(),
        previous_binding
    );
    client
        .disable_schedule(id, workspace.to_string_lossy())
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    drop(store);
    ayeque_forge_core::ForgeMutation::begin(&storage, &catalog).unwrap().sync(&[]).unwrap();
    ayeque_forge_core::ForgeMutation::begin(&storage, &workspace).unwrap().sync(&[]).unwrap();
}
