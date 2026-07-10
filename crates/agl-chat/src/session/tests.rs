use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelDialect, ToolCallFormat};
use agl_ids::{RequestId, RunId, SessionId, TurnId};

use super::*;

const TEST_RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
const TEST_TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";
const TEST_SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000003";
const TEST_REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000004";

fn run_id() -> RunId {
    RunId::parse(TEST_RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TEST_TURN_ID).unwrap()
}

fn session_id() -> SessionId {
    SessionId::parse(TEST_SESSION_ID).unwrap()
}

fn request_id() -> RequestId {
    RequestId::parse(TEST_REQUEST_ID).unwrap()
}

fn effective_capabilities(ids: &[&str]) -> EffectiveCapabilitySet {
    let catalog = full_tool_catalog();
    CapabilityPolicyInput::new(
        catalog.providers().iter().cloned(),
        tool_ids(ids),
        ToolAccessMode::Admin,
    )
    .resolve()
    .unwrap()
}

#[test]
fn build_request_uses_agentlibre_boundaries() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let session_id = session_id();
    let request_id = request_id();
    let effective = effective_capabilities(&[]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 7,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            session_id: Some(&session_id),
            request_id: Some(&request_id),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.run_id, run_id());
    assert_eq!(request.turn_id, turn_id());
    assert_eq!(request.session_id, Some(session_id));
    assert_eq!(request.request_id, Some(request_id));
    assert!(request.attempt_id.as_str().starts_with("attempt_"));
    assert_eq!(request.rendered.run_id, run_id());
    assert_eq!(request.rendered.turn_id, turn_id());
    assert_eq!(request.rendered.request_index, 7);
    assert_eq!(request.rendered.messages.len(), 1);
    assert_eq!(request.rendered.dialect, ModelDialect::Qwen3);
    assert_eq!(
        request.rendered.tool_call_format,
        ToolCallFormat::HermesJson
    );
}

#[test]
fn build_request_prepends_configured_system_prompt() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&[]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            system_prompt: Some("demo system"),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 2);
    assert_eq!(
        request.rendered.messages[0].role,
        agl_oven::RenderedMessageRole::System
    );
    assert_eq!(request.rendered.messages[0].content, "demo system");
    assert_eq!(
        request.rendered.messages[1].role,
        agl_oven::RenderedMessageRole::User
    );
    assert_eq!(request.rendered.messages[1].content, "hello");
}

#[test]
fn build_request_prepends_skill_context_after_system_prompt() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&[]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            system_prompt: Some("system"),
            skill_context: Some("skill context"),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 3);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert_eq!(request.rendered.messages[1].content, "skill context");
    assert_eq!(request.rendered.messages[2].content, "hello");
}

#[test]
fn build_request_prepends_memory_context_before_skill_context() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&[]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            system_prompt: Some("system"),
            memory_context: Some("memory context"),
            skill_context: Some("skill context"),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 4);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert_eq!(request.rendered.messages[1].content, "memory context");
    assert_eq!(request.rendered.messages[2].content, "skill context");
    assert_eq!(request.rendered.messages[3].content, "hello");
}

#[test]
fn build_request_injects_runtime_features_before_tools() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&["fs.list", "fs.read", "fs.search"]);
    let visible_tools = visible_tools_from_effective(&effective);
    let runtime_context = build_runtime_feature_context(
        std::path::Path::new("/repo"),
        ToolAccessMode::ReadOnly,
        &visible_tools,
    );

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "can you run cron jobs?".to_string(),
            }],
            visible_tools,
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            system_prompt: Some("system"),
            runtime_feature_context: Some(&runtime_context.content),
            memory_context: Some("memory context"),
            skill_context: Some("skill context"),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 6);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert!(
        request.rendered.messages[1]
            .content
            .contains("<agentlibre_runtime_features>")
    );
    assert!(request.rendered.messages[1].content.contains("- cron:"));
    assert!(
        request.rendered.messages[1]
            .content
            .contains("tool_mode: read-only")
    );
    assert!(
        request.rendered.messages[1]
            .content
            .contains("read-only: list, show, history, preflight")
    );
    assert!(
        request.rendered.messages[1]
            .content
            .contains("write: add, delete, run, tick")
    );
    assert_eq!(request.rendered.messages[2].content, "memory context");
    assert_eq!(request.rendered.messages[3].content, "skill context");
    assert!(
        request.rendered.messages[4]
            .content
            .contains("<agentlibre_tool_context>")
    );
    assert_eq!(
        request.rendered.messages[5].content,
        "can you run cron jobs?"
    );
}

