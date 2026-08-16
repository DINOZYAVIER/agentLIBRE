use super::*;

#[test]
fn private_history_is_bounded_and_uses_only_opaque_workspace_scope() {
    let root = std::env::temp_dir().join(format!(
        "agl-terminal-history-test-{}",
        agl_ids::RequestId::generate()
    ));
    let state_dir = root.join("state");
    let workspace_history_scope = format!("sha256:{}", "b".repeat(64));
    let (mut history, warnings) = InputHistory::load(&state_dir, &workspace_history_scope, true);
    assert!(warnings.is_empty());
    history.record_prompt("hello").unwrap();
    history.record_prompt("hello").unwrap();
    let history_root = history.root.clone().unwrap();
    assert!(
        !history_root
            .to_string_lossy()
            .contains(&workspace_history_scope)
    );
    assert_eq!(history.prompt, vec!["hello"]);
    assert!(history.entries(ComposerMode::Shell).is_empty());
    assert!(!history_root.join("shell.jsonl").exists());
    let (reloaded, warnings) = InputHistory::load(&state_dir, &workspace_history_scope, true);
    assert!(warnings.is_empty());
    assert_eq!(reloaded.prompt, vec!["hello"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(history_root.join("prompt.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::remove_dir_all(root).unwrap();
}
