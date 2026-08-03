use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelDialect, ToolCallFormat};
use agl_content::{Content, ContentPart};
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

fn text(value: impl Into<String>) -> Content {
    Content::text(value).unwrap()
}

trait TestRenderedContent {
    fn as_str(&self) -> &str;

    fn contains(&self, needle: &str) -> bool {
        self.as_str().contains(needle)
    }
}

impl TestRenderedContent for Option<Content> {
    fn as_str(&self) -> &str {
        let content = self.as_ref().expect("expected rendered content");
        match content.parts.as_slice() {
            [ContentPart::Text { text }] => text,
            _ => panic!("expected one rendered text part"),
        }
    }
}

fn effective_capabilities(ids: &[&str]) -> EffectiveToolSet {
    let catalog = full_tool_catalog();
    ToolPolicyInput::new(
        catalog.providers().iter().cloned(),
        tool_ids(ids),
        ToolAccessMode::Admin,
    )
    .resolve()
    .unwrap()
}

#[test]
fn external_same_id_extension_cannot_bind_the_builtin_provider() {
    let root =
        std::env::temp_dir().join(format!("agl-chat-extension-binding-{}", std::process::id()));
    let workspace = root.join("workspace");
    let function_root = workspace.join(".agl/functions/external-extension");
    let extension_root = workspace.join(".agl/extensions/core.workspace");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::create_dir_all(&extension_root).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: external-extension
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - extension:core.workspace@^1.0
title: External extension fixture
runtime:
  tool_mode: read-only
  max_output_tokens: 32
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply without tools."
---
"#,
    )
    .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Do not execute tools.\n").unwrap();
    std::fs::write(
        extension_root.join("EXTENSION.json"),
        r#"{
  "schema": "agentlibre.artifact/v1",
  "type": "extension",
  "id": "core.workspace",
  "version": "1.1.0",
  "payload_schema": "agentlibre.extension/v1",
  "agl": {
    "compatible": ">=1.0.0-alpha.12",
    "tested": ["1.0.0-alpha.12"]
  },
  "requires": [],
  "api_major": 1,
  "implementation": "builtin:agl-core-tools/fs"
}
"#,
    )
    .unwrap();
    let paths = AgentLibrePaths::from_agl_home(root.join("home"));
    let composition = agl_runtime::compose_artifacts(&paths, &workspace).unwrap();
    let bundle = composition
        .resolve_runtime_bundle(
            &workspace,
            &paths.config_dir,
            "external-extension",
            false,
            &[],
        )
        .unwrap();
    assert_eq!(
        bundle.graph.nodes[&bundle.extensions["core.workspace"].node_key]
            .candidate
            .tier,
        ArtifactSourceTier::Workspace
    );

    let error = bind_runtime_extensions(&bundle).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot bind to a builtin provider")
    );
    assert!(error.to_string().contains("no Tool effect occurred"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn selected_skill_context_uses_the_admitted_bundle_bytes() {
    let root = std::env::temp_dir().join(format!("agl-chat-skill-snapshot-{}", std::process::id()));
    let workspace = root.join("workspace");
    let function_root = workspace.join(".agl/functions/skill-snapshot");
    let skill_root = workspace.join(".agl/skills/snapshot-skill");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::create_dir_all(&skill_root).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: skill-snapshot
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - skill:snapshot-skill@^1.0
title: Skill snapshot fixture
runtime:
  tool_mode: read-only
  max_output_tokens: 32
skills:
  use: [snapshot-skill]
subagents:
  use: []
doctor:
  smoke_prompt: "Reply without tools."
---
"#,
    )
    .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Use the selected skill.\n").unwrap();
    let skill_document = |body: &str| {
        format!(
            r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: skill
  id: snapshot-skill
  version: 1.0.0
  payload_schema: agentlibre.skill/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
description: Snapshot test skill.
pack: test
required_hooks: []
allowed_tools: []
context_budget_tokens: 256
references:
  include: []
guarantees:
  - admitted bytes remain stable
---

{body}
"#
        )
    };
    std::fs::write(skill_root.join("SKILL.md"), skill_document("admitted body")).unwrap();
    let paths = AgentLibrePaths::from_agl_home(root.join("home"));
    let composition = agl_runtime::compose_artifacts(&paths, &workspace).unwrap();
    let bundle = composition
        .resolve_runtime_bundle(&workspace, &paths.config_dir, "skill-snapshot", false, &[])
        .unwrap();
    std::fs::write(skill_root.join("SKILL.md"), skill_document("mutated body")).unwrap();
    let selected = vec![SkillId::new("snapshot-skill").unwrap()];
    let registry = composed_skill_registry(
        &paths,
        &workspace,
        &paths.state_dir.join("skill-trust.toml"),
        &selected,
        Some(&bundle),
    )
    .unwrap();
    let harness = &registry.get(&selected[0]).unwrap().harness;
    assert!(harness.body.contains("admitted body"));
    assert!(!harness.body.contains("mutated body"));
    assert_eq!(bundle.skills["snapshot-skill"].harness.body, harness.body);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn runtime_resolution_is_one_atomic_canonical_record() {
    let root = temp_store_root("runtime-resolution-record");
    let workspace = root.join("workspace");
    let function_root = workspace.join(".agl/functions/evidence-function");
    let artifact_root = root.join("artifacts");
    let config_path = root.join("inference.toml");
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: evidence-function
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - extension:core.workspace@^1.0
title: Evidence function
runtime:
  tool_mode: read-only
  max_output_tokens: 64
  max_capability_calls: 3
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply with evidence."
---
"#,
    )
    .unwrap();
    std::fs::write(
        function_root.join("SYSTEM.md"),
        "Write canonical evidence.\n",
    )
    .unwrap();
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
        paths: AgentLibrePaths::from_agl_home(root.join("home")),
        logging: agl_runtime::AgentLibreLoggingConfig::default(),
        history: agl_runtime::AgentLibreHistoryConfig::default(),
        workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
        inference: agl_runtime::AgentLibreInferenceConfig::default(),
        execution: agl_runtime::AgentLibreExecutionConfig::default(),
    };
    let session_id = session_id();
    let run_id = run_id();
    let turn_id = turn_id();
    let mut session = InferenceSession::new(
        InferenceOptions {
            config: Some(config_path),
            function_ref: Some("evidence-function".to_owned()),
            artifact_root: Some(artifact_root.clone()),
            workspace_root: Some(workspace),
            ..Default::default()
        },
        &runtime,
        None,
        session_id.clone(),
        crate::inference_client::test_inference_client(),
    )
    .unwrap();
    session
        .refresh_runtime_context(Some(&run_id), Some(&turn_id))
        .unwrap();
    let attempt_id = AttemptId::generate();
    session
        .write_runtime_resolution(&run_id, Some(&turn_id), Some(&attempt_id), None, None)
        .unwrap();

    let run_dir = InferenceArtifactRoot::new(artifact_root).run_dir(&run_id);
    let path = run_dir.join("runtime-resolution.json");
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["schema"], "agentlibre.runtime-resolution/v1");
    assert_eq!(record["run_id"], run_id.as_str());
    assert_eq!(record["session_id"], session_id.as_str());
    assert_eq!(record["turn_id"], turn_id.as_str());
    assert_eq!(record["attempt_id"], attempt_id.as_str());
    assert_eq!(
        record["artifacts"]["root"],
        "function:evidence-function@1.0.0"
    );
    assert_eq!(
        record["extension_bindings"]["core.workspace"]["implementation"],
        "builtin:agl-core-tools/fs"
    );
    assert_eq!(record["function_policy"]["max_capability_calls"], 3);
    assert_eq!(record["admission"]["fallback_allowed"], false);
    assert_eq!(record["admission"]["status"], "pre_effect_admitted");
    assert!(record["model_reuse_key"].as_str().is_some());
    assert!(record["context_reuse_key"].as_str().is_some());
    assert_eq!(record["client_runtime"], record["daemon_runtime"]);

    let details = ResourceAdmissionDetails {
        selected_profile_id: "gemma4-31b-64k-reviewed".to_owned(),
        context_tokens: 65_536,
        model_key: record["model_reuse_key"].as_str().unwrap().to_owned(),
        context_key: record["context_reuse_key"].as_str().unwrap().to_owned(),
        snapshot: agl_inference::admission::DeviceMemorySnapshot {
            physical_device_id: "drm-render-128".to_owned(),
            driver_id: "amdgpu-test".to_owned(),
            total_bytes: 25_752_453_120,
            available_bytes: 22_516_338_688,
            observed_at_unix_ms: 1_000,
        },
        estimate: agl_inference::admission::AllocationEstimate {
            model_bytes: 17_773_363_200,
            context_bytes: 4_037_017_600,
            transient_bytes: 335_544_320,
            uncertainty_bytes: 268_435_456,
        },
        required_bytes: 23_488_102_400,
        available_bytes: 22_516_338_688,
        reserved_bytes: 0,
        pressure_bytes: 3_236_114_432,
        reserve_bytes: 1_073_741_824,
        fallback_allowed: false,
        model_load_started: false,
        tool_effect_started: false,
    };
    let rejection = ModelManagerError::ResourceAdmission {
        code: "accelerator_capacity_exceeded".to_owned(),
        message: "reviewed 64K envelope does not fit".to_owned(),
        details: Some(Box::new(details.clone())),
    };
    session
        .write_runtime_resolution(
            &run_id,
            Some(&turn_id),
            Some(&attempt_id),
            Some(&rejection),
            None,
        )
        .unwrap();
    let rejected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(rejected["admission"]["status"], "rejected");
    assert_eq!(
        rejected["admission"]["error"]["code"],
        "accelerator_capacity_exceeded"
    );
    assert_eq!(
        rejected["admission"]["error"]["details"]["selected_profile_id"],
        details.selected_profile_id
    );
    assert_eq!(
        rejected["admission"]["error"]["details"]["estimate"]["context_bytes"],
        details.estimate.context_bytes
    );
    assert_eq!(
        rejected["admission"]["error"]["details"]["model_load_started"],
        false
    );
    session
        .write_runtime_resolution(
            &run_id,
            Some(&turn_id),
            Some(&attempt_id),
            None,
            Some(&details),
        )
        .unwrap();
    let granted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(granted["admission"]["status"], "granted");
    assert_eq!(granted["admission"]["model_load_started"], true);
    assert!(granted["admission"].get("error").is_none());
    assert_eq!(
        granted["admission"]["grant"]["selected_profile_id"],
        details.selected_profile_id
    );
    assert!(!run_dir.join("function-resolution.json").exists());
    assert!(!run_dir.join("runtime-identity.json").exists());
    assert!(!run_dir.join("runtime-resolution.json.tmp").exists());
    std::fs::remove_dir_all(root).unwrap();
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
                content: text("hello"),
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
                content: text("hello"),
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
    assert_eq!(
        request.rendered.messages[0].content,
        Some(text("demo system"))
    );
    assert_eq!(
        request.rendered.messages[1].role,
        agl_oven::RenderedMessageRole::User
    );
    assert_eq!(request.rendered.messages[1].content, Some(text("hello")));
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
                content: text("hello"),
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
    assert_eq!(request.rendered.messages[0].content, Some(text("system")));
    assert_eq!(
        request.rendered.messages[1].content,
        Some(text("skill context"))
    );
    assert_eq!(request.rendered.messages[2].content, Some(text("hello")));
}

