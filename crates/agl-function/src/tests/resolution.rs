use crate::*;
#[test]
fn package_bound_profile_is_an_id_not_a_local_file() {
    let root = std::env::temp_dir().join(format!(
        "agl-function-missing-profile-{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let config = root.join("config");
    let function_root = workspace.join(".agl/functions/coding");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::write(
        function_root.join(FUNCTION_FILE_NAME),
        r#"---
package:
  schema: agentlibre.package/v1
  type: function
  id: coding
  version: 1.0.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Coding
model:
  profile: missing-profile
---
"#,
    )
    .unwrap();
    std::fs::write(
        function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
        "Code.\n",
    )
    .unwrap();

    let allowed = resolve_runtime_function("coding", &workspace, &config).unwrap();
    assert_eq!(allowed.model_profile.as_deref(), Some("missing-profile"));
    let _ = std::fs::remove_dir_all(&root);
}