#[test]
fn build_request_injects_visible_tool_context_for_hermes() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&["fs.read"]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "read README".to_string(),
            }],
            visible_tools: visible_tools_from_effective(&effective),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            system_prompt: Some("system"),
            skill_context: Some("skill context"),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 4);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert_eq!(request.rendered.messages[1].content, "skill context");
    assert!(request.rendered.messages[2].content.contains("fs.read"));
    assert!(request.rendered.messages[2].content.contains("<tool_call>"));
    assert_eq!(request.rendered.messages[3].content, "read README");
    assert_eq!(request.rendered.tools[0].name, "fs.read");
}

#[test]
fn build_request_injects_visible_tool_context_for_gemma() {
    let config = ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    };
    let effective = effective_capabilities(&["fs.read"]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "read README".to_string(),
            }],
            visible_tools: visible_tools_from_effective(&effective),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            system_prompt: Some("system"),
            skill_context: Some("skill context"),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 4);
    let tool_context = &request.rendered.messages[2].content;
    assert!(tool_context.contains("# GEMMA NATIVE TOOL CALLING"));
    assert!(tool_context.contains("<|tool_call>call:TOOL_NAME"));
    assert!(tool_context.contains("fs.read"));
    assert!(!tool_context.contains(r#"{"name":"TOOL_NAME""#));
    assert_eq!(request.rendered.messages[3].content, "read README");
    assert_eq!(request.rendered.tools[0].name, "fs.read");
}

#[test]
fn hermes_and_gemma_render_the_same_complete_input_schema() {
    let effective = effective_capabilities(&["fs.read"]);
    let declaration = effective
        .capability(&CapabilityId::new("fs.read").unwrap())
        .unwrap()
        .declaration();
    let schema_record = render_action_schema(declaration);

    let hermes = render_hermes_tool_context(&effective);
    let gemma = render_gemma_tool_context(&effective);

    assert!(hermes.contains(&schema_record));
    assert!(gemma.contains(&schema_record));
    assert!(schema_record.contains(r#""additionalProperties":false"#));
    assert!(schema_record.contains(r#""path":{"type":"string"}"#));
}

#[test]
fn selected_skill_ids_deduplicates_across_config_function_and_cli() {
    let selected = selected_skill_ids(
        &["task-spec".to_string()],
        &["task-spec".to_string(), "repo-status".to_string()],
        &["repo-status".to_string()],
    )
    .unwrap();

    let names = selected
        .iter()
        .map(|skill| skill.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["task-spec", "repo-status"]);
}

#[test]
fn artifact_write_preflight_normalizes_only_agl_paths() {
    let normal = normalize_agl_artifact_write_path(&serde_json::json!({
        "path": "README.md"
    }))
    .unwrap();
    assert_eq!(normal, None);

    let agl = normalize_agl_artifact_write_path(&serde_json::json!({
        "path": ".agl/tasks/example.md"
    }))
    .unwrap();
    assert_eq!(agl, Some(PathBuf::from(".agl/tasks/example.md")));
}

#[test]
fn artifact_write_preflight_rejects_parent_traversal() {
    let err = normalize_agl_artifact_write_path(&serde_json::json!({
        "path": ".agl/tasks/../secret.md"
    }))
    .unwrap_err();

    assert!(err.to_string().contains("parent traversal"));
}

#[test]
fn artifact_write_preflight_is_limited_to_fs_edit_selected_skills_and_agl_paths() {
    let selected_skills = [SkillId::new("task-spec").unwrap()];
    let agl_args = serde_json::json!({
        "path": ".agl/tasks/example.md"
    });

    assert_eq!(
        artifact_write_preflight_path_for_tool(
            agl_tools::FS_EDIT_TOOL_ID,
            &selected_skills,
            &agl_args
        )
        .unwrap(),
        Some(PathBuf::from(".agl/tasks/example.md"))
    );
    assert_eq!(
        artifact_write_preflight_path_for_tool("skill.status", &selected_skills, &agl_args)
            .unwrap(),
        None
    );
    assert_eq!(
        artifact_write_preflight_path_for_tool(agl_tools::FS_EDIT_TOOL_ID, &[], &agl_args).unwrap(),
        None
    );
    assert_eq!(
        artifact_write_preflight_path_for_tool(
            agl_tools::FS_EDIT_TOOL_ID,
            &selected_skills,
            &serde_json::json!({
                "path": "README.md"
            })
        )
        .unwrap(),
        None
    );
}

#[test]
fn selected_skill_hook_batches_use_declared_hook_events() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let batches = selected_skill_hook_batches(
        &skill_registry,
        &extension_registry,
        &[SkillId::new("task-spec").unwrap()],
    )
    .unwrap();

    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].event, HookEvent::ArtifactWrite);
    assert_eq!(
        batches[0]
            .required_hooks
            .iter()
            .map(HookId::as_str)
            .collect::<Vec<_>>(),
        vec!["repo_path.validate", "task_spec.validate"]
    );
    assert!(batches[0].optional_hooks.is_empty());
}

#[test]
fn selected_skill_visible_tools_use_declared_tool_metadata() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &extension_registry,
        &[SkillId::new("task-spec").unwrap()],
        ToolAccessMode::Write,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fs.edit", "fs.list", "fs.read", "fs.search"]
    );
    assert_eq!(
        tools[0].input_schema["required"],
        serde_json::json!(["path", "old_text", "new_text"])
    );
    assert_eq!(tools[0].input_schema["additionalProperties"], false);
    assert!(tools[0].description.contains("exact text"));
}