#[test]
fn build_request_rejects_skill_routing_parity_failure_before_inference() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&["core.workspace:fs.read"]);
    let skill_id = SkillId::new("forged-routing").unwrap();
    let routing = SkillToolRoutingView::new([(
        skill_id,
        SkillToolRouting::new(
            [],
            [],
            [(
                ToolId::new("core.workspace:fs.read").unwrap(),
                agl_kernel::ToolExclusionReason::NotRouted,
            )],
        ),
    )])
    .unwrap();

    let error = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: text("hello"),
            }],
            visible_tools: visible_tools_from_effective(&effective),
        },
        AttemptId::generate(),
        &config,
        InferenceRequestContexts {
            skill_context: Some("<agentlibre_skill_context>\nforged\n</agentlibre_skill_context>"),
            skill_tool_routing: Some(&routing),
            effective_capabilities: Some(&effective),
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("callable routing differs from the effective capability set")
    );
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
                content: text("hello"),
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
    assert_eq!(request.rendered.messages[0].content, Some(text("system")));
    assert_eq!(
        request.rendered.messages[1].content,
        Some(text("memory context"))
    );
    assert_eq!(
        request.rendered.messages[2].content,
        Some(text("skill context"))
    );
    assert_eq!(request.rendered.messages[3].content, Some(text("hello")));
}

