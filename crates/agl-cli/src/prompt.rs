use agl_config::SystemPrompt;

const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../../../assets/prompts/system/default.md");
const VERSION_PLACEHOLDER: &str = "{{AGL_VERSION}}";

pub(crate) fn resolve_system_prompt(selection: SystemPrompt) -> Option<String> {
    match selection {
        SystemPrompt::BuiltinDefault => Some(
            DEFAULT_SYSTEM_PROMPT
                .trim()
                .replace(VERSION_PLACEHOLDER, env!("CARGO_PKG_VERSION")),
        ),
        SystemPrompt::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_default_bakes_cli_version() {
        let prompt = resolve_system_prompt(SystemPrompt::BuiltinDefault).unwrap();

        assert!(prompt.contains("You are an instance of agentLIBRE, an agentic system."));
        assert!(prompt.contains(&format!("You run on agl {}.", env!("CARGO_PKG_VERSION"))));
        assert!(!prompt.contains(VERSION_PLACEHOLDER));
    }

    #[test]
    fn builtin_default_does_not_describe_itself_as_demo() {
        let prompt = resolve_system_prompt(SystemPrompt::BuiltinDefault).unwrap();

        assert!(!prompt.to_lowercase().contains("demo"));
    }

    #[test]
    fn builtin_default_leaves_capabilities_to_runtime_context() {
        let prompt = resolve_system_prompt(SystemPrompt::BuiltinDefault).unwrap();

        assert!(!prompt.contains("model pull"));
        assert!(!prompt.contains("filesystem access"));
        assert!(!prompt.contains("tool execution"));
        assert!(!prompt.contains("not wired"));
    }

    #[test]
    fn none_disables_system_prompt() {
        assert_eq!(resolve_system_prompt(SystemPrompt::None), None);
    }
}
