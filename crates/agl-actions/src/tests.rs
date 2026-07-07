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
fn parses_valid_gemma_tool_call() {
    assert_eq!(
        parse_model_action(r#"<|tool_call>call:fs.read{path:<|"|>README.MD<|"|>}<tool_call|>"#),
        ModelAction::ToolCall(ToolCall {
            name: "fs.read".to_string(),
            arguments: json!({"path": "README.MD"}),
        })
    );
}

#[test]
fn parses_gemma_scalar_arguments() {
    assert_eq!(
        parse_model_action(
            r#"<|tool_call>call:memory.search{query:<|"|>rust<|"|>,limit:5,exact:false,after:null}<tool_call|>"#
        ),
        ModelAction::ToolCall(ToolCall {
            name: "memory.search".to_string(),
            arguments: json!({
                "query": "rust",
                "limit": 5,
                "exact": false,
                "after": null,
            }),
        })
    );
}

#[test]
fn parses_gemma_json_quoted_string_argument() {
    assert_eq!(
        parse_model_action(r#"<|tool_call>call:fs.read{path:"README.MD"}<tool_call|>"#),
        ModelAction::ToolCall(ToolCall {
            name: "fs.read".to_string(),
            arguments: json!({"path": "README.MD"}),
        })
    );
}

#[test]
fn parses_gemma_mixed_json_quoted_and_scalar_arguments() {
    assert_eq!(
        parse_model_action(
            r#"<|tool_call>call:memory.search{query:"rust",limit:5,exact:false,after:null}<tool_call|>"#
        ),
        ModelAction::ToolCall(ToolCall {
            name: "memory.search".to_string(),
            arguments: json!({
                "query": "rust",
                "limit": 5,
                "exact": false,
                "after": null,
            }),
        })
    );
}

#[test]
fn parses_gemma_json_quoted_string_with_escaped_quote_and_comma() {
    assert_eq!(
        parse_model_action(r#"<|tool_call>call:fs.read{path:"notes/a,\"b,c.md"}<tool_call|>"#),
        ModelAction::ToolCall(ToolCall {
            name: "fs.read".to_string(),
            arguments: json!({"path": "notes/a,\"b,c.md"}),
        })
    );
}

#[test]
fn rejects_nested_gemma_json_object_argument() {
    let action =
        parse_model_action(r#"<|tool_call>call:fs.read{payload:{"path":"README.MD"}}<tool_call|>"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call");
    };

    assert_eq!(
        malformed.classification,
        MalformedToolJsonKind::InvalidShape
    );
    assert_eq!(malformed.repair, None);
}

#[test]
fn rejects_nested_gemma_json_array_argument() {
    let action = parse_model_action(r#"<|tool_call>call:fs.read{items:[1,2]}<tool_call|>"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call");
    };

    assert_eq!(
        malformed.classification,
        MalformedToolJsonKind::InvalidShape
    );
    assert_eq!(malformed.repair, None);
}

#[test]
fn classifies_missing_gemma_terminator_without_json_repair() {
    let action = parse_model_action(r#"<|tool_call>call:fs.read{path:<|"|>README.MD<|"|>}"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call");
    };

    assert_eq!(
        malformed.classification,
        MalformedToolJsonKind::MissingTerminator
    );
    assert_eq!(
        malformed.raw_json,
        r#"call:fs.read{path:<|"|>README.MD<|"|>}"#
    );
    assert_eq!(malformed.repair, None);
}

#[test]
fn classifies_invalid_gemma_shape() {
    let action = parse_model_action(r#"<|tool_call>fs.read{path:<|"|>README.MD<|"|>}<tool_call|>"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call");
    };

    assert_eq!(
        malformed.classification,
        MalformedToolJsonKind::InvalidShape
    );
    assert_eq!(malformed.repair, None);
}

#[test]
fn parses_earliest_tool_call_format() {
    assert_eq!(
        parse_model_action(
            r#"<|tool_call>call:fs.read{path:<|"|>README.MD<|"|>}<tool_call|> then <tool_call>{"name":"other","arguments":{}}</tool_call>"#
        ),
        ModelAction::ToolCall(ToolCall {
            name: "fs.read".to_string(),
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
fn classifies_missing_terminator_before_json_syntax() {
    let action = parse_model_action(r#"<tool_call>{"name":,"arguments":42"#);

    let ModelAction::MalformedToolCall(malformed) = action else {
        panic!("expected malformed tool call");
    };

    assert_eq!(
        malformed.classification,
        MalformedToolJsonKind::MissingTerminator
    );
    assert!(matches!(
        malformed.repair,
        Some(ToolJsonRepair::Failed { .. })
    ));
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