#[test]
fn build_request_injects_runtime_features_before_tools() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&[
        "core.workspace:fs.list",
        "core.workspace:fs.read",
        "core.workspace:fs.search",
    ]);
    let visible_tools = visible_tools_from_effective(&effective);
    let runtime_context = build_runtime_feature_context(
        std::path::Path::new("/repo"),
        ToolAccessMode::ReadOnly,
        &visible_tools,
    )
    .unwrap();

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: text("can you run cron jobs?"),
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

    assert_eq!(request.rendered.messages.len(), 5);
    assert_eq!(request.rendered.messages[0].content, Some(text("system")));
    assert!(
        request.rendered.messages[1]
            .content
            .contains("<agentlibre_runtime>")
    );
    assert!(
        request.rendered.messages[1]
            .content
            .contains("tool_mode: read-only")
    );
    assert!(
        request.rendered.messages[1]
            .content
            .contains("Only the tool schemas supplied for this turn are callable")
    );
    assert!(!request.rendered.messages[1].content.contains("cron"));
    assert!(!request.rendered.messages[1].content.contains("memory"));
    assert_eq!(
        request.rendered.messages[2].content,
        Some(text("memory context"))
    );
    assert_eq!(
        request.rendered.messages[3].content,
        Some(text("skill context"))
    );
    assert_eq!(
        request.rendered.messages[4].content,
        Some(text("can you run cron jobs?"))
    );
}

#[test]
fn build_request_keeps_hermes_tool_schema_out_of_system_messages() {
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let effective = effective_capabilities(&["core.workspace:fs.read"]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: text("read README"),
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

    assert_eq!(request.rendered.messages.len(), 3);
    assert_eq!(request.rendered.messages[0].content, Some(text("system")));
    assert_eq!(
        request.rendered.messages[1].content,
        Some(text("skill context"))
    );
    assert_eq!(
        request.rendered.messages[2].content,
        Some(text("read README"))
    );
    assert_eq!(request.rendered.tools[0].name, "core.workspace:fs.read");
}

#[test]
fn build_request_keeps_gemma_tool_schema_out_of_system_messages() {
    let config = ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    };
    let effective = effective_capabilities(&["core.workspace:fs.read"]);

    let request = build_inference_request(
        ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![TurnMessage::User {
                content: text("read README"),
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

    assert_eq!(request.rendered.messages.len(), 3);
    assert_eq!(
        request.rendered.messages[2].content,
        Some(text("read README"))
    );
    assert_eq!(request.rendered.tools[0].name, "core.workspace:fs.read");
}

#[test]
fn rendered_tool_keeps_the_exact_input_schema() {
    let effective = effective_capabilities(&["core.workspace:fs.read"]);
    let declaration = effective
        .capability(&ToolId::new("core.workspace:fs.read").unwrap())
        .unwrap()
        .declaration();
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    };
    let rendered = render_model_request(
        &ModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            request_index: 0,
            messages: vec![],
            visible_tools: visible_tools_from_effective(&effective),
        },
        &config,
    )
    .unwrap();

    assert_eq!(rendered.tools[0].input_schema, declaration.input_schema);
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
            agl_core_tools::FS_APPLY_PATCH_TOOL_ID,
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
        artifact_write_preflight_path_for_tool(
            agl_core_tools::FS_APPLY_PATCH_TOOL_ID,
            &[],
            &agl_args
        )
        .unwrap(),
        None
    );
    assert_eq!(
        artifact_write_preflight_path_for_tool(
            agl_core_tools::FS_APPLY_PATCH_TOOL_ID,
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
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
        vec!["core:repo_path.validate", "core:task_spec.validate"]
    );
    assert!(batches[0].optional_hooks.is_empty());
}

#[test]
fn selected_skill_visible_tools_use_declared_tool_metadata() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
        vec![
            "core.workspace:fs.apply_patch",
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
        ]
    );
    assert_eq!(
        tools[0].input_schema["required"],
        serde_json::json!(["operations"])
    );
    assert_eq!(tools[0].input_schema["additionalProperties"], false);
    assert!(tools[0].description.contains("Atomically"));
}

#[test]
fn no_skill_route_exposes_only_read_only_filesystem_tools() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
        ]
    );
}

#[test]
fn no_skill_route_adds_only_filesystem_edit_in_write_mode() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
            "core.workspace:fs.apply_patch",
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
        ]
    );
}

