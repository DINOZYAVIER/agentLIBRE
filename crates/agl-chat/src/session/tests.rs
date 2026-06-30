use agl_config::{ModelDialect, ToolCallFormat};

use super::*;

#[test]
fn build_request_uses_agentlibre_boundaries() {
    let run_id = InferenceRunId::new("manual-test").unwrap();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };

    let request = build_inference_request(
        run_id.clone(),
        ModelRequest {
            turn_id: "manual-test".to_string(),
            request_index: 7,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        &config,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(request.run_id, run_id);
    assert_eq!(request.attempt_id.as_str(), "attempt-0007");
    assert_eq!(request.rendered.turn_id, "manual-test");
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
    let run_id = InferenceRunId::new("manual-test").unwrap();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };

    let request = build_inference_request(
        run_id,
        ModelRequest {
            turn_id: "manual-test".to_string(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        &config,
        Some("demo system"),
        None,
        None,
        None,
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
    let run_id = InferenceRunId::new("manual-test").unwrap();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };

    let request = build_inference_request(
        run_id,
        ModelRequest {
            turn_id: "manual-test".to_string(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        &config,
        Some("system"),
        None,
        None,
        Some("skill context"),
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 3);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert_eq!(request.rendered.messages[1].content, "skill context");
    assert_eq!(request.rendered.messages[2].content, "hello");
}

#[test]
fn build_request_prepends_memory_context_before_skill_context() {
    let run_id = InferenceRunId::new("manual-test").unwrap();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };

    let request = build_inference_request(
        run_id,
        ModelRequest {
            turn_id: "manual-test".to_string(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            visible_tools: Vec::new(),
        },
        &config,
        Some("system"),
        None,
        Some("memory context"),
        Some("skill context"),
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 4);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert_eq!(request.rendered.messages[1].content, "memory context");
    assert_eq!(request.rendered.messages[2].content, "skill context");
    assert_eq!(request.rendered.messages[3].content, "hello");
}

#[test]
fn build_request_injects_runtime_capabilities_before_tools() {
    let run_id = InferenceRunId::new("manual-test").unwrap();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let runtime_context = build_runtime_capability_context(
        std::path::Path::new("/repo"),
        ToolAccessMode::ReadOnly,
        &[
            VisibleTool::new("fs.list"),
            VisibleTool::new("fs.read"),
            VisibleTool::new("fs.search"),
        ],
    );

    let request = build_inference_request(
        run_id,
        ModelRequest {
            turn_id: "manual-test".to_string(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "can you run cron jobs?".to_string(),
            }],
            visible_tools: vec![
                VisibleTool::new("fs.list"),
                VisibleTool::new("fs.read"),
                VisibleTool::new("fs.search"),
            ],
        },
        &config,
        Some("system"),
        Some(&runtime_context.content),
        Some("memory context"),
        Some("skill context"),
    )
    .unwrap();

    assert_eq!(request.rendered.messages.len(), 6);
    assert_eq!(request.rendered.messages[0].content, "system");
    assert!(
        request.rendered.messages[1]
            .content
            .contains("<agentlibre_runtime_capabilities>")
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
    let run_id = InferenceRunId::new("manual-test").unwrap();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };

    let request = build_inference_request(
        run_id,
        ModelRequest {
            turn_id: "manual-test".to_string(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: "read README".to_string(),
            }],
            visible_tools: vec![
                VisibleTool::new("fs.read")
                    .describe("Read a repository file")
                    .require_argument("path"),
            ],
        },
        &config,
        Some("system"),
        None,
        None,
        Some("skill context"),
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
fn selected_skill_ids_rejects_duplicates_across_config_and_cli() {
    let err =
        selected_skill_ids(&["task-spec".to_string()], &["task-spec".to_string()]).unwrap_err();

    assert!(err.to_string().contains("selected skill id is duplicated"));
}

#[test]
fn selected_skill_hook_batches_use_declared_hook_events() {
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
            .map(|tool| tool.name.as_str())
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
    assert_eq!(
        tools[0].required_arguments,
        vec!["path", "old_text", "new_text"]
    );
    assert!(tools[0].description.contains("exact text"));
}

#[test]
fn visible_tools_include_read_only_core_tools_without_skills() {
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
            .map(|tool| tool.name.as_str())
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
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
            .map(|tool| tool.name.as_str())
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
fn approve_mode_includes_permission_approval_tools_without_broad_write_hack() {
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
            .map(|tool| tool.name.as_str())
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
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
            .map(|tool| tool.name.as_str())
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
    let skill_registry = agl_skills::builtin_registry().unwrap();
    let catalog = full_tool_catalog();
    let run_id = InferenceRunId::new("manual-grant-test").unwrap();

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

    let tool_names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"cron.add"));
    assert!(!tool_names.contains(&"cron.delete"));
    assert_eq!(snapshot.granted_visible_tools(), vec!["cron.add"]);
    assert!(snapshot.ignored_grants().is_empty());
    assert!(store.active_permission_grants().unwrap().is_empty());
    let consumed = store.permission_grant(&grant.id).unwrap().unwrap();
    assert_eq!(consumed.status, agl_store::PermissionGrantStatus::Expired);
    assert_eq!(
        consumed.last_admitted_run_id.as_deref(),
        Some("manual-grant-test")
    );
    assert!(consumed.consumed_at.is_some());

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
    let skill_registry = agl_skills::builtin_registry().unwrap();
    let catalog = full_tool_catalog();
    let run_id = InferenceRunId::new("manual-denied-test").unwrap();

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

    assert!(!tools.iter().any(|tool| tool.name == "notes.delete"));
    assert!(snapshot.granted_visible_tools().is_empty());
    assert!(
        snapshot.ignored_grants()[0].contains("notes.delete:denied_by_selected_skill"),
        "{:?}",
        snapshot.ignored_grants()
    );
    assert_eq!(store.active_permission_grants().unwrap().len(), 1);

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

fn temp_store_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("agl-chat-{label}-{}-{nanos}", std::process::id()))
}

#[test]
fn selected_tool_smoke_skill_uses_read_only_core_tool_set() {
    let skill_registry = agl_skills::builtin_registry().unwrap();
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
            .map(|tool| tool.name.as_str())
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
fn resolves_default_paths_from_runtime_config() {
    let runtime = AgentLibreRuntimeConfig {
        paths: agl_runtime::AgentLibrePaths::from_agl_home("/tmp/agl-home"),
        logging: agl_runtime::AgentLibreLoggingConfig::default(),
        history: agl_runtime::AgentLibreHistoryConfig::default(),
        workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
    };
    let options = InferenceOptions::default();

    assert_eq!(
        InferenceSession::resolve_config_path(&options, &runtime),
        PathBuf::from("/tmp/agl-home/config/inference/local.toml")
    );
    assert_eq!(
        InferenceSession::default_artifact_root(&runtime),
        PathBuf::from("/tmp/agl-home/data/runs")
    );
}

#[test]
fn agent_event_stream_is_separate_from_inference_evidence_events() {
    let run_id = InferenceRunId::new("run-001").unwrap();

    assert_eq!(
        agent_event_stream_path(std::path::Path::new("/tmp/artifacts"), &run_id),
        PathBuf::from("/tmp/artifacts/inference-runs/run-001/agent-events.jsonl")
    );
}
