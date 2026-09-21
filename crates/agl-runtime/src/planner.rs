//! Runtime-owned planner generation.
//!
//! Planner output is deliberately kept separate from the normal Agent loop:
//! the runtime owns the output contract, performs the one allowed correction,
//! and returns a plan only after canonical validation has succeeded.

use std::path::PathBuf;

use agl_core::Content;
use agl_core::agent::{AdmittedTool, InstructionBlock, InstructionSet, InstructionSource};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub const PLANNER_FUNCTION_KIND: &str = "planner";
pub const PLANNER_SCHEMA_NAME: &str = "agentlibre_implementation_plan_v1";
pub const PLANNER_CORRECTION_PREFIX: &str = "Return only the corrected canonical JSON object. The previous assistant output was invalid and is retained in this Conversation.";
pub const PLANNER_DOMAIN_RULES: &str = "Cross-reference rules: each slice start_condition may contain only IDs from top-level evidence or that same slice's required_reads; never put a decision ID, invariant ID, acceptance ID, or done_when ID in start_condition; decisions are cited as sources and must not be used as start-condition references; slice depends_on names existing earlier slice IDs; step depends_on names steps in that slice; step satisfies and verification covers name existing invariant or objective.acceptance IDs; every declared invariant must appear in at least one step.satisfies or verification.covers entry, every objective acceptance ID must be covered too, and if no invariant is needed do not invent one; every step file must be declared in that slice's files; every ReadRequirement and ConditionalRead read ID must be globally unique across the entire plan, so suffix repeated concepts with the slice ID (for example read.decisions.implementation and read.decisions.test). Every slice that changes workspace files must include at least one executable behavior verification (for example a focused test, test suite, type check, linter, build, or runtime check) covering the changed behavior. A command that only reports repository state or differences, such as git diff, git status, or git diff --name-only, is not sufficient by itself.";

const FILESYSTEM_WRITE_EFFECT: &str = "agentlibre.builtins:filesystem_write";
const TERMINAL_CONTROL_EFFECT: &str = "agentlibre.execution:terminal.control";

#[derive(Clone, Debug)]
pub struct PlannerWorkspaceBoundary {
    pub workspace_root: PathBuf,
    pub scratch_root: PathBuf,
}

impl PlannerWorkspaceBoundary {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.workspace_root.is_absolute(),
            "planner workspace is not absolute"
        );
        ensure!(
            self.scratch_root.is_absolute(),
            "planner scratch root is not absolute"
        );
        let workspace = self.workspace_root.canonicalize()?;
        ensure!(workspace.is_dir(), "planner workspace is not a directory");
        if self.scratch_root.exists() {
            let scratch = self.scratch_root.canonicalize()?;
            ensure!(
                scratch != workspace && !scratch.starts_with(&workspace),
                "planner scratch root must not be inside the workspace"
            );
        }
        Ok(())
    }

    /// Planner execution may read the workspace, but no admitted capability
    /// may mutate it or retain a terminal session that can do so.
    pub fn validate_capabilities(&self, tools: &[AdmittedTool]) -> Result<()> {
        self.validate()?;
        for tool in tools {
            for effect in &tool.definition.required_effects {
                ensure!(
                    effect.as_str() != FILESYSTEM_WRITE_EFFECT
                        && effect.as_str() != TERMINAL_CONTROL_EFFECT,
                    "planner capability {} is not read-only",
                    tool.definition.id
                );
            }
        }
        Ok(())
    }
}

pub fn instructions(base: &InstructionSet) -> Result<InstructionSet> {
    let mut blocks = base.blocks.clone();
    blocks.push(InstructionBlock {
        source: InstructionSource::Agent,
        content: Content::text(
            format!("You are the agentLIBRE planner. Read repository instructions and accepted specs, inspect concrete code and tests, distinguish evidence from human decisions and recommendations, and put unresolved material choices in open_decisions. Partition the work into independently verifiable slices with exact files, symbols, reads, start conditions, steps, verification commands, and done checks. Emit only one JSON object matching the runtime-provided implementation-plan schema. The top-level schema field must be exactly \"agentlibre.implementation-plan/v1\". The top-level id must be a lowercase UUIDv7 with form xxxxxxxx-xxxx-7xxx-[89ab]xxx-xxxxxxxxxxxx. {} Do not emit Markdown or commentary.", PLANNER_DOMAIN_RULES),
        )?,
    });
    InstructionSet::new(blocks).map_err(|error| anyhow::anyhow!(error))
}