#[test]
fn foreground_process_and_shell_are_baseline_and_skill_routes_the_lifecycle_surface() {
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();

    let baseline = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &[],
        ToolAccessMode::Admin,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert!(baseline.contains(&ToolId::new(agl_core_tools::PROCESS_EXEC_TOOL_ID).unwrap()));
    assert!(baseline.contains(&ToolId::new(agl_core_tools::SHELL_EXEC_TOOL_ID).unwrap()));
    for id in agl_core_tools::PROCESS_TOOL_IDS {
        if !matches!(
            *id,
            agl_core_tools::PROCESS_EXEC_TOOL_ID | agl_core_tools::SHELL_EXEC_TOOL_ID
        ) {
            assert!(!baseline.contains(&ToolId::new((*id).to_string()).unwrap()));
        }
    }

    let process_skill = [SkillId::new("process").unwrap()];
    let read_only = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &process_skill,
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert_eq!(
        read_only
            .capabilities()
            .map(|capability| capability.declaration().id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "core.process:process.pwd",
            "core.process:process.read",
            "core.process:process.status",
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search",
        ]
    );

    let execute = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &process_skill,
        ToolAccessMode::Execute,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert_eq!(
        execute.capabilities().len(),
        agl_core_tools::PROCESS_TOOL_IDS.len() + 4
    );
    assert!(
        !execute
            .capability(&ToolId::new(agl_core_tools::SHELL_EXEC_TOOL_ID).unwrap())
            .unwrap()
            .authorized_state_effects()
            .contains(&EffectId::host_process_execution())
    );
}

#[test]
fn omitted_function_extension_does_not_activate_its_core_tools() {
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let selected_extensions = [ExtensionId::new(agl_core_tools::fs::PROVIDER_ID).unwrap()]
        .into_iter()
        .collect();
    let effective = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &[],
        ToolAccessMode::Admin,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary {
            authority_ceiling: None,
            delegation_enabled: false,
            selected_extensions,
        },
    )
    .unwrap();

    assert!(effective.contains(&ToolId::new(agl_core_tools::FS_READ_TOOL_ID).unwrap()));
    assert!(!effective.contains(&ToolId::new(agl_core_tools::PROCESS_EXEC_TOOL_ID).unwrap()));
    assert!(!effective.contains(&ToolId::new(agl_core_tools::SHELL_EXEC_TOOL_ID).unwrap()));
}