#[test]
fn visible_tools_include_read_only_core_tools_without_skills() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &extension_registry,
        &[],
        ToolAccessMode::ReadOnly,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "fs.list",
            "fs.read",
            "fs.search",
            "permissions.request",
            "permissions.status",
            "skill.inspect",
            "skill.list",
            "skill.status",
            "skill.verify",
        ]
    );
}

#[test]
fn visible_tools_include_edit_in_write_mode_without_skills() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &extension_registry,
        &[],
        ToolAccessMode::Write,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "fs.edit",
            "fs.list",
            "fs.read",
            "fs.search",
            "permissions.request",
            "permissions.status",
            "skill.inspect",
            "skill.list",
            "skill.status",
            "skill.verify",
        ]
    );
}

#[test]
fn function_policy_absence_empty_allow_and_deny_precedence_are_distinct() {
    let registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let fs_read = CapabilityId::new("fs.read").unwrap();

    let inherited = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        None,
    )
    .unwrap();
    assert!(inherited.contains(&fs_read));

    let empty_allow = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        Some(FunctionToolPolicy::default()),
    )
    .unwrap();
    assert!(!empty_allow.contains(&fs_read));
    assert_eq!(
        empty_allow.exclusion(&fs_read).unwrap().reason,
        agl_capabilities::CapabilityExclusionReason::FunctionAllowDenied
    );

    let denied = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        Some(FunctionToolPolicy::new(
            [fs_read.clone()],
            [fs_read.clone()],
        )),
    )
    .unwrap();
    assert!(!denied.contains(&fs_read));
    assert_eq!(
        denied.exclusion(&fs_read).unwrap().reason,
        agl_capabilities::CapabilityExclusionReason::FunctionDenied
    );
}

