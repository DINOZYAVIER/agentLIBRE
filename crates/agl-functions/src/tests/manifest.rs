use agl_capabilities::CapabilityId;

use crate::loader::parse_function_document;
use crate::*;
#[test]
fn parses_function_document() {
    let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
model:
  config: inference.toml
runtime:
  tool_mode: write
validation:
  runtime_identity:
    required: true
    fields:
      - function
      - skills
    repair_attempts: 2
skills:
  use:
    - repo-status
---
"#;

    let (front_matter, body) = parse_function_document(content).unwrap();

    assert_eq!(front_matter.id, "coding");
    assert_eq!(front_matter.model_profile(), None);
    assert_eq!(front_matter.model_config_path(), Some("inference.toml"));
    assert_eq!(
        front_matter.runtime_tool_mode(),
        Some(FunctionToolMode::Write)
    );
    assert_eq!(
        front_matter.runtime_identity_validation(),
        Some(RuntimeIdentityValidation {
            required: true,
            fields: vec!["function".to_string(), "skills".to_string()],
            repair_attempts: 2,
        })
    );
    assert_eq!(front_matter.selected_skills(), ["repo-status"]);
    assert!(body.trim().is_empty());
}

#[test]
fn runtime_function_preserves_function_tool_policy_states() {
    fn policy(allow: &[&str], deny: &[&str]) -> FunctionToolPolicy {
        FunctionToolPolicy::new(
            allow
                .iter()
                .map(|id| CapabilityId::new(*id).expect("test capability ID is valid")),
            deny.iter()
                .map(|id| CapabilityId::new(*id).expect("test capability ID is valid")),
        )
    }

    struct Case {
        name: &'static str,
        tools_yaml: &'static str,
        expected: Option<FunctionToolPolicy>,
    }

    let cases = [
        Case {
            name: "absent",
            tools_yaml: "",
            expected: None,
        },
        Case {
            name: "present-empty",
            tools_yaml: "tools: {}\n",
            expected: Some(FunctionToolPolicy::default()),
        },
        Case {
            name: "allow-and-deny",
            tools_yaml: "tools:\n  allow:\n    - fs.read\n    - repo.status\n  deny:\n    - repo.status\n",
            expected: Some(policy(&["fs.read", "repo.status"], &["repo.status"])),
        },
        Case {
            name: "deny-only",
            tools_yaml: "tools:\n  deny:\n    - fs.edit\n",
            expected: Some(policy(&[], &["fs.edit"])),
        },
    ];

    let root =
        std::env::temp_dir().join(format!("agl-functions-tool-policy-{}", std::process::id()));
    let workspace = root.join("workspace");
    let config = root.join("config");
    let _ = std::fs::remove_dir_all(&root);

    for (index, case) in cases.iter().enumerate() {
        let id = format!("policy-{index}");
        let function_root = workspace.join(".agl/functions").join(&id);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join(FUNCTION_FILE_NAME),
            format!(
                "---\nschema: agentfunction/v1\nid: {id}\ntitle: Policy {index}\n{}---\n",
                case.tools_yaml
            ),
        )
        .unwrap();
        std::fs::write(
            function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
            "Test policy.\n",
        )
        .unwrap();

        let runtime =
            resolve_runtime_function_allow_missing_profile(&id, &workspace, &config).unwrap();
        assert_eq!(runtime.tool_policy, case.expected, "{}", case.name);

        let report = function_status(&id, &workspace, &config);
        assert_eq!(report.tool_policy, case.expected, "{} status", case.name);

        let serialized = serde_yaml::to_value(&runtime).unwrap();
        let serialized_policy = serialized
            .get("tool_policy")
            .unwrap_or_else(|| panic!("{} evidence omitted tool_policy", case.name))
            .clone();
        let round_trip: Option<FunctionToolPolicy> =
            serde_yaml::from_value(serialized_policy).unwrap();
        assert_eq!(round_trip, case.expected, "{} evidence", case.name);
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rejects_model_profile_and_config_together() {
    let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
model:
  profile: local
  config: inference.toml
---
"#;

    let (front_matter, _) = parse_function_document(content).unwrap();
    let err = front_matter.validate().unwrap_err();

    assert!(
        err.to_string()
            .contains("model.profile and model.config cannot both be set")
    );
}

#[test]
fn rejects_unknown_fields_without_extension_prefix() {
    let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
unknown: true
---
"#;

    let (front_matter, _) = parse_function_document(content).unwrap();
    let err = front_matter.validate().unwrap_err();

    assert!(
        err.to_string()
            .contains("unknown function front matter field")
    );
}
