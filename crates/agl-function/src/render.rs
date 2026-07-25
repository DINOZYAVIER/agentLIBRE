use crate::loader::LoadedFunction;
pub fn render_function_context(function: &LoadedFunction) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_function_context>\n");
    content.push_str("id: ");
    content.push_str(function.front_matter.id());
    content.push('\n');
    if !function.subagents.is_empty() {
        content.push_str("\nAvailable subagents:\n");
        for subagent_id in function.front_matter.selected_subagents() {
            let subagent = function
                .subagents
                .iter()
                .find(|candidate| candidate.front_matter.id == *subagent_id)
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
    content.push_str("\nInstructions:\n");
    content.push_str(function.system_prompt.trim());
    content.push('\n');
    content.push_str("</agentlibre_function_context>\n");
    content
}