#[test]
fn function_policy_absence_empty_allow_and_deny_precedence_are_distinct() {
    let registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let fs_read = ToolId::new("core.workspace:fs.read").unwrap();

    let inherited = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
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
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert!(!empty_allow.contains(&fs_read));
    assert_eq!(
        empty_allow.exclusion(&fs_read).unwrap().reason,
        agl_kernel::ToolExclusionReason::FunctionAllowDenied
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
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert!(!denied.contains(&fs_read));
    assert_eq!(
        denied.exclusion(&fs_read).unwrap().reason,
        agl_kernel::ToolExclusionReason::FunctionDenied
    );
}

#[test]
fn delegation_is_visible_only_for_declared_children_with_parent_authority() {
    let registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let delegate = ToolId::new(agl_extension::AGENT_DELEGATE_TOOL_ID).unwrap();

    let disabled = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert!(!disabled.contains(&delegate));

    let enabled = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::Execute,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary {
            authority_ceiling: None,
            delegation_enabled: true,
            ..RuntimeCapabilityBoundary::default()
        },
    )
    .unwrap();
    assert!(enabled.contains(&delegate));

    let explicitly_empty = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::Execute,
        &RuntimePermissionGrantSnapshot::default(),
        Some(FunctionToolPolicy::default()),
        RuntimeCapabilityBoundary {
            authority_ceiling: None,
            delegation_enabled: true,
            ..RuntimeCapabilityBoundary::default()
        },
    )
    .unwrap();
    assert!(!explicitly_empty.contains(&delegate));
    assert_eq!(
        explicitly_empty.exclusion(&delegate).unwrap().reason,
        agl_kernel::ToolExclusionReason::FunctionAllowDenied
    );

    let ceiling = BTreeSet::new();
    let child_denied = resolve_effective_capabilities(
        &registry,
        &catalog,
        &[],
        ToolAccessMode::Execute,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary {
            authority_ceiling: Some(&ceiling),
            delegation_enabled: true,
            ..RuntimeCapabilityBoundary::default()
        },
    )
    .unwrap();
    assert!(!child_denied.contains(&delegate));
    assert_eq!(
        child_denied.exclusion(&delegate).unwrap().reason,
        agl_kernel::ToolExclusionReason::ParentAuthorityDenied
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
                "core.workspace:fs.list",
                "core.workspace:fs.read",
                "core.workspace:fs.search",
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
            tools_yaml: "tools:\n  allow:\n    - core.workspace:fs.list\n    - core.workspace:fs.read\n  deny:\n    - core.workspace:fs.list\n",
            expected_ids: &["core.workspace:fs.read"],
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
        inference: agl_runtime::AgentLibreInferenceConfig::default(),
        execution: agl_runtime::AgentLibreExecutionConfig::default(),
    };
    let catalog = full_tool_catalog();
    let catalog_ids = catalog
        .providers()
        .iter()
        .flat_map(|provider| provider.tools.iter())
        .map(|action| action.id.as_str())
        .collect::<BTreeSet<_>>();

    for case in cases {
        let function_root = workspace.join(".agl/functions").join(case.id);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join(agl_function::FUNCTION_FILE_NAME),
            format!(
                "---\nartifact:\n  schema: agentlibre.artifact/v1\n  type: function\n  id: {}\n  version: 1.0.0\n  payload_schema: agentlibre.function/v2\n  agl:\n    compatible: \">=1.0.0-alpha.12\"\n    tested: [1.0.0-alpha.12]\n  requires:\n    - extension:core.workspace@^1.0\ntitle: Function policy test\n{}---\n",
                case.id, case.tools_yaml
            ),
        )
        .unwrap();
        std::fs::write(
            function_root.join(agl_function::FUNCTION_SYSTEM_PROMPT_FILE_NAME),
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
            session_id(),
            crate::inference_client::test_inference_client(),
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
                    content: text("test policy"),
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
        assert!(tool_context.is_none(), "{} textual tool context", case.id);
        for capability_id in &catalog_ids {
            let marker = format!(r#""name":"{capability_id}""#);
            assert!(
                request
                    .rendered
                    .messages
                    .iter()
                    .all(|message| !message.content.contains(&marker)),
                "{} duplicated textual prompt capability {}",
                case.id,
                capability_id
            );
        }
    }

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn inference_session_rejects_function_package_digest_drift() {
    let root = temp_store_root("function-lock-drift");
    let workspace = root.join("workspace");
    let function_root = workspace.join(".agl/functions/locked");
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: locked
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Locked Function
---
"#,
    )
    .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Original prompt.\n").unwrap();
    let paths = agl_runtime::AgentLibrePaths::from_agl_home(root.join("home"));
    let reference: agl_artifact::ArtifactPackageRef = "function:locked@*".parse().unwrap();
    let composition = agl_runtime::compose_artifacts(&paths, &workspace).unwrap();
    let lock = composition
        .resolve_for_lock_refresh(&reference)
        .unwrap()
        .lock()
        .unwrap();
    lock.write_atomic(workspace.join(".agl/artifact-lock.toml"))
        .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Mutated prompt.\n").unwrap();

    let runtime = AgentLibreRuntimeConfig {
        paths,
        logging: agl_runtime::AgentLibreLoggingConfig::default(),
        history: agl_runtime::AgentLibreHistoryConfig::default(),
        workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
        inference: agl_runtime::AgentLibreInferenceConfig::default(),
        execution: agl_runtime::AgentLibreExecutionConfig::default(),
    };
    let error = match InferenceSession::new(
        InferenceOptions {
            config: Some(root.join("unused-config.toml")),
            function_ref: Some("locked".to_owned()),
            workspace_root: Some(workspace),
            ..Default::default()
        },
        &runtime,
        None,
        session_id(),
        crate::inference_client::test_inference_client(),
    ) {
        Ok(_) => panic!("mutated locked Function package must be rejected"),
        Err(error) => error,
    };
    assert_eq!(
        error
            .downcast_ref::<agl_artifact::ArtifactError>()
            .unwrap()
            .code(),
        "digest_drift"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dynamic_grant_cannot_exceed_the_run_tool_mode() {
    let registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let cron_add = ToolId::new("cron.add").unwrap();
    let snapshot = RuntimePermissionGrantSnapshot {
        admitted: vec![AdmittedPermissionGrant {
            grant_id: "grant-1".to_string(),
            capability_id: cron_add.clone(),
            max_operation_kind: OperationKind::Write,
            state_effects: BTreeSet::from([EffectId::store_cron()]),
            sensitive_inputs: BTreeSet::new(),
            run_id: run_id(),
            duration: "one_turn".to_string(),
            admitted_scope: "{}".to_string(),
            scope_digest: format!("sha256:{}", "0".repeat(64)),
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
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();

    assert!(!effective.contains(&cron_add));
    assert_eq!(
        effective.exclusion(&cron_add).unwrap().reason,
        agl_kernel::ToolExclusionReason::ToolModeDenied
    );
}

#[test]
fn no_skill_route_does_not_load_permission_tools_in_approve_mode() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
            "core.workspace:fs.apply_patch",
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
        ]
    );
}

#[test]
fn selected_skill_visible_tools_hide_write_tools_in_read_only_mode() {
    let skill_registry = test_skill_registry();
    let mut extension_registry = ToolCatalog::new();
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
        vec![
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
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
            sensitive_inputs: Vec::new(),
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
fn session_host_grant_retains_scope_provenance_and_remains_active() {
    let root = temp_store_root("grant-host-session");
    let store = AglStore::open_at(&root).unwrap();
    let session_id = session_id();
    let grant = store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: agl_core_tools::SHELL_EXEC_TOOL_ID.to_string(),
            max_operation_kind: "execute".to_string(),
            state_effects: vec![
                "spawn_process".to_string(),
                "host_process_execution".to_string(),
            ],
            sensitive_inputs: Vec::new(),
            scope: serde_json::json!({
                "workspace_root": "/repo",
                "session_id": session_id.clone(),
            }),
            duration: "session".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let selected = [SkillId::new("process").unwrap()];
    let run_id = run_id();
    let mut snapshot = admit_dynamic_permission_grants(
        &skill_registry,
        &catalog,
        &selected,
        &root,
        std::path::Path::new("/repo"),
        &run_id,
        Some(&session_id),
    )
    .unwrap();
    let effective = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Execute,
        &snapshot,
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    finalize_permission_grants(&root, &run_id, &effective, &mut snapshot).unwrap();

    let capability = effective
        .capability(&ToolId::new(agl_core_tools::SHELL_EXEC_TOOL_ID).unwrap())
        .unwrap();
    assert!(
        capability
            .authorized_state_effects()
            .contains(&EffectId::host_process_execution())
    );
    let provenance = capability.grant_provenance().unwrap();
    assert_eq!(provenance.grant_id, grant.id);
    assert_eq!(provenance.duration, "session");
    assert!(provenance.admitted_scope.contains(TEST_SESSION_ID));
    assert!(provenance.scope_digest.starts_with("sha256:"));

    let retained = store.permission_grant(&grant.id).unwrap().unwrap();
    assert_eq!(retained.status, agl_store::PermissionGrantStatus::Active);
    assert_eq!(retained.last_admitted_run_id.as_deref(), Some(TEST_RUN_ID));
    assert!(retained.consumed_at.is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn screen_grant_requires_sensitive_input_and_host_effect_together() {
    let root = temp_store_root("grant-screen-exact");
    let store = AglStore::open_at(&root).unwrap();
    let missing_effect = store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: agl_host_tools::SCREEN_CAPTURE_TOOL_ID.to_string(),
            max_operation_kind: "read".to_string(),
            state_effects: Vec::new(),
            sensitive_inputs: vec!["screen_capture".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let exact = store
        .create_permission_grant(agl_store::PermissionGrantDraft {
            request_id: None,
            tool_id: agl_host_tools::SCREEN_CAPTURE_TOOL_ID.to_string(),
            max_operation_kind: "read".to_string(),
            state_effects: vec!["host_screen_capture".to_string()],
            sensitive_inputs: vec!["screen_capture".to_string()],
            scope: serde_json::json!({}),
            duration: "one_turn".to_string(),
            granted_by_ref: "test".to_string(),
        })
        .unwrap();
    let mut catalog = full_tool_catalog();
    catalog
        .register(agl_host_tools::screen::declaration())
        .unwrap();
    let policy = SelectedSkillGrantPolicy::default();

    assert_eq!(
        evaluate_permission_grant(
            &missing_effect,
            &catalog,
            &policy,
            std::path::Path::new("/repo"),
            &run_id(),
            None,
        )
        .unwrap_err(),
        "state_effect_denied"
    );
    let admitted = evaluate_permission_grant(
        &exact,
        &catalog,
        &policy,
        std::path::Path::new("/repo"),
        &run_id(),
        None,
    )
    .unwrap();
    assert_eq!(
        admitted.state_effects,
        BTreeSet::from([EffectId::host_screen_capture()])
    );
    assert_eq!(
        admitted.sensitive_inputs,
        BTreeSet::from([SensitiveInput::ScreenCapture])
    );

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
            sensitive_inputs: Vec::new(),
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
            sensitive_inputs: Vec::new(),
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
            sensitive_inputs: Vec::new(),
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
        vec![
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
        ]
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
fn selected_cron_planner_routes_grant_fixable_tools_as_requestable() {
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let selected = [SkillId::new("cron-planner").unwrap()];
    let effective = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Write,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing = derive_skill_tool_routing(&skill_registry, &selected, &effective).unwrap();
    let route = routing.route(&selected[0]).unwrap();

    assert_eq!(
        route
            .callable_tools()
            .iter()
            .map(ToolId::as_str)
            .collect::<Vec<_>>(),
        vec![
            "core.workspace:fs.read",
            "core.workspace:fs.search",
            "cron.preflight",
            "permissions.request",
            "permissions.status",
        ]
    );
    assert_eq!(
        route
            .requestable_tools()
            .iter()
            .map(ToolId::as_str)
            .collect::<Vec<_>>(),
        vec!["cron.add", "matrix.outbox.enqueue"]
    );
    assert_eq!(
        route
            .unavailable_tools()
            .iter()
            .map(|(tool, reason)| (tool.as_str(), reason.code()))
            .collect::<Vec<_>>(),
        vec![("matrix.outbox.deliver", "unknown_capability")]
    );
    ensure_skill_tool_routing_parity(&routing, &effective).unwrap();

    let bundle =
        build_verified_context_bundle(&skill_registry, &catalog, &selected, &routing).unwrap();
    assert!(bundle.content.contains(
        "directly_callable_tools: core.workspace:fs.read, core.workspace:fs.search, cron.preflight, permissions.request, permissions.status"
    ));
    assert!(
        bundle
            .content
            .contains("requestable_tools: cron.add, matrix.outbox.enqueue")
    );
    assert!(bundle.content.contains("id: schedule-matrix-cron"));
}

#[test]
fn operation_mode_and_function_policy_make_skill_tools_unavailable_not_requestable() {
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let selected = [SkillId::new("cron-planner").unwrap()];

    let read_only = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing = derive_skill_tool_routing(&skill_registry, &selected, &read_only).unwrap();
    let route = routing.route(&selected[0]).unwrap();
    assert!(route.requestable_tools().is_empty());
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("cron.add").unwrap()],
        agl_kernel::ToolExclusionReason::ToolModeDenied
    );
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("matrix.outbox.enqueue").unwrap()],
        agl_kernel::ToolExclusionReason::ToolModeDenied
    );
    let bundle =
        build_verified_context_bundle(&skill_registry, &catalog, &selected, &routing).unwrap();
    assert!(bundle.content.contains("requestable_tools: []"));
    assert!(!bundle.content.contains("id: schedule-matrix-cron"));

    let function_denied = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Write,
        &RuntimePermissionGrantSnapshot::default(),
        Some(FunctionToolPolicy::default()),
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing = derive_skill_tool_routing(&skill_registry, &selected, &function_denied).unwrap();
    let route = routing.route(&selected[0]).unwrap();
    assert!(route.callable_tools().is_empty());
    assert!(route.requestable_tools().is_empty());
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("cron.add").unwrap()],
        agl_kernel::ToolExclusionReason::FunctionAllowDenied
    );
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("permissions.request").unwrap()],
        agl_kernel::ToolExclusionReason::FunctionAllowDenied
    );
    ensure_skill_tool_routing_parity(&routing, &function_denied).unwrap();

    let request_path_denied = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Write,
        &RuntimePermissionGrantSnapshot::default(),
        Some(FunctionToolPolicy::new(
            tool_ids(&[
                "cron.add",
                "cron.preflight",
                "core.workspace:fs.read",
                "core.workspace:fs.search",
                "matrix.outbox.enqueue",
                "permissions.status",
            ]),
            [],
        )),
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing =
        derive_skill_tool_routing(&skill_registry, &selected, &request_path_denied).unwrap();
    let route = routing.route(&selected[0]).unwrap();
    assert!(route.requestable_tools().is_empty());
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("cron.add").unwrap()],
        agl_kernel::ToolExclusionReason::NotRouted
    );
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("permissions.request").unwrap()],
        agl_kernel::ToolExclusionReason::FunctionAllowDenied
    );

    let explicitly_denied = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Write,
        &RuntimePermissionGrantSnapshot::default(),
        Some(FunctionToolPolicy::new(
            tool_ids(&[
                "cron.add",
                "cron.preflight",
                "core.workspace:fs.read",
                "core.workspace:fs.search",
                "matrix.outbox.deliver",
                "matrix.outbox.enqueue",
                "permissions.request",
                "permissions.status",
            ]),
            tool_ids(&["core.workspace:fs.read"]),
        )),
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing =
        derive_skill_tool_routing(&skill_registry, &selected, &explicitly_denied).unwrap();
    let route = routing.route(&selected[0]).unwrap();
    assert!(
        !route
            .callable_tools()
            .contains(&ToolId::new("core.workspace:fs.read").unwrap())
    );
    assert_eq!(
        route.unavailable_tools()[&ToolId::new("core.workspace:fs.read").unwrap()],
        agl_kernel::ToolExclusionReason::FunctionDenied
    );
    ensure_skill_tool_routing_parity(&routing, &explicitly_denied).unwrap();
}

