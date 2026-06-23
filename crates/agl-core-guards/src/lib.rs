use agl_extension::{
    ExtensionId, HookDeclaration, HookEvent, HookId, HookInput, HookMessage, HookResult,
    HookStatus, StaticExtension, StaticExtensionDeclaration, StaticExtensionRegistry,
    StaticExtensionRegistryError,
};

pub const EXTENSION_ID: &str = "core-guards";
pub const JSON_VALIDATE_HOOK_ID: &str = "json.validate";
pub const REPO_PATH_VALIDATE_HOOK_ID: &str = "repo_path.validate";
pub const TASK_SPEC_VALIDATE_HOOK_ID: &str = "task_spec.validate";

#[derive(Clone, Debug)]
pub struct CoreGuards {
    declaration: StaticExtensionDeclaration,
}

impl Default for CoreGuards {
    fn default() -> Self {
        Self {
            declaration: declaration(),
        }
    }
}

impl CoreGuards {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StaticExtension for CoreGuards {
    fn declaration(&self) -> &StaticExtensionDeclaration {
        &self.declaration
    }

    fn run_hook(&self, input: HookInput) -> HookResult {
        match input.hook_id.as_str() {
            JSON_VALIDATE_HOOK_ID => validate_json(input),
            REPO_PATH_VALIDATE_HOOK_ID => validate_repo_path(input),
            TASK_SPEC_VALIDATE_HOOK_ID => validate_task_spec(input),
            _ => fail(
                input.hook_id,
                "unknown_hook",
                "unknown core guard hook",
                None,
            ),
        }
    }
}

pub fn declaration() -> StaticExtensionDeclaration {
    StaticExtensionDeclaration::new(
        ExtensionId::new(EXTENSION_ID).expect("core guard extension id is valid"),
        "Core Guards",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("core guard declaration is valid")
    .with_hook(HookDeclaration {
        id: HookId::new(JSON_VALIDATE_HOOK_ID).expect("json hook id is valid"),
        event: HookEvent::ModelResponse,
        required: false,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(REPO_PATH_VALIDATE_HOOK_ID).expect("repo path hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(TASK_SPEC_VALIDATE_HOOK_ID).expect("task spec hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
}

pub fn register(
    registry: &mut StaticExtensionRegistry,
) -> Result<(), StaticExtensionRegistryError> {
    registry.register(declaration())
}

fn validate_json(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return fail(
            input.hook_id,
            "missing_json_text",
            "json.validate requires a string payload field named text, content, json, or artifact",
            None,
        );
    };
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(_) => pass(input.hook_id),
        Err(err) => fail(
            input.hook_id,
            "invalid_json",
            "payload is not valid JSON",
            Some(err.to_string()),
        ),
    }
}

fn validate_repo_path(input: HookInput) -> HookResult {
    let paths = payload_paths(&input.payload);

    let invalid = paths
        .iter()
        .filter_map(|path| {
            validate_single_repo_path(path)
                .err()
                .map(|reason| (path, reason))
        })
        .collect::<Vec<_>>();
    if invalid.is_empty() {
        pass(input.hook_id)
    } else {
        let details = invalid
            .into_iter()
            .map(|(path, reason)| format!("{path}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        fail(
            input.hook_id,
            "invalid_repo_path",
            "one or more repository paths are invalid",
            Some(details),
        )
    }
}

fn validate_task_spec(input: HookInput) -> HookResult {
    let Some(markdown) = payload_text(&input.payload) else {
        return fail(
            input.hook_id,
            "missing_task_spec_text",
            "task_spec.validate requires a string payload field named text, content, markdown, or artifact",
            None,
        );
    };
    let lower = markdown.to_ascii_lowercase();
    let required = [
        "problem",
        "goal",
        "scope",
        "non-goals",
        "acceptance criteria",
        "verification",
    ];
    let missing = required
        .iter()
        .filter(|section| !lower.contains(**section))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "task_spec_missing_sections",
            "task spec is missing required sections",
            Some(missing.join(", ")),
        )
    }
}

fn payload_text(payload: &serde_json::Value) -> Option<&str> {
    for field in ["text", "content", "json", "markdown", "artifact"] {
        if let Some(value) = payload.get(field).and_then(serde_json::Value::as_str) {
            return Some(value);
        }
    }
    payload.as_str()
}

fn payload_paths(payload: &serde_json::Value) -> Vec<String> {
    if let Some(path) = payload.get("path").and_then(serde_json::Value::as_str) {
        return vec![path.to_string()];
    }
    if let Some(paths) = payload.get("paths").and_then(serde_json::Value::as_array) {
        return paths
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(content) = payload_text(payload) {
        return extract_markdown_repo_paths(content);
    }
    payload
        .as_str()
        .map(|path| vec![path.to_string()])
        .unwrap_or_default()
}

fn extract_markdown_repo_paths(content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for candidate in content
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | '(' | '[' | ']'))
        .map(|candidate| {
            candidate.trim_matches(|ch: char| {
                matches!(ch, '`' | '"' | '\'' | ':' | '.' | '!' | '?' | '<' | '>')
            })
        })
        .filter(|candidate| candidate.contains('/'))
    {
        if looks_like_repo_path(candidate) {
            paths.push(candidate.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn looks_like_repo_path(candidate: &str) -> bool {
    !candidate.contains("://")
        && !candidate.starts_with('#')
        && !candidate.starts_with('@')
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

fn validate_single_repo_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty");
    }
    if path.starts_with('/') {
        return Err("absolute path");
    }
    if path.contains('\\') {
        return Err("backslashes are not accepted");
    }
    if path.contains('\0') {
        return Err("NUL byte");
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".." | ".git"))
    {
        return Err("contains empty, dot, parent, or .git segment");
    }
    Ok(())
}

fn pass(hook_id: HookId) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Pass,
        messages: Vec::new(),
    }
}

fn fail(hook_id: HookId, code: &str, message: &str, fix: Option<String>) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Fail,
        messages: vec![HookMessage {
            code: code.to_string(),
            message: message.to_string(),
            fix,
        }],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
            ]
        );
    }

    #[test]
    fn extension_registers_with_static_registry() {
        let mut registry = StaticExtensionRegistry::new();

        register(&mut registry).unwrap();

        assert!(registry.has_hook(&HookId::new(TASK_SPEC_VALIDATE_HOOK_ID).unwrap()));
    }

    #[test]
    fn builtin_task_spec_skill_requirements_are_satisfied() {
        let skills = agl_skills::builtin_registry().unwrap();
        let mut registry = StaticExtensionRegistry::new();
        register(&mut registry).unwrap();

        skills
            .verify_required_hooks(
                &agl_extension::SkillId::new("core:task-spec").unwrap(),
                &registry,
            )
            .unwrap();
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

    fn input(hook_id: &str, payload: serde_json::Value) -> HookInput {
        HookInput {
            hook_id: HookId::new(hook_id).unwrap(),
            event: HookEvent::ArtifactWrite,
            payload,
        }
    }
}