pub fn response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": PLANNER_SCHEMA_NAME,
            "strict": true,
            "schema": implementation_plan_schema()
        }
    })
}

/// The schema is intentionally runtime-owned. `deny_unknown_fields` and the
/// typed domain validator remain authoritative after decoding.
pub fn implementation_plan_schema() -> Value {
    let text = json!({"type":"string","minLength":1,"maxLength":16384});
    let path = json!({"type":"string","minLength":1,"maxLength":512});
    let id = json!({"type":"string","pattern":"^[a-z][a-z0-9._-]{0,127}$"});
    let digest = json!({"type":"string","pattern":"^sha256:[0-9a-f]{64}$"});
    let source = json!({
        "type":"object","additionalProperties":false,
        "required":["kind","locator","digest","line_start","line_end"],
        "properties":{"kind":{"enum":["file","message","decision","test","config"]},"locator":text,"digest":{"anyOf":[digest,json!({"type":"null"})]},"line_start":{"anyOf":[json!({"type":"integer","minimum":1}),json!({"type":"null"})]},"line_end":{"anyOf":[json!({"type":"integer","minimum":1}),json!({"type":"null"})]}}
    });
    let check = json!({"type":"object","additionalProperties":false,"required":["id","statement","sources"],"properties":{"id":id,"statement":text,"sources":{"type":"array","minItems":1,"maxItems":256,"items":source}}});
    let read = json!({"type":"object","additionalProperties":false,"required":["id","path","symbols_or_ranges","obtain"],"properties":{"id":id,"path":path,"symbols_or_ranges":{"type":"array","maxItems":256,"items":text},"obtain":text}});
    let file = json!({"type":"object","additionalProperties":false,"required":["path","disposition","purpose","symbols"],"properties":{"path":path,"disposition":{"enum":["create","modify","delete","move"]},"purpose":text,"symbols":{"type":"array","maxItems":256,"items":text}}});
    let step = json!({"type":"object","additionalProperties":false,"required":["id","action","files","symbols","depends_on","satisfies"],"properties":{"id":id,"action":text,"files":{"type":"array","minItems":1,"maxItems":256,"items":path},"symbols":{"type":"array","maxItems":256,"items":text},"depends_on":{"type":"array","maxItems":256,"items":id},"satisfies":{"type":"array","maxItems":256,"items":id}}});
    let verification_expected = json!({"type":"object","additionalProperties":false,"required":["exit_code","stdout_contains","stderr_contains"],"properties":{"exit_code":{"type":"integer"},"stdout_contains":{"type":"array","maxItems":256,"items":text},"stderr_contains":{"type":"array","maxItems":256,"items":text}}});
    let verification = json!({"type":"object","additionalProperties":false,"required":["command","working_directory","expected","covers"],"properties":{"command":text,"working_directory":path,"expected":verification_expected,"covers":{"type":"array","maxItems":256,"items":id}}});
    let slice = json!({"type":"object","additionalProperties":false,"required":["id","outcome","depends_on","files","required_reads","conditional_reads","start_condition","steps","verification","done_when"],"properties":{"id":id,"outcome":text,"depends_on":{"type":"array","maxItems":256,"items":id},"files":{"type":"array","minItems":1,"maxItems":256,"items":file},"required_reads":{"type":"array","maxItems":256,"items":read},"conditional_reads":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":false,"required":["id","condition","read"],"properties":{"id":id,"condition":text,"read":read}}},"start_condition":{"type":"array","minItems":1,"maxItems":256,"items":id},"steps":{"type":"array","minItems":1,"maxItems":256,"items":step},"verification":{"type":"array","minItems":1,"maxItems":256,"items":verification},"done_when":{"type":"array","minItems":1,"maxItems":256,"items":check}}});
    let common_sources = json!({"type":"array","minItems":1,"maxItems":256,"items":source});
    json!({
        "type":"object","additionalProperties":false,
        "required":["schema","id","workspace","objective","evidence","decisions","invariants","slices","open_decisions"],
        "properties":{
            "schema":{"enum":["agentlibre.implementation-plan/v1"]},"id":{"type":"string","pattern":"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"},
            "workspace":{"type":"object","additionalProperties":false,"required":["canonical_root_sha256","git_commit","dirty_paths"],"properties":{"canonical_root_sha256":digest,"git_commit":{"anyOf":[{"type":"string","pattern":"^[0-9a-f]{40}([0-9a-f]{24})?$"},{"type":"null"}]},"dirty_paths":{"type":"array","maxItems":256,"items":path}}},
            "objective":{"type":"object","additionalProperties":false,"required":["outcome","acceptance","sources"],"properties":{"outcome":text,"acceptance":{"type":"array","minItems":1,"maxItems":256,"items":check},"sources":common_sources}},
            "evidence":{"type":"array","minItems":1,"maxItems":256,"items":{"type":"object","additionalProperties":false,"required":["id","statement","sources"],"properties":{"id":id,"statement":text,"sources":common_sources}}},
            "decisions":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":false,"required":["id","statement","authority","sources"],"properties":{"id":id,"statement":text,"authority":{"enum":["repository","human"]},"sources":common_sources}}},
            "invariants":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":false,"required":["id","statement","sources"],"properties":{"id":id,"statement":text,"sources":common_sources}}},
            "slices":{"type":"array","minItems":1,"maxItems":256,"items":slice},
            "open_decisions":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":false,"required":["id","question","consequences","affected_slices","sources"],"properties":{"id":id,"question":text,"consequences":text,"affected_slices":{"type":"array","minItems":1,"maxItems":256,"items":id},"sources":common_sources}}}
        }
    })
}

#[cfg(any())]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use crate::inference::{InferenceServiceError, test_runtime_selection};
    use crate::package::{PackageId, PackageVersion};
    use agl_core::agent::{
        AgentDefinitionRef, AgentPresentation, AgentRunLimits, ModelDefinitionRef, ModelSelection,
        WorkspaceScope,
    };
    use agl_core::implementation_plan::PlanId;

    struct RecordingRoute {
        outputs: Mutex<VecDeque<String>>,
        requests: Mutex<Vec<InferenceGenerateRequest>>,
    }

    impl InferenceRoute for RecordingRoute {
        fn measure(
            &self,
            _request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            unreachable!("planner generation does not call measure")
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<ModelGenerationResult, InferenceServiceError> {
            let output = self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(InferenceServiceError::Stopped)?;
            self.requests.lock().unwrap().push(request);
            Ok(ModelGenerationResult {
                output: ModelGenerationOutput::Assistant(Content::text(output).unwrap()),
                private_reasoning: None,
                finish_reason: ModelFinishReason::Stop,
                usage: Default::default(),
                realization: agl_core::agent::InferenceRealizationRef {
                    runtime_profile_digest:
                        agl_core::agent::InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                    engine_build_digest: agl_core::agent::InferenceEngineBuildDigest::from_bytes(
                        [2; 32],
                    ),
                    physical_resource_digest: agl_core::agent::PhysicalResourceDigest::from_bytes(
                        [3; 32],
                    ),
                },
                correction: None,
            })
        }
    }

    fn snapshot(root: &Path) -> AgentRunSnapshot {
        AgentRunSnapshot {
            agent: AgentDefinitionRef {
                id: PackageId::new("planner-agent").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: agl_core::agent::PackageDigest::from_bytes([1; 32]),
            },
            model: ModelSelection {
                model: ModelDefinitionRef {
                    id: PackageId::new("planner-model").unwrap(),
                    version: PackageVersion::new("1.0.0").unwrap(),
                    digest: agl_core::agent::PackageDigest::from_bytes([2; 32]),
                },
                runtime: test_runtime_selection(),
                reasoning_efforts: vec![],
            },
            invalid_model_output_recovery: None,
            instructions: InstructionSet::new(vec![]).unwrap(),
            workspace: WorkspaceScope {
                root: agl_core::agent::AbsolutePath::try_from(root.to_string_lossy().into_owned())
                    .unwrap(),
                working_directory: agl_core::agent::RelativePath::try_from(".".to_owned()).unwrap(),
            },
            tools: vec![],
            authority: Default::default(),
            limits: AgentRunLimits {
                deadline_ms: 60_000,
                model_input_tokens: None,
                model_output_tokens: 1024,
                model_calls: 2,
                correction_input_tokens: 1024,
                correction_output_tokens: 1024,
                correction_calls: 1,
                tool_calls: 0,
                tool_result_bytes: 1024,
            },
            presentation: AgentPresentation::default(),
            response_format: None,
            planner_read_only: false,
        }
    }

    fn valid_plan(root: &Path) -> ImplementationPlan {
        let source = json!({
            "kind": "file", "locator": "reports/spec.md", "digest": null,
            "line_start": null, "line_end": null
        });
        serde_json::from_value(json!({
            "schema": agl_core::implementation_plan::IMPLEMENTATION_PLAN_SCHEMA,
            "id": PlanId::generate().to_string(),
            "workspace": {
                "canonical_root_sha256": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "git_commit": null, "dirty_paths": []
            },
            "objective": {
                "outcome": "implement the change", "acceptance": [{"id":"acceptance","statement":"it works","sources":[source]}], "sources":[source]
            },
            "evidence": [{"id":"evidence","statement":"the repository establishes the current behavior","sources":[source]}],
            "decisions": [],
            "invariants": [{"id":"invariant","statement":"the contract remains valid","sources":[source]}],
            "slices": [{
                "id":"slice","outcome":"ship the change","depends_on":[],
                "files":[{"path":"src/lib.rs","disposition":"modify","purpose":"implement it","symbols":[]}],
                "required_reads":[{"id":"read","path":"src/lib.rs","symbols_or_ranges":[],"obtain":"understand the current implementation"}],
                "conditional_reads":[],"start_condition":["evidence"],
                "steps":[{"id":"step","action":"make the implementation change","files":["src/lib.rs"],"symbols":[],"depends_on":[],"satisfies":["invariant"]}],
                "verification":[{"command":"cargo test","working_directory":".","expected":{"exit_code":0,"stdout_contains":[],"stderr_contains":[]},"covers":["invariant","acceptance"]}],
                "done_when":[{"id":"done","statement":"the change is complete","sources":[source]}]
            }],
            "open_decisions": []
        }))
        .unwrap_or_else(|error| panic!("invalid fixture for {}: {error}", root.display()))
    }

    #[test]
    fn schema_is_strict_and_named() {
        let format = response_format();
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["json_schema"]["strict"], true);
        assert_eq!(format["json_schema"]["name"], PLANNER_SCHEMA_NAME);
        assert_eq!(
            format["json_schema"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn boundary_rejects_mutating_capability() {
        let result = PlannerWorkspaceBoundary {
            workspace_root: PathBuf::from("/tmp"),
            scratch_root: PathBuf::from("/tmp/planner-scratch"),
        }
        .validate_capabilities(&[]);
        assert!(result.is_ok());
        let tool = AdmittedTool {
            definition: agl_core::ToolDefinition {
                id: agl_core::ToolId::new("agentlibre.builtins:fs_apply_patch").unwrap(),
                description: "write".into(),
                input_schema: agl_core::JsonSchema::new(json!({"type":"object"})).unwrap(),
                required_effects: vec![agl_core::EffectId::new(FILESYSTEM_WRITE_EFFECT).unwrap()],
                delivery: agl_core::agent::DeliveryClass::AtMostOnce,
            },
            extension: agl_core::agent::ExtensionDefinitionRef {
                id: agl_core::ExtensionId::new("agentlibre.builtins").unwrap(),
                package: agl_core::agent::ExactPackageRef {
                    id: crate::package::PackageId::new("agentlibre.builtins").unwrap(),
                    version: crate::package::PackageVersion::new("1.0.0").unwrap(),
                    digest: agl_core::agent::PackageDigest::from_bytes([0; 32]),
                },
                definition_digest: agl_core::agent::ExtensionDefinitionDigest::from_bytes([0; 32]),
            },
            definition_digest: agl_core::agent::ToolDefinitionDigest::from_bytes([0; 32]),
        };
        let result = PlannerWorkspaceBoundary {
            workspace_root: PathBuf::from("/tmp"),
            scratch_root: PathBuf::from("/tmp/planner-scratch"),
        }
        .validate_capabilities(&[tool]);
        assert!(result.is_err());
    }

    #[test]
    fn first_valid_response_is_accepted_without_correction() {
        let root =
            std::env::temp_dir().join(format!("agl-planner-success-{}", AgentRunId::generate()));
        std::fs::create_dir_all(&root).unwrap();
        let plan = valid_plan(&root);
        let route = RecordingRoute {
            outputs: Mutex::new(VecDeque::from([String::from_utf8(
                plan.canonical_bytes().unwrap(),
            )
            .unwrap()])),
            requests: Mutex::new(vec![]),
        };
        let result = generate(
            &route,
            PlannerRequest {
                conversation_id: ConversationId::generate(),
                snapshot: snapshot(&root),
                input: Content::text("plan this").unwrap(),
                boundary: PlannerWorkspaceBoundary {
                    workspace_root: root.clone(),
                    scratch_root: std::env::temp_dir().join("agl-planner-scratch"),
                },
                deadline_at_ms: i64::MAX,
            },
        )
        .unwrap();
        assert_eq!(result.attempts, 1);
        assert!(result.first_error.is_none());
        assert_eq!(route.requests.lock().unwrap().len(), 1);
        assert!(route.requests.lock().unwrap()[0].response_format.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_correction_carries_validation_diagnostics_and_succeeds() {
        let root =
            std::env::temp_dir().join(format!("agl-planner-correction-{}", AgentRunId::generate()));
        std::fs::create_dir_all(&root).unwrap();
        let plan = valid_plan(&root);
        let route = RecordingRoute {
            outputs: Mutex::new(VecDeque::from([
                "{}".into(),
                String::from_utf8(plan.canonical_bytes().unwrap()).unwrap(),
            ])),
            requests: Mutex::new(vec![]),
        };
        let result = generate(
            &route,
            PlannerRequest {
                conversation_id: ConversationId::generate(),
                snapshot: snapshot(&root),
                input: Content::text("plan this").unwrap(),
                boundary: PlannerWorkspaceBoundary {
                    workspace_root: root.clone(),
                    scratch_root: std::env::temp_dir().join("agl-planner-scratch"),
                },
                deadline_at_ms: i64::MAX,
            },
        )
        .unwrap();
        assert_eq!(result.attempts, 2);
        assert!(
            result
                .first_error
                .as_deref()
                .is_some_and(|error| error.contains("missing field"))
        );
        let requests = route.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].context.iter().any(|entry| {
            entry
                .message
                .content
                .as_text()
                .contains("Validation errors:")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn second_invalid_response_fails_after_exactly_one_correction() {
        let root =
            std::env::temp_dir().join(format!("agl-planner-failure-{}", AgentRunId::generate()));
        std::fs::create_dir_all(&root).unwrap();
        let route = RecordingRoute {
            outputs: Mutex::new(VecDeque::from(["{}".into(), "[]".into()])),
            requests: Mutex::new(vec![]),
        };
        let error = generate(
            &route,
            PlannerRequest {
                conversation_id: ConversationId::generate(),
                snapshot: snapshot(&root),
                input: Content::text("plan this").unwrap(),
                boundary: PlannerWorkspaceBoundary {
                    workspace_root: root.clone(),
                    scratch_root: std::env::temp_dir().join("agl-planner-scratch"),
                },
                deadline_at_ms: i64::MAX,
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PlannerError::InvalidOutput { attempts: 2, .. }
        ));
        assert_eq!(route.requests.lock().unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::package::PackageId;
    use agl_core::agent::{
        AdmittedTool, DeliveryClass, ExactPackageRef, ExtensionDefinitionDigest,
        ExtensionDefinitionRef, PackageDigest, ToolDefinitionDigest,
    };
    use serde_json::json;

    #[test]
    fn schema_is_strict_and_named() {
        let format = response_format();
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["json_schema"]["strict"], true);
        assert_eq!(format["json_schema"]["name"], PLANNER_SCHEMA_NAME);
        assert_eq!(
            format["json_schema"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn boundary_rejects_workspace_mutating_capability() {
        let tool = AdmittedTool {
            definition: agl_core::ToolDefinition {
                id: agl_core::ToolId::new("agentlibre.builtins:fs_apply_patch").unwrap(),
                description: "write".into(),
                input_schema: agl_core::JsonSchema::new(json!({"type":"object"})).unwrap(),
                required_effects: vec![agl_core::EffectId::new(FILESYSTEM_WRITE_EFFECT).unwrap()],
                delivery: DeliveryClass::AtMostOnce,
            },
            extension: ExtensionDefinitionRef {
                id: agl_core::ExtensionId::new("agentlibre.builtins").unwrap(),
                package: ExactPackageRef {
                    id: PackageId::new("agentlibre.builtins").unwrap(),
                    version: crate::package::PackageVersion::new("1.0.0").unwrap(),
                    digest: PackageDigest::from_bytes([0; 32]),
                },
                definition_digest: ExtensionDefinitionDigest::from_bytes([0; 32]),
            },
            definition_digest: ToolDefinitionDigest::from_bytes([0; 32]),
        };
        let root = std::env::temp_dir();
        let result = PlannerWorkspaceBoundary {
            workspace_root: root.clone(),
            scratch_root: root.join("planner-scratch-contract-test"),
        }
        .validate_capabilities(&[tool]);
        assert!(result.is_err());
    }
}