#[test]
fn invalid_grant_remains_requestable_when_a_correct_grant_can_fix_it() {
    let skill_registry = test_skill_registry();
    let catalog = full_tool_catalog();
    let selected = [SkillId::new("cron-planner").unwrap()];
    let cron_add = ToolId::new("cron.add").unwrap();
    let invalid_grant = RuntimePermissionGrantSnapshot {
        admitted: vec![AdmittedPermissionGrant {
            grant_id: "grant-invalid".to_string(),
            capability_id: cron_add.clone(),
            max_operation_kind: OperationKind::Read,
            state_effects: BTreeSet::new(),
            sensitive_inputs: BTreeSet::new(),
            run_id: run_id(),
            duration: "one_turn".to_string(),
            admitted_scope: "{}".to_string(),
            scope_digest: format!("sha256:{}", "0".repeat(64)),
        }],
        ignored: Vec::new(),
    };
    let effective = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Write,
        &invalid_grant,
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    assert_eq!(
        effective.exclusion(&cron_add).unwrap().reason,
        agl_kernel::ToolExclusionReason::GrantOperationDenied
    );

    let routing = derive_skill_tool_routing(&skill_registry, &selected, &effective).unwrap();
    let route = routing.route(&selected[0]).unwrap();
    assert!(route.requestable_tools().contains(&cron_add));
    ensure_skill_tool_routing_parity(&routing, &effective).unwrap();
}

