use super::*;

#[test]
fn command_lexer_handles_quotes_and_escapes_without_shell_expansion() {
    assert_eq!(
        lex_command("workspace 'dir with spaces'/child\\ name").unwrap(),
        vec!["workspace", "dir with spaces/child name"]
    );
    assert_eq!(
        lex_command("workspace \"$HOME/*.rs\"").unwrap()[1],
        "$HOME/*.rs"
    );
    assert!(lex_command("workspace 'unfinished").is_err());
    assert!(lex_command("workspace trailing\\").is_err());
}

#[test]
fn operation_mode_parser_uses_the_canonical_catalog_spelling() {
    assert_eq!(
        parse_protocol_tool_mode("read-only").unwrap(),
        ProtocolToolMode::ReadOnly
    );
    assert_eq!(
        parse_protocol_tool_mode("execute").unwrap(),
        ProtocolToolMode::Execute
    );
    assert!(parse_protocol_tool_mode("read_only").is_err());
}
