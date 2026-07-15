use crate::loader::LoadedFunction;
use crate::manifest::FUNCTION_SCHEMA;
pub fn render_function_context(function: &LoadedFunction) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_function_context>\n");
    content.push_str("schema: ");
    content.push_str(FUNCTION_SCHEMA);
    content.push('\n');
    content.push_str("id: ");
    content.push_str(&function.front_matter.id);
    content.push('\n');
    content.push_str("title: ");
    content.push_str(&function.front_matter.title);
    content.push('\n');
    if let Some(description) = &function.front_matter.description {
        content.push_str("description: ");
        content.push_str(description.trim());
        content.push('\n');
    }
    if let Some(profile) = function.front_matter.model_profile() {
        content.push_str("model_profile: ");
        content.push_str(profile);
        content.push('\n');
    }
    let skills = function.front_matter.selected_skills();
    if !skills.is_empty() {
        content.push_str("skills: ");
        content.push_str(&skills.join(", "));
        content.push('\n');
    }
    if !function.subagents.is_empty() {
        content.push_str("\nAvailable subagents:\n");
        for subagent_id in function.front_matter.selected_subagents() {
            let subagent = function
                .subagents
                .iter()
                .find(|candidate| &candidate.front_matter.id == subagent_id)
                .expect("validated root subagent remains loaded");
            content.push_str("- ");
            content.push_str(&subagent.front_matter.id);
            content.push_str(": ");
            content.push_str(&subagent.front_matter.title);
            content.push_str(" - ");
            content.push_str(subagent.front_matter.description.trim());
            content.push('\n');
        }
    }
    content.push_str("\nFunction system prompt:\n");
    content.push_str(function.system_prompt.trim());
    content.push('\n');
    content.push_str("</agentlibre_function_context>\n");
    content
}