#[test]
fn untrusted_provider_tools_are_unavailable_and_skill_instructions_remain() {
    let skill_registry = test_skill_registry();
    let mut catalog = ToolCatalog::new();
    catalog
        .register(agl_extension::delegation_provider())
        .unwrap();
    agl_core_tools::guards::register(&mut catalog).unwrap();
    catalog
        .register(
            agl_core_tools::cron::declaration().with_trust(agl_extension::ExtensionTrust::Revoked),
        )
        .unwrap();
    agl_core_tools::fs::register(&mut catalog).unwrap();
    agl_core_tools::matrix::register(&mut catalog).unwrap();
    agl_core_tools::memory::register(&mut catalog).unwrap();
    agl_core_tools::notes::register(&mut catalog).unwrap();
    agl_core_tools::permissions::register(&mut catalog).unwrap();
    agl_core_tools::process::register(&mut catalog).unwrap();
    agl_core_tools::repo::register(&mut catalog).unwrap();
    agl_core_tools::skills::register(&mut catalog).unwrap();
    agl_core_tools::store::register(&mut catalog).unwrap();
    let selected = [SkillId::new("cron-planner").unwrap()];
    let effective = resolve_effective_capabilities(
        &skill_registry,
        &catalog,
        &selected,
        ToolAccessMode::Write,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing = derive_skill_tool_routing(&skill_registry, &selected, &effective).unwrap();
    let route = routing.route(&selected[0]).unwrap();
    for capability in ["cron.add", "cron.preflight"] {
        assert_eq!(
            route.unavailable_tools()[&ToolId::new(capability).unwrap()],
            agl_kernel::ToolExclusionReason::ProviderUntrusted
        );
    }
    assert!(
        !route
            .requestable_tools()
            .contains(&ToolId::new("cron.add").unwrap())
    );
    ensure_skill_tool_routing_parity(&routing, &effective).unwrap();

    let bundle =
        build_verified_context_bundle(&skill_registry, &catalog, &selected, &routing).unwrap();
    assert!(
        bundle
            .content
            .contains("Use this test-only cron-planner skill.")
    );
    assert!(bundle.content.contains("cron.add [provider_untrusted]"));
}

#[test]
fn unknown_skill_tool_is_unavailable_without_hiding_skill_instructions() {
    let skill_id = SkillId::new("unknown-tool").unwrap();
    let mut registry = test_skill_registry();
    registry
        .register(agl_skill::RegisteredSkill::trusted_builtin(test_skill(
            skill_id.as_str(),
            &["core:repo_path.validate"],
            &["unknown.capability"],
            &[],
            &[],
            Vec::new(),
        )))
        .unwrap();
    let mut catalog = ToolCatalog::new();
    agl_core_tools::guards::register(&mut catalog).unwrap();
    let selected = [skill_id.clone()];
    let effective = resolve_effective_capabilities(
        &registry,
        &catalog,
        &selected,
        ToolAccessMode::ReadOnly,
        &RuntimePermissionGrantSnapshot::default(),
        None,
        RuntimeCapabilityBoundary::default(),
    )
    .unwrap();
    let routing = derive_skill_tool_routing(&registry, &selected, &effective).unwrap();
    assert_eq!(
        routing.route(&skill_id).unwrap().unavailable_tools()
            [&ToolId::new("unknown.capability").unwrap()],
        agl_kernel::ToolExclusionReason::UnknownCapability
    );

    let bundle = build_verified_context_bundle(&registry, &catalog, &selected, &routing).unwrap();
    assert!(
        bundle
            .content
            .contains("Use this test-only unknown-tool skill.")
    );
    assert!(
        bundle
            .content
            .contains("unknown.capability [unknown_capability]")
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
            sensitive_inputs: Vec::new(),
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
    catalog
        .register(agl_extension::delegation_provider())
        .unwrap();
    agl_core_tools::guards::register(&mut catalog).unwrap();
    agl_core_tools::cron::register(&mut catalog).unwrap();
    agl_core_tools::fs::register(&mut catalog).unwrap();
    agl_core_tools::matrix::register(&mut catalog).unwrap();
    agl_core_tools::memory::register(&mut catalog).unwrap();
    agl_core_tools::notes::register(&mut catalog).unwrap();
    agl_core_tools::permissions::register(&mut catalog).unwrap();
    agl_core_tools::process::register(&mut catalog).unwrap();
    agl_core_tools::repo::register(&mut catalog).unwrap();
    agl_core_tools::skills::register(&mut catalog).unwrap();
    agl_core_tools::store::register(&mut catalog).unwrap();
    catalog
}

fn test_skill_registry() -> agl_skill::SkillRegistry {
    let mut registry = agl_skill::builtin_registry().unwrap();
    for skill in [
        test_skill(
            "task-spec",
            &["core:repo_path.validate", "core:task_spec.validate"],
            &[
                "core.workspace:fs.apply_patch",
                "core.workspace:fs.list",
                "core.workspace:fs.read",
                "core.workspace:fs.search",
            ],
            &[],
            &[],
            Vec::new(),
        ),
        test_skill(
            "tool-smoke",
            &["core:repo_path.validate"],
            &["core.workspace:fs.read"],
            &[],
            &[],
            Vec::new(),
        ),
        test_skill(
            "notes-capture",
            &["core:repo_path.validate"],
            &["notes.add", "notes.link"],
            &[],
            &["notes.delete"],
            Vec::new(),
        ),
        test_skill(
            "cron-planner",
            &["core:repo_path.validate"],
            &[
                "cron.preflight",
                "core.workspace:fs.read",
                "core.workspace:fs.search",
                "permissions.request",
                "permissions.status",
            ],
            &["cron.add", "matrix.outbox.enqueue"],
            &["matrix.outbox.deliver"],
            vec![agl_skill::SkillPermissionRequestTemplate {
                id: "schedule-matrix-cron".to_string(),
                tools: tool_ids(&["cron.add", "matrix.outbox.enqueue"]),
                max_operation_kind: Some(OperationKind::Write),
                state_effects: vec![EffectId::store_cron(), EffectId::matrix_outbox()],
                default_duration: "one_turn".to_string(),
                reason_template: "Schedule a Matrix notification cron job.".to_string(),
            }],
        ),
    ] {
        registry
            .register(agl_skill::RegisteredSkill::trusted_builtin(skill))
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
    permission_request_templates: Vec<agl_skill::SkillPermissionRequestTemplate>,
) -> agl_skill::SkillHarness {
    agl_skill::SkillHarness {
        artifact: test_skill_artifact(id),
        id: SkillId::new(id).unwrap(),
        name: id.to_string(),
        description: format!("Test-only {id} skill."),
        version: agl_artifact::ArtifactVersion::new("1.0.0").unwrap(),
        source: agl_skill::SkillSource::Core,
        pack: "test".to_string(),
        required_hooks: hook_ids(required_hooks),
        allowed_tools: tool_ids(allowed_tools),
        requestable_tools: tool_ids(requestable_tools),
        denied_tools: tool_ids(denied_tools),
        permission_request_templates,
        permissions: agl_skill::SkillPermissions::default(),
        context_budget_tokens: 512,
        reference_policy: agl_skill::SkillReferencePolicy {
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

fn test_skill_artifact(id: &str) -> agl_artifact::ArtifactEnvelope {
    agl_artifact::ArtifactEnvelope::new(
        agl_artifact::ArtifactTypeId::skill(),
        agl_artifact::ArtifactPackageId::new(id).unwrap(),
        agl_artifact::ArtifactVersion::new("1.0.0").unwrap(),
        agl_artifact::ArtifactSchemaId::new("agentlibre.skill/v2").unwrap(),
        agl_artifact::AglCompatibility::new(
            agl_artifact::ArtifactVersionReq::new(">=1.0.0-alpha.12").unwrap(),
            [agl_artifact::ArtifactVersion::new("1.0.0-alpha.12").unwrap()],
        )
        .unwrap(),
        Vec::new(),
    )
    .unwrap()
}

fn hook_ids(values: &[&str]) -> Vec<HookId> {
    values
        .iter()
        .map(|value| HookId::new(*value).unwrap())
        .collect()
}

fn tool_ids(values: &[&str]) -> Vec<ToolId> {
    values
        .iter()
        .map(|value| ToolId::new(*value).unwrap())
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
    agl_core_tools::guards::register(&mut extension_registry).unwrap();
    agl_core_tools::fs::register(&mut extension_registry).unwrap();
    agl_core_tools::permissions::register(&mut extension_registry).unwrap();
    agl_core_tools::skills::register(&mut extension_registry).unwrap();

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
        vec![
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search"
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
        inference: agl_runtime::AgentLibreInferenceConfig::default(),
        execution: agl_runtime::AgentLibreExecutionConfig::default(),
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
fn embedded_function_profile_rejects_external_config_override() {
    let root = temp_store_root("embedded-function-config-override");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let runtime = AgentLibreRuntimeConfig {
        paths: agl_runtime::AgentLibrePaths::from_agl_home(root.join("home")),
        logging: agl_runtime::AgentLibreLoggingConfig::default(),
        history: agl_runtime::AgentLibreHistoryConfig::default(),
        workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
        inference: agl_runtime::AgentLibreInferenceConfig::default(),
        execution: agl_runtime::AgentLibreExecutionConfig::default(),
    };

    let error = InferenceSession::new(
        InferenceOptions {
            config: Some(root.join("override.toml")),
            function_ref: Some("gemma4-31b-32k".to_owned()),
            artifact_root: Some(root.join("artifacts")),
            workspace_root: Some(workspace),
            ..Default::default()
        },
        &runtime,
        None,
        session_id(),
        crate::inference_client::test_inference_client(),
    )
    .err()
    .expect("external config must not replace an embedded Function profile");

    let rendered = format!("{error:#}");
    assert!(rendered.contains("function:gemma4-31b-32k@1.0.0"));
    assert!(rendered.contains("owns an embedded inference profile"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn agent_event_stream_uses_canonical_run_event_path() {
    let run_id = run_id();

    assert_eq!(
        agent_event_stream_path(std::path::Path::new("/tmp/artifacts"), &run_id),
        PathBuf::from(format!("/tmp/artifacts/runs/{TEST_RUN_ID}/events.jsonl"))
    );
}