#[test]
fn function_manifest_policy_controls_session_effective_visible_and_prompt_tools() {
    struct Case {
        id: &'static str,
        tools_yaml: &'static str,
        expected_ids: &'static [&'static str],
        policy_present: bool,
    }

    let cases = [
        Case {
            id: "policy-absent",
            tools_yaml: "",
            expected_ids: &[
                "fs.list",
                "fs.read",
                "fs.search",
                "permissions.request",
                "permissions.status",
                "skill.inspect",
                "skill.list",
                "skill.status",
                "skill.verify",
            ],
            policy_present: false,
        },
        Case {
            id: "policy-empty",
            tools_yaml: "tools: {}\n",
            expected_ids: &[],
            policy_present: true,
        },
        Case {
            id: "policy-allow-deny",
            tools_yaml: "tools:\n  allow:\n    - fs.list\n    - fs.read\n  deny:\n    - fs.list\n",
            expected_ids: &["fs.read"],
            policy_present: true,
        },
    ];
    let root = temp_store_root("function-policy-session");
    let workspace = root.join("workspace");
    let config_path = root.join("inference.toml");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        &config_path,
        format!(
            r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 128
threads = 1
batch_size = 16
ubatch_size = 16

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
            root.join("missing-model.gguf").display()
        ),
    )
    .unwrap();
    let runtime = AgentLibreRuntimeConfig {
        paths: agl_runtime::AgentLibrePaths::from_agl_home(root.join("home")),
        logging: agl_runtime::AgentLibreLoggingConfig::default(),
        history: agl_runtime::AgentLibreHistoryConfig::default(),
        workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
    };
    let catalog = full_tool_catalog();
    let catalog_ids = catalog
        .providers()
        .iter()
        .flat_map(|provider| provider.actions.iter())
        .map(|action| action.id.as_str())
        .collect::<BTreeSet<_>>();

    for case in cases {
        let function_root = workspace.join(".agl/functions").join(case.id);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join(agl_functions::FUNCTION_FILE_NAME),
            format!(
                "---\nschema: agentfunction/v1\nid: {}\ntitle: Function policy test\n{}---\n",
                case.id, case.tools_yaml
            ),
        )
        .unwrap();
        std::fs::write(
            function_root.join(agl_functions::FUNCTION_SYSTEM_PROMPT_FILE_NAME),
            "Apply the function policy.\n",
        )
        .unwrap();

        let session = InferenceSession::new(
            InferenceOptions {
                config: Some(config_path.clone()),
                function_ref: Some(case.id.to_string()),
                artifact_root: Some(root.join("artifacts").join(case.id)),
                workspace_root: Some(workspace.clone()),
                ..Default::default()
            },
            &runtime,
            None,
        )
        .unwrap();
        assert_eq!(
            session
                .runtime_function
                .as_ref()
                .unwrap()
                .tool_policy
                .is_some(),
            case.policy_present,
            "{}",
            case.id
        );

        let visible_ids = session
            .turn_visible_tools()
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>();
        let effective_ids = session
            .effective_capabilities()
            .capabilities()
            .map(|capability| capability.declaration().id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(visible_ids, case.expected_ids, "{} visible", case.id);
        assert_eq!(effective_ids, case.expected_ids, "{} effective", case.id);

        let request = build_inference_request(
            ModelRequest {
                run_id: run_id(),
                turn_id: turn_id(),
                request_index: 0,
                messages: vec![TurnMessage::User {
                    content: "test policy".to_string(),
                }],
                visible_tools: session.turn_visible_tools().to_vec(),
            },
            AttemptId::generate(),
            &session.model_config,
            InferenceRequestContexts {
                system_prompt: session.system_prompt.as_deref(),
                runtime_feature_context: session.runtime_feature_context.as_deref(),
                function_context: session.function_context.as_deref(),
                memory_context: session.memory_context.as_deref(),
                skill_context: session.skill_context.as_deref(),
                effective_capabilities: Some(session.effective_capabilities()),
                ..Default::default()
            },
        )
        .unwrap();
        let prompt_ids = request
            .rendered
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(prompt_ids, case.expected_ids, "{} prompt tools", case.id);

        let tool_context = request
            .rendered
            .messages
            .iter()
            .find(|message| message.content.contains("<agentlibre_tool_context>"))
            .map(|message| message.content.as_str());
        assert_eq!(
            tool_context.is_some(),
            !case.expected_ids.is_empty(),
            "{} textual tool context",
            case.id
        );
        let expected = case.expected_ids.iter().copied().collect::<BTreeSet<_>>();
        for capability_id in &catalog_ids {
            let marker = format!(r#""name":"{capability_id}""#);
            assert_eq!(
                tool_context.is_some_and(|context| context.contains(&marker)),
                expected.contains(capability_id),
                "{} textual prompt capability {}",
                case.id,
                capability_id
            );
        }
    }

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn dynamic_grant_cannot_exceed_the_run_tool_mode() {
    let registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let cron_add = CapabilityId::new("cron.add").unwrap();
    let snapshot = RuntimePermissionGrantSnapshot {
        admitted: vec![AdmittedPermissionGrant {
            grant_id: "grant-1".to_string(),
            capability_id: cron_add.clone(),
            max_operation_kind: OperationKind::Write,
            state_effects: BTreeSet::from([StateEffect::StoreCron]),
        }],
        ignored: Vec::new(),
    };

    let effective = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &snapshot,
        None,
    )
    .unwrap();

    assert!(!effective.contains(&cron_add));
    assert_eq!(
        effective.exclusion(&cron_add).unwrap().reason,
        agl_capabilities::CapabilityExclusionReason::ToolModeDenied
    );
}

