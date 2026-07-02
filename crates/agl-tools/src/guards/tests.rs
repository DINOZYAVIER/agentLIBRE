use serde_json::json;

use super::*;
use crate::HookStatus;

#[test]
fn declaration_exposes_core_guard_hooks() {
    let declaration = declaration();
    let ids = declaration
        .hooks
        .iter()
        .map(|hook| hook.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec![
            JSON_VALIDATE_HOOK_ID,
            REPO_PATH_VALIDATE_HOOK_ID,
            TASK_SPEC_VALIDATE_HOOK_ID,
            SECRET_SCAN_VALIDATE_HOOK_ID,
            DIFF_SCOPE_VALIDATE_HOOK_ID,
            VERIFICATION_VALIDATE_HOOK_ID,
            COMMIT_MESSAGE_VALIDATE_HOOK_ID,
            SKILL_MANIFEST_VALIDATE_HOOK_ID,
            REVIEW_PACK_VALIDATE_HOOK_ID,
        ]
    );
}

#[test]
fn provider_registers_with_tool_catalog() {
    let mut catalog = ToolCatalog::new();

    register(&mut catalog).unwrap();

    assert!(catalog.has_hook(&HookId::new(TASK_SPEC_VALIDATE_HOOK_ID).unwrap()));
}

#[test]
fn json_guard_passes_and_fails() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                JSON_VALIDATE_HOOK_ID,
                json!({"text": "{\"ok\": true}"})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(JSON_VALIDATE_HOOK_ID, json!({"text": "{bad"})))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn repo_path_guard_rejects_escape_paths() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                REPO_PATH_VALIDATE_HOOK_ID,
                json!({"path": "crates/agl"})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                REPO_PATH_VALIDATE_HOOK_ID,
                json!({"path": "../secrets"})
            ))
            .status,
        HookStatus::Fail
    );
    assert_eq!(
        guards
            .run_hook(input(
                REPO_PATH_VALIDATE_HOOK_ID,
                json!({"path": ".git/config"})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn repo_path_guard_accepts_markdown_without_repo_paths() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                REPO_PATH_VALIDATE_HOOK_ID,
                json!({"content": "No repository paths here."})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                REPO_PATH_VALIDATE_HOOK_ID,
                json!({"content": "Touch crates/agl-cli/src/lib.rs only."})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                REPO_PATH_VALIDATE_HOOK_ID,
                json!({"content": "Never write ../secrets/config."})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn task_spec_guard_requires_contract_sections() {
    let guards = CoreGuards::new();
    let valid = r#"
# Problem
# Goal
# Scope
# Non-goals
# Acceptance Criteria
# Verification
"#;

    assert_eq!(
        guards
            .run_hook(input(
                TASK_SPEC_VALIDATE_HOOK_ID,
                json!({"markdown": valid})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                TASK_SPEC_VALIDATE_HOOK_ID,
                json!({"markdown": "# Problem"})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn secret_scan_guard_rejects_obvious_tokens() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                SECRET_SCAN_VALIDATE_HOOK_ID,
                json!({"content": "placeholder-token"})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                SECRET_SCAN_VALIDATE_HOOK_ID,
                json!({"content": "github_pat_abcdefghijklmnopqrstuvwxyz"})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn diff_scope_guard_rejects_generated_paths_in_patch_text() {
    let guards = CoreGuards::new();
    let patch = r#"
diff --git a/target/debug/build.log b/target/debug/build.log
new file mode 100644
"#;

    assert_eq!(
        guards
            .run_hook(input(
                DIFF_SCOPE_VALIDATE_HOOK_ID,
                json!({"content": patch})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn verification_guard_requires_evidence() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                VERIFICATION_VALIDATE_HOOK_ID,
                json!({"content": "Verification: cargo test -p agl-tools"})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                VERIFICATION_VALIDATE_HOOK_ID,
                json!({"content": "Changed the parser."})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn commit_message_guard_rejects_llm_dco_trailers() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                COMMIT_MESSAGE_VALIDATE_HOOK_ID,
                json!({"content": "Subject\n\nAssisted-by: Codex:gpt-5.5"})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                COMMIT_MESSAGE_VALIDATE_HOOK_ID,
                json!({"content": "Subject\n\nSigned-off-by: Codex <bot@example.com>"})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn skill_manifest_guard_checks_agentlibre_fields() {
    let guards = CoreGuards::new();
    let manifest = r#"---
name: change
description: Make repo changes.
version: 1
source: builtin
pack: agl
required_hooks: []
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees: []
---
Body.
"#;
    let incomplete = r#"---
name: change
description: Make repo changes.
---
Body.
"#;

    assert_eq!(
        guards
            .run_hook(input(
                SKILL_MANIFEST_VALIDATE_HOOK_ID,
                json!({"content": manifest})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                SKILL_MANIFEST_VALIDATE_HOOK_ID,
                json!({"content": incomplete})
            ))
            .status,
        HookStatus::Fail
    );
}

#[test]
fn review_pack_guard_requires_rendered_artifacts() {
    let guards = CoreGuards::new();

    assert_eq!(
        guards
            .run_hook(input(
                REVIEW_PACK_VALIDATE_HOOK_ID,
                json!({"content": ".agl/reviews/pack/review-manifest.json payload.json pr.html index.html"})
            ))
            .status,
        HookStatus::Pass
    );
    assert_eq!(
        guards
            .run_hook(input(
                REVIEW_PACK_VALIDATE_HOOK_ID,
                json!({"content": ".agl/reviews/pack/review-manifest.json"})
            ))
            .status,
        HookStatus::Fail
    );
}

fn input(hook_id: &str, payload: serde_json::Value) -> HookInput {
    HookInput {
        hook_id: HookId::new(hook_id).unwrap(),
        event: HookEvent::ArtifactWrite,
        payload,
    }
}
