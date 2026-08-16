use agl_package::{InMemoryPackageView, PackageId, PackageRelativePath};
use agl_skill::{SkillHarness, SkillSource};

fn view(skill: &str) -> InMemoryPackageView {
    InMemoryPackageView::new([(
        "SKILL.md".parse::<PackageRelativePath>().unwrap(),
        skill.as_bytes().to_vec(),
    )])
    .unwrap()
}

// AGL172-063.
#[test]
fn skill_parser_rejects_artifacts_frontmatter_instead_of_creating_folders() {
    let package_id: PackageId = "example.skill".parse().unwrap();
    let error = SkillHarness::parse_package_view(
        &view(
            r#"---
package:
  schema: agentlibre.package/v1
  type: skill
  id: example.skill
  version: 1.0.0
  payload_schema: agentlibre.skill/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
description: Fixture skill.
pack: fixture
required_hooks: []
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees: [fixture]
artifacts:
  - id: tasks
    path: .agl/tasks
    kind: git
    access: read
---
Instructions.
"#,
        ),
        package_id.as_str(),
        SkillSource::Local,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("unknown field `artifacts`"), "{message}");
}

// AGL172-058 and AGL172-064.
#[test]
fn loading_a_skill_never_creates_or_exports_an_artifact_path() {
    let package_id: PackageId = "example.skill".parse().unwrap();
    let harness = SkillHarness::parse_package_view(
        &view(
            r#"---
package:
  schema: agentlibre.package/v1
  type: skill
  id: example.skill
  version: 1.0.0
  payload_schema: agentlibre.skill/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
description: Fixture skill.
pack: fixture
required_hooks: []
allowed_tools:
  - core.repo:artifact.commit
context_budget_tokens: 128
references:
  include: []
guarantees: [fixture]
---
Use the admitted Tool.
"#,
        ),
        package_id.as_str(),
        SkillSource::Local,
    )
    .unwrap();
    assert_eq!(harness.source_path, "package:example.skill/SKILL.md");
    assert_eq!(harness.allowed_tools.len(), 1);
}
