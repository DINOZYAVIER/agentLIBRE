use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use agl_package::{
    PackageCompositionInput, PackageSourceInput, PackageSourceKind, PackageSourceProvenance,
    PackageSourceTier,
};
use agl_runtime::{AgentLibrePaths, compose_packages};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn runtime_freezes_typed_git_source_without_opening_a_repository() {
    let root = std::env::temp_dir().join(format!(
        "agl172-runtime-source-input-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let workspace = root.join("workspace");
    let source = workspace.join("prepared-source");
    write_skill(&source);
    let revision = "a".repeat(40);
    let tree = "b".repeat(40);
    let prepared = PackageSourceInput::new(
        "prepared".parse().unwrap(),
        PackageSourceTier::Workspace,
        PackageSourceKind::Git,
        &source,
        Some(PackageSourceProvenance::new(&revision, &tree)),
    )
    .unwrap();
    let input = PackageCompositionInput::new(&workspace, [prepared]).unwrap();
    let composition =
        compose_packages(&AgentLibrePaths::from_agl_home(root.join("home")), input).unwrap();
    let graph = composition
        .resolve_for_lock_refresh(&"skill:prepared@*".parse().unwrap())
        .unwrap();
    let candidate = &graph.nodes[&graph.root].candidate;
    assert_eq!(
        candidate.source_revision.as_deref(),
        Some(revision.as_str())
    );
    assert_eq!(candidate.source_tree.as_deref(), Some(tree.as_str()));
    let lock = graph.lock().unwrap();
    assert_eq!(
        lock.packages[0].source_revision.as_deref(),
        Some(revision.as_str())
    );
    assert_eq!(lock.packages[0].source_tree.as_deref(), Some(tree.as_str()));
    fs::remove_dir_all(root).unwrap();
}

fn write_skill(source: &Path) {
    let package = source.join("skills/prepared");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("SKILL.md"),
        r#"---
package:
  schema: agentlibre.package/v1
  type: skill
  id: prepared
  version: 1.0.0
  payload_schema: agentlibre.skill/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
description: Prepared source fixture.
pack: agl
required_hooks: []
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees:
  - prepared source
---

Prepared source.
"#,
    )
    .unwrap();
}