#[test]
fn approve_mode_includes_permission_approval_tools_without_broad_write_hack() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &extension_registry,
        &[],
        ToolAccessMode::Approve,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "fs.edit",
            "fs.list",
            "fs.read",
            "fs.search",
            "permissions.grant",
            "permissions.request",
            "permissions.revoke",
            "permissions.status",
            "skill.inspect",
            "skill.list",
            "skill.status",
            "skill.verify",
        ]
    );
}

#[test]
fn selected_skill_visible_tools_hide_write_tools_in_read_only_mode() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &extension_registry,
        &[SkillId::new("task-spec").unwrap()],
        ToolAccessMode::ReadOnly,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fs.list", "fs.read", "fs.search"]
    );
}

#[test]
fn dynamic_grant_admits_exact_tool_and_expires_one_turn() {
    let root = temp_store_root("grant-cron");
    let store = AglStore::open_at(&root).unwrap();
    let grant = store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: "cron.add".to_string(),
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_cron".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let run_id = run_id();

    let (tools, snapshot) = selected_skill_visible_tools_with_dynamic_grants(
        &skill_registry,
        &catalog,
        &[],
        ToolAccessMode::Write,
        &root,
        std::path::Path::new("/repo"),
        &run_id,
    )
    .unwrap();

    let tool_names = tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"cron.add"));
    assert!(!tool_names.contains(&"cron.delete"));
    assert_eq!(snapshot.granted_visible_tools(), vec!["cron.add"]);
    assert!(snapshot.ignored_grants().is_empty());
    assert!(store.active_permission_grants().unwrap().is_empty());
    let consumed = store.permission_grant(&grant.id).unwrap().unwrap();
    assert_eq!(consumed.status, agl_store::PermissionGrantStatus::Expired);
    assert_eq!(consumed.last_admitted_run_id.as_deref(), Some(TEST_RUN_ID));
    assert!(consumed.consumed_at.is_some());

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn dynamic_grant_blocked_by_tool_mode_is_not_consumed() {
    let root = temp_store_root("grant-mode-blocked");
    let store = AglStore::open_at(&root).unwrap();
    store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: "cron.add".to_string(),
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_cron".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let run_id = run_id();

    let (tools, snapshot) = selected_skill_visible_tools_with_dynamic_grants(
        &skill_registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &root,
        std::path::Path::new("/repo"),
        &run_id,
    )
    .unwrap();

    assert!(!tools.iter().any(|tool| tool.id.as_str() == "cron.add"));
    assert!(snapshot.granted_visible_tools().is_empty());
    assert!(
        snapshot
            .ignored_grants()
            .iter()
            .any(|grant| grant.contains("cron.add:tool_mode_denied"))
    );
    assert_eq!(store.active_permission_grants().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn dynamic_grant_denied_by_selected_skill_is_ignored() {
    let root = temp_store_root("grant-denied");
    let store = AglStore::open_at(&root).unwrap();
    store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: "notes.delete".to_string(),
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_notes".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let run_id = run_id();

    let (tools, snapshot) = selected_skill_visible_tools_with_dynamic_grants(
        &skill_registry,
        &catalog,
        &[SkillId::new("notes-capture").unwrap()],
        ToolAccessMode::ReadOnly,
        &root,
        std::path::Path::new("/repo"),
        &run_id,
    )
    .unwrap();

    assert!(!tools.iter().any(|tool| tool.id.as_str() == "notes.delete"));
    assert!(snapshot.granted_visible_tools().is_empty());
    assert!(
        snapshot.ignored_grants()[0].contains("notes.delete:denied_by_selected_skill"),
        "{:?}",
        snapshot.ignored_grants()
    );
    assert_eq!(store.active_permission_grants().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn dynamic_grant_not_routed_by_selected_skill_is_ignored() {
    let root = temp_store_root("grant-not-routed");
    let store = AglStore::open_at(&root).unwrap();
    store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: "cron.add".to_string(),
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_cron".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let run_id = run_id();

    let (tools, snapshot) = selected_skill_visible_tools_with_dynamic_grants(
        &skill_registry,
        &catalog,
        &[SkillId::new("tool-smoke").unwrap()],
        ToolAccessMode::ReadOnly,
        &root,
        std::path::Path::new("/repo"),
        &run_id,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fs.read"]
    );
    assert!(snapshot.granted_visible_tools().is_empty());
    assert!(
        snapshot
            .ignored_grants()
            .iter()
            .any(|grant| grant.contains("cron.add:not_routed_by_selected_skill")),
        "{:?}",
        snapshot.ignored_grants()
    );
    assert_eq!(store.active_permission_grants().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn selected_cron_planner_can_request_but_not_call_requestable_tools() {
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &catalog,
        &[SkillId::new("cron-planner").unwrap()],
        ToolAccessMode::ReadOnly,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "cron.preflight",
            "fs.read",
            "fs.search",
            "permissions.request",
            "permissions.status",
        ]
    );
    assert!(!tools.iter().any(|tool| tool.id.as_str() == "cron.add"));
    assert!(
        !tools
            .iter()
            .any(|tool| tool.id.as_str() == "matrix.outbox.enqueue")
    );
}

#[test]
fn selected_cron_planner_admits_requestable_tool_after_grant() {
    let root = temp_store_root("grant-cron-selected");
    let store = AglStore::open_at(&root).unwrap();
    store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: "cron.add".to_string(),
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_cron".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let run_id = run_id();

    let (tools, snapshot) = selected_skill_visible_tools_with_dynamic_grants(
        &skill_registry,
        &catalog,
        &[SkillId::new("cron-planner").unwrap()],
        ToolAccessMode::Write,
        &root,
        std::path::Path::new("/repo"),
        &run_id,
    )
    .unwrap();

    assert!(tools.iter().any(|tool| tool.id.as_str() == "cron.add"));
    assert_eq!(snapshot.granted_visible_tools(), vec!["cron.add"]);
    assert!(store.active_permission_grants().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(root);
}

fn full_tool_catalog() -> ToolCatalog {
    let mut catalog = ToolCatalog::new();
    agl_tools::guards::register(&mut catalog).unwrap();
    agl_tools::cron::register(&mut catalog).unwrap();
    agl_tools::fs::register(&mut catalog).unwrap();
    agl_tools::matrix::register(&mut catalog).unwrap();
    agl_tools::memory::register(&mut catalog).unwrap();
    agl_tools::notes::register(&mut catalog).unwrap();
    agl_tools::permissions::register(&mut catalog).unwrap();
    agl_tools::repo::register(&mut catalog).unwrap();
    agl_tools::skills::register(&mut catalog).unwrap();
    agl_tools::store::register(&mut catalog).unwrap();
    catalog
}

fn test_skill_registry() -> agl_skills::SkillRegistry {
    let mut registry = agl_skills::builtin_registry().unwrap();
    for skill in [
        test_skill(
            "task-spec",
            &["repo_path.validate", "task_spec.validate"],
            &["fs.edit", "fs.list", "fs.read", "fs.search"],
            &[],
            &[],
            Vec::new(),
        ),
        test_skill(
            "tool-smoke",
            &["repo_path.validate"],
            &["fs.read"],
            &[],
            &[],
            Vec::new(),
        ),
        test_skill(
            "notes-capture",
            &["repo_path.validate"],
            &["notes.add", "notes.link"],
            &[],
            &["notes.delete"],
            Vec::new(),
        ),
        test_skill(
            "cron-planner",
            &["repo_path.validate"],
            &[
                "cron.preflight",
                "fs.read",
                "fs.search",
                "permissions.request",
                "permissions.status",
            ],
            &["cron.add", "matrix.outbox.enqueue"],
            &["matrix.outbox.deliver"],
            vec![agl_skills::SkillPermissionRequestTemplate {
                id: "schedule-matrix-cron".to_string(),
                tools: tool_ids(&["cron.add", "matrix.outbox.enqueue"]),
                max_operation_kind: Some(OperationKind::Write),
                state_effects: vec![StateEffect::StoreCron, StateEffect::MatrixOutbox],
                default_duration: "one_turn".to_string(),
                reason_template: "Schedule a Matrix notification cron job.".to_string(),
            }],
        ),
    ] {
        registry
            .register(agl_skills::RegisteredSkill::trusted_builtin(skill))
            .unwrap();
    }
    registry
}

fn test_skill(
    id: &str,
    required_hooks: &[&str],
    allowed_tools: &[&str],
    requestable_tools: &[&str],
    denied_tools: &[&str],
    permission_request_templates: Vec<agl_skills::SkillPermissionRequestTemplate>,
) -> agl_skills::SkillHarness {
    agl_skills::SkillHarness {
        id: SkillId::new(id).unwrap(),
        name: id.to_string(),
        description: format!("Test-only {id} skill."),
        version: 1,
        source: agl_skills::SkillSource::Core,
        pack: "test".to_string(),
        required_hooks: hook_ids(required_hooks),
        allowed_tools: tool_ids(allowed_tools),
        requestable_tools: tool_ids(requestable_tools),
        denied_tools: tool_ids(denied_tools),
        permission_request_templates,
        permissions: agl_skills::SkillPermissions::default(),
        context_budget_tokens: 512,
        reference_policy: agl_skills::SkillReferencePolicy {
            include: Vec::new(),
        },
        references: Vec::new(),
        artifacts: Vec::new(),
        guarantees: vec!["test fixture is trusted by construction".to_string()],
        body: format!("Use this test-only {id} skill."),
        source_path: format!("test/{id}/SKILL.md"),
        manifest_sha256: "0".repeat(64),
        tree_sha256: "1".repeat(64),
    }
}

fn hook_ids(values: &[&str]) -> Vec<HookId> {
    values
        .iter()
        .map(|value| HookId::new(*value).unwrap())
        .collect()
}

fn tool_ids(values: &[&str]) -> Vec<CapabilityId> {
    values
        .iter()
        .map(|value| CapabilityId::new(*value).unwrap())
        .collect()
}

fn temp_store_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("agl-chat-{label}-{}-{nanos}", std::process::id()))
}

#[test]
fn selected_tool_smoke_skill_exposes_only_declared_tool() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_tools::guards::register(&mut extension_registry).unwrap();
    agl_tools::fs::register(&mut extension_registry).unwrap();
    agl_tools::permissions::register(&mut extension_registry).unwrap();
    agl_tools::skills::register(&mut extension_registry).unwrap();

    let tools = selected_skill_visible_tools(
        &skill_registry,
        &extension_registry,
        &[SkillId::new("tool-smoke").unwrap()],
        ToolAccessMode::ReadOnly,
    )
    .unwrap();

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fs.read"]
    );
}

#[test]
fn resolves_default_paths_from_runtime_config() {
    let runtime = AgentLibreRuntimeConfig {
        paths: agl_runtime::AgentLibrePaths::from_agl_home("/tmp/agl-home"),
        logging: agl_runtime::AgentLibreLoggingConfig::default(),
        history: agl_runtime::AgentLibreHistoryConfig::default(),
        workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
    };
    let options = InferenceOptions::default();

    assert_eq!(
        InferenceSession::resolve_config_path(&options, &runtime, None),
        PathBuf::from("/tmp/agl-home/config/inference/local.toml")
    );
    assert_eq!(
        InferenceSession::default_artifact_root(&runtime),
        PathBuf::from("/tmp/agl-home/data")
    );
}

#[test]
fn agent_event_stream_uses_canonical_run_event_path() {
    let run_id = run_id();

    assert_eq!(
        agent_event_stream_path(std::path::Path::new("/tmp/artifacts"), &run_id),
        PathBuf::from(format!("/tmp/artifacts/runs/{TEST_RUN_ID}/events.jsonl"))
    );
}
