//! Planner policy: instructions, strict response schema, and the read-only
//! capability and workspace checks applied to planner Conversations.

use std::path::Path;

use agl_core::Content;
use agl_core::agent::{AdmittedTool, InstructionBlock, InstructionSet, InstructionSource};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) const PLANNER_SCHEMA_NAME: &str = "agentlibre_implementation_plan_v1";
pub(crate) const PLANNER_CORRECTION_PREFIX: &str = "Return only the corrected canonical JSON object. The previous assistant output was invalid and is retained in this Conversation.";
pub(crate) const PLANNER_DOMAIN_RULES: &str = "Cross-reference rules: each slice start_condition may contain only IDs from top-level evidence or that same slice's required_reads; never put a decision ID, invariant ID, acceptance ID, or done_when ID in start_condition; decisions are cited as sources and must not be used as start-condition references; slice depends_on names existing earlier slice IDs; step depends_on names steps in that slice; step satisfies and verification covers name existing invariant or objective.acceptance IDs; every declared invariant must appear in at least one step.satisfies or verification.covers entry, every objective acceptance ID must be covered too, and if no invariant is needed do not invent one; every step file must be declared in that slice's files; every ReadRequirement and ConditionalRead read ID must be globally unique across the entire plan, so suffix repeated concepts with the slice ID (for example read.decisions.implementation and read.decisions.test). Every slice that changes workspace files must include at least one executable behavior verification (for example a focused test, test suite, type check, linter, build, or runtime check) covering the changed behavior. A command that only reports repository state or differences, such as git diff, git status, or git diff --name-only, is not sufficient by itself.";

const FILESYSTEM_WRITE_EFFECT: &str = "agentlibre.files:file.write";
const TERMINAL_CONTROL_EFFECT: &str = "agentlibre.execution:terminal.control";

/// The planner workspace must be an absolute path to an existing directory.
pub(crate) fn validate_workspace(workspace_root: &Path) -> Result<()> {
    ensure!(
        workspace_root.is_absolute(),
        "planner workspace is not absolute"
    );
    let workspace = workspace_root.canonicalize()?;
    ensure!(workspace.is_dir(), "planner workspace is not a directory");
    Ok(())
}

/// Planner execution may read the workspace, but no admitted capability
/// may mutate it or retain a terminal session that can do so.
pub(crate) fn validate_capabilities(workspace_root: &Path, tools: &[AdmittedTool]) -> Result<()> {
    validate_workspace(workspace_root)?;
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

pub(crate) fn instructions(base: &InstructionSet) -> Result<InstructionSet> {
    let mut blocks = base.blocks.clone();
    blocks.push(InstructionBlock {
        source: InstructionSource::Agent,
        content: Content::text(
            format!("You are the agentLIBRE planner. Read repository instructions and accepted specs, inspect concrete code and tests, distinguish evidence from human decisions and recommendations, and put unresolved material choices in open_decisions. Partition the work into independently verifiable slices with exact files, symbols, reads, start conditions, steps, verification commands, and done checks. Emit only one JSON object matching the runtime-provided implementation-plan schema. The top-level schema field must be exactly \"agentlibre.implementation-plan/v1\". The top-level id must be a lowercase UUIDv7 with form xxxxxxxx-xxxx-7xxx-[89ab]xxx-xxxxxxxxxxxx. {} Do not emit Markdown or commentary.", PLANNER_DOMAIN_RULES),
        )?,
    });
    InstructionSet::new(blocks).map_err(|error| anyhow::anyhow!(error))
}

pub(crate) fn response_format() -> Value {
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
pub(crate) fn implementation_plan_schema() -> Value {
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

#[cfg(test)]
mod contract_tests {
    use super::*;
    use agl_core::agent::{
        AdmittedTool, DeliveryClass, ExactPackageRef, ExtensionDefinitionDigest,
        ExtensionDefinitionRef, PackageDigest, ToolDefinitionDigest,
    };
    use agl_runtime::package::PackageId;
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
    fn capabilities_reject_workspace_mutating_capability() {
        let tool = AdmittedTool {
            definition: agl_core::ToolDefinition {
                id: agl_core::ToolId::new("agentlibre.files:file.patch").unwrap(),
                description: "write".into(),
                input_schema: agl_core::JsonSchema::new(json!({"type":"object"})).unwrap(),
                required_effects: vec![agl_core::EffectId::new(FILESYSTEM_WRITE_EFFECT).unwrap()],
                delivery: DeliveryClass::AtMostOnce,
            },
            extension: ExtensionDefinitionRef {
                id: agl_core::ExtensionId::new("agentlibre.files").unwrap(),
                package: ExactPackageRef {
                    id: PackageId::new("agentlibre.files").unwrap(),
                    version: agl_runtime::package::PackageVersion::new("1.0.0").unwrap(),
                    digest: PackageDigest::from_bytes([0; 32]),
                },
                definition_digest: ExtensionDefinitionDigest::from_bytes([0; 32]),
            },
            definition_digest: ToolDefinitionDigest::from_bytes([0; 32]),
        };
        let root = std::env::temp_dir();
        let result = validate_capabilities(&root, &[tool]);
        assert!(result.is_err());
    }
}
