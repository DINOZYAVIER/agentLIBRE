use crate::*;
use serde_json::json;

#[test]
fn parses_plain_answer() {
    assert_eq!(
        parse_model_action("done"),
        ModelAction::Answer("done".to_string())
    );
}

#[test]
fn parses_valid_qwen_hermes_tool_call() {
    assert_eq!(
        parse_model_action(
            r#"<tool_call>{"name":"read_file","arguments":{"path":"README.MD"}}</tool_call>"#
        ),
        ModelAction::ToolCall(ToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        })
    );
}

#[test]
fn repairs_quoted_tool_json() {
    let action = parse_model_action(
        r#"<tool_call>"{\"name\":\"read_file\",\"arguments\":{\"path\":\"README.MD\"}}"</tool_call>"#,
    );

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call with repair");
    };
    let Some(ToolJsonRepair::Succeeded {
        strategy,
        tool_call,
        ..
    }) = malformed.repair
    else {
        panic!("expected successful repair");
    };

    assert_eq!(strategy, RepairStrategy::UnescapeQuotedJson);
    assert_eq!(tool_call.name, "read_file");
    assert_eq!(tool_call.arguments, json!({"path": "README.MD"}));
}

#[test]
fn repairs_one_missing_closing_brace() {
    let action = parse_model_action(
        r#"<tool_call>{"name":"read_file","arguments":{"path":"README.MD"}</tool_call>"#,
    );

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call with repair");
    };
    let Some(ToolJsonRepair::Succeeded {
        strategy,
        tool_call,
        ..
    }) = malformed.repair
    else {
        panic!("expected successful repair");
    };

    assert_eq!(strategy, RepairStrategy::AppendMissingBrace);
    assert_eq!(tool_call.name, "read_file");
    assert_eq!(tool_call.arguments, json!({"path": "README.MD"}));
}

#[test]
fn repairs_missing_tool_call_terminator_when_json_is_complete() {
    let action =
        parse_model_action(r#"<tool_call>{"name":"read_file","arguments":{"path":"README.MD"}}"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call with repair");
    };
    let Some(ToolJsonRepair::Succeeded {
        strategy,
        tool_call,
        ..
    }) = malformed.repair
    else {
        panic!("expected successful repair");
    };

    assert_eq!(
        malformed.classification,
        MalformedToolJsonKind::MissingTerminator
    );
    assert_eq!(strategy, RepairStrategy::AcceptMissingTerminator);
    assert_eq!(tool_call.name, "read_file");
    assert_eq!(tool_call.arguments, json!({"path": "README.MD"}));
}

#[test]
fn leaves_unrepairable_json_malformed() {
    let action = parse_model_action(r#"<tool_call>{"name":,"arguments":42</tool_call>"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call");
    };

    assert_eq!(malformed.classification, MalformedToolJsonKind::Syntax);
    assert!(matches!(
        malformed.repair,
        Some(ToolJsonRepair::Failed { .. })
    ));
}
