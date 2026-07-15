use crate::*;
#[test]
fn runtime_function_can_allow_missing_profile_when_config_overrides() {
    let root = std::env::temp_dir().join(format!(
        "agl-functions-missing-profile-{}",
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
schema: agentfunction/v1
id: coding
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

    let missing = resolve_runtime_function("coding", &workspace, &config).unwrap_err();
    assert!(missing.to_string().contains("missing-profile"));

    let allowed =
        resolve_runtime_function_allow_missing_profile("coding", &workspace, &config).unwrap();
    assert_eq!(allowed.model_profile.as_deref(), Some("missing-profile"));
    assert_eq!(allowed.profile_path, None);
    let _ = std::fs::remove_dir_all(&root);
}
