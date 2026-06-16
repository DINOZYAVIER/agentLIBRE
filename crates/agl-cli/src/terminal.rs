pub(crate) fn assistant_text_for_terminal(content: &str) -> String {
    let mut content = content.trim();
    if let Some(stripped) = content.strip_prefix("Assistant:") {
        content = stripped.trim_start();
    }

    let marker_offset = ["\nUser:", "\nAssistant:", "\nTool:"]
        .iter()
        .filter_map(|marker| content.find(marker))
        .min()
        .unwrap_or(content.len());

    content[..marker_offset].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_text_cuts_generated_next_turn() {
        let content = "agentLIBRE ok\n\nUser:\nnew prompt\n\nAssistant:\nnext";

        assert_eq!(assistant_text_for_terminal(content), "agentLIBRE ok");
    }

    #[test]
    fn terminal_text_cuts_generated_tool_continuation() {
        let content = "agentLIBRE ok\nTool:\nobservation";

        assert_eq!(assistant_text_for_terminal(content), "agentLIBRE ok");
    }

    #[test]
    fn terminal_text_strips_leading_assistant_label() {
        assert_eq!(
            assistant_text_for_terminal("Assistant:\nhello\n"),
            "hello".to_string()
        );
    }
}
