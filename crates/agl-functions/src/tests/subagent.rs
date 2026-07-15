use std::path::PathBuf;

use crate::*;
fn graph_fixture(
    case: &str,
    root_subagents: &[&str],
    subagents: &[(&str, &str)],
) -> (PathBuf, FunctionLocator) {
    let root =
        std::env::temp_dir().join(format!("agl-functions-graph-{}-{case}", std::process::id()));
    let function_root = root.join("graph");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(function_root.join("subagents")).unwrap();
    let selected = root_subagents
        .iter()
        .map(|id| format!("    - {id}\n"))
        .collect::<String>();
    std::fs::write(
            function_root.join(FUNCTION_FILE_NAME),
            format!(
                "---\nschema: agentfunction/v1\nid: graph\ntitle: Graph\nsubagents:\n  use:\n{selected}delegation:\n  max_depth: 4\n  max_children_per_run: 4\n  max_descendants: 8\n  max_total_output_tokens: 4096\n  timeout_seconds: 600\n---\n"
            ),
        )
        .unwrap();
    std::fs::write(
        function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
        "Coordinate declared subagents.\n",
    )
    .unwrap();
    for (id, document) in subagents {
        std::fs::write(
            function_root.join("subagents").join(format!("{id}.md")),
            document,
        )
        .unwrap();
    }
    let locator = FunctionLocator {
        reference: "graph".to_string(),
        source: FunctionSource::Workspace,
        path: function_root.join(FUNCTION_FILE_NAME),
        root_dir: function_root,
    };
    (root, locator)
}

fn subagent_document(id: &str, children: &[&str]) -> String {
    let selected = children
        .iter()
        .map(|child| format!("    - {child}\n"))
        .collect::<String>();
    format!(
        "---\nschema: agentlibre/subagent/v1\nid: {id}\ntitle: {id}\ndescription: Handles {id} tasks.\nmodel:\n  inherit: true\ntools:\n  mode: read-only\n  allow: []\n  deny: []\nsubagents:\n  use:\n{selected}limits:\n  max_model_attempts: 2\n  max_output_tokens: 512\n  max_capability_calls: 4\n  timeout_seconds: 120\n---\n\n# Mission\n\nPrivate instructions for {id}.\n"
    )
}

#[test]
fn renders_subagent_context() {
    let root = std::env::temp_dir().join(format!("agl-functions-render-{}", std::process::id()));
    let function_root = root.join("coding");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(function_root.join("subagents")).unwrap();
    std::fs::write(
        function_root.join(FUNCTION_FILE_NAME),
        r#"---
schema: agentfunction/v1
id: coding
title: Coding
subagents:
  use:
    - reviewer
delegation:
  max_depth: 2
  max_children_per_run: 2
  max_descendants: 4
  max_total_output_tokens: 2048
  timeout_seconds: 300
---
"#,
    )
    .unwrap();
    std::fs::write(
        function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
        "Code.\n",
    )
    .unwrap();
    std::fs::write(
        function_root.join("subagents").join("reviewer.md"),
        r#"---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
description: Reviews a delegated task.
model:
  inherit: true
tools:
  mode: read-only
  allow: []
  deny: []
subagents:
  use: []
limits:
  max_model_attempts: 2
  max_output_tokens: 512
  max_capability_calls: 4
  timeout_seconds: 120
---

# Mission

Review.
"#,
    )
    .unwrap();
    let locator = FunctionLocator {
        reference: "coding".to_string(),
        source: FunctionSource::Workspace,
        path: function_root.join(FUNCTION_FILE_NAME),
        root_dir: function_root,
    };

    let loaded = load_function(locator).unwrap();
    let context = render_function_context(&loaded);

    assert!(context.contains("id: coding"));
    assert!(context.contains("Function system prompt"));
    assert!(context.contains("Code."));
    assert!(context.contains("Available subagents"));
    assert!(context.contains("Reviews a delegated task."));
    assert!(!context.contains("Review.\n"));
    assert!(!context.contains("subagents/reviewer.md"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resolves_complete_acyclic_subagent_graph() {
    let reviewer = subagent_document("reviewer", &["researcher"]);
    let researcher = subagent_document("researcher", &[]);
    let (root, locator) = graph_fixture(
        "valid",
        &["reviewer"],
        &[("reviewer", &reviewer), ("researcher", &researcher)],
    );

    let loaded = load_function(locator).unwrap();
    assert_eq!(loaded.subagents.len(), 2);
    assert_eq!(loaded.subagents[0].front_matter.id, "researcher");
    assert_eq!(loaded.subagents[1].front_matter.id, "reviewer");
    assert_eq!(
        loaded.subagents[1].front_matter.subagents.use_,
        ["researcher"]
    );
    assert!(loaded.subagents.iter().all(|subagent| {
        subagent.source_digest.starts_with("sha256:") && !subagent.body.trim().is_empty()
    }));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn rejects_unknown_subagent_reference() {
    let reviewer = subagent_document("reviewer", &["missing"]);
    let (root, locator) = graph_fixture("unknown", &["reviewer"], &[("reviewer", &reviewer)]);

    let error = load_function(locator).unwrap_err();
    assert!(format!("{error:#}").contains("failed to read declared subagent `missing`"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn rejects_direct_and_indirect_subagent_cycles() {
    let direct = subagent_document("reviewer", &["reviewer"]);
    let (direct_root, direct_locator) =
        graph_fixture("direct-cycle", &["reviewer"], &[("reviewer", &direct)]);
    let error = load_function(direct_locator).unwrap_err();
    assert!(error.to_string().contains("cannot delegate to itself"));
    let _ = std::fs::remove_dir_all(direct_root);

    let reviewer = subagent_document("reviewer", &["researcher"]);
    let researcher = subagent_document("researcher", &["reviewer"]);
    let (indirect_root, indirect_locator) = graph_fixture(
        "indirect-cycle",
        &["reviewer"],
        &[("reviewer", &reviewer), ("researcher", &researcher)],
    );
    let error = load_function(indirect_locator).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("reviewer -> researcher -> reviewer")
    );
    let _ = std::fs::remove_dir_all(indirect_root);
}

#[test]
fn rejects_subagent_without_finite_limits() {
    let document = r#"---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
description: Reviews tasks.
model:
  inherit: true
tools:
  mode: read-only
  allow: []
  deny: []
subagents:
  use: []
---

Review.
"#;
    let (root, locator) = graph_fixture("missing-limits", &["reviewer"], &[("reviewer", document)]);

    let error = load_function(locator).unwrap_err();
    assert!(format!("{error:#}").contains("missing field `limits`"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn rejects_conflicting_subagent_model_selection() {
    let document = subagent_document("reviewer", &[]).replace(
        "model:\n  inherit: true",
        "model:\n  inherit: true\n  profile: local",
    );
    let (root, locator) =
        graph_fixture("model-conflict", &["reviewer"], &[("reviewer", &document)]);

    let error = load_function(locator).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exactly one of inherit=true or profile")
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn explicit_empty_subagent_allow_list_denies_all_tools() {
    let reviewer = subagent_document("reviewer", &[]);
    let (root, locator) = graph_fixture("empty-tools", &["reviewer"], &[("reviewer", &reviewer)]);

    let loaded = load_function(locator).unwrap();
    let policy = loaded.subagents[0].front_matter.tools.to_runtime_policy();
    assert!(policy.allow.is_empty());
    assert!(policy.deny.is_empty());

    let _ = std::fs::remove_dir_all(root);
}
