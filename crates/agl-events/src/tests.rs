use crate::*;
use serde_json::json;

fn example_events() -> Vec<AgentEvent> {
    let turn_id = "turn-1".to_string();
    vec![
        AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
            user_input: "read README".to_string(),
        },
        AgentEvent::PromptRendered {
            turn_id: turn_id.clone(),
            message_count: 1,
        },
        AgentEvent::ModelRequested {
            turn_id: turn_id.clone(),
            request_index: 0,
        },
        AgentEvent::ModelResponseReceived {
            turn_id: turn_id.clone(),
            request_index: 0,
            content: "answer\nwith newline".to_string(),
        },
        AgentEvent::ModelRequestFailed {
            turn_id: turn_id.clone(),
            request_index: 0,
            message: "backend stderr secret".to_string(),
        },
        AgentEvent::ModelActionParsed {
            turn_id: turn_id.clone(),
            action: ParsedActionEvent::Answer,
        },
        AgentEvent::ModelActionParsed {
            turn_id: turn_id.clone(),
            action: ParsedActionEvent::ToolCall {
                name: "read_file".to_string(),
            },
        },
        AgentEvent::ToolJsonMalformed {
            turn_id: turn_id.clone(),
            classification: ToolJsonMalformedKind::Syntax,
            raw_json: "{bad".to_string(),
        },
        AgentEvent::ToolJsonRepairAttempted {
            turn_id: turn_id.clone(),
            strategy: "unescape_quoted_json".to_string(),
        },
        AgentEvent::ToolJsonRepairSucceeded {
            turn_id: turn_id.clone(),
            strategy: "unescape_quoted_json".to_string(),
            repaired_json: r#"{"name":"read_file","arguments":{"path":"README.MD"}}"#.to_string(),
        },
        AgentEvent::ToolJsonRepairFailed {
            turn_id: turn_id.clone(),
            strategy: "unescape_quoted_json".to_string(),
            message: "expected value".to_string(),
        },
        AgentEvent::ToolArgsValidated {
            turn_id: turn_id.clone(),
            name: "read_file".to_string(),
            arguments: json!({"path":"README.MD"}),
        },
        AgentEvent::ToolArgsInvalid {
            turn_id: turn_id.clone(),
            name: "read_file".to_string(),
            message: "missing required argument path".to_string(),
        },
        AgentEvent::ToolHiddenRejected {
            turn_id: turn_id.clone(),
            name: "write_file".to_string(),
        },
        AgentEvent::ToolLimitReached {
            turn_id: turn_id.clone(),
            limit: 0,
        },
        AgentEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            name: "read_file".to_string(),
            arguments: json!({"path":"README.MD"}),
        },
        AgentEvent::ToolCallFinished {
            turn_id: turn_id.clone(),
            name: "read_file".to_string(),
            observation: "README contents".to_string(),
        },
        AgentEvent::ToolCallFailed {
            turn_id: turn_id.clone(),
            name: "read_file".to_string(),
            message: "tool failed with secret path".to_string(),
        },
        AgentEvent::ObservationAppended {
            turn_id: turn_id.clone(),
            name: "read_file".to_string(),
            observation: "README contents".to_string(),
        },
        AgentEvent::AnswerFinal {
            turn_id: turn_id.clone(),
            answer: "done".to_string(),
        },
        AgentEvent::TurnStopped {
            turn_id: turn_id.clone(),
            reason: StopReasonEvent::ToolJsonUnrepairable,
            visible: true,
        },
        AgentEvent::TurnFinished {
            turn_id,
            status: TurnFinishStatus::Answered,
        },
    ]
}

#[test]
fn serializes_every_event_as_jsonl() {
    for event in example_events() {
        let line = event.to_jsonl_line().expect("event serializes");
        assert!(!line.contains('\n'), "{line}");
        let decoded: AgentEvent = serde_json::from_str(&line).expect("event round trips");
        assert_eq!(decoded, event);
    }
}

#[test]
fn safe_jsonl_omits_content_bearing_fields() {
    let forbidden = [
        "read README",
        "answer\nwith newline",
        "backend stderr secret",
        "{bad",
        r#"{"name":"read_file","arguments":{"path":"README.MD"}}"#,
        "README contents",
        "tool failed with secret path",
        "done",
    ];

    for event in example_events() {
        let line = event.to_safe_jsonl_line().expect("event serializes");
        assert!(!line.contains('\n'), "{line}");
        for value in forbidden {
            assert!(
                !line.contains(value),
                "safe event leaked content `{value}` in {line}"
            );
        }
        let decoded: SafeAgentEvent = serde_json::from_str(&line).expect("safe event round trips");
        assert_eq!(decoded.kind(), event.kind());
    }
}

#[test]
fn safe_runtime_jsonl_wraps_events_with_fsm_metadata() {
    let event = AgentEvent::ModelRequestFailed {
        turn_id: "turn-1".to_string(),
        request_index: 2,
        message: "backend stderr secret".to_string(),
    };

    let line = event
        .to_safe_runtime_jsonl_line("turn", "fail", 7, "awaiting_model", "failed")
        .expect("event serializes");

    assert!(line.contains(r#""fsm":"turn""#), "{line}");
    assert!(line.contains(r#""transition":"fail""#), "{line}");
    assert!(line.contains(r#""sequence":7"#), "{line}");
    assert!(line.contains(r#""from_phase":"awaiting_model""#), "{line}");
    assert!(line.contains(r#""to_phase":"failed""#), "{line}");
    assert!(line.contains(r#""kind":"model.request_failed""#), "{line}");
    assert!(line.contains(r#""message_bytes":21"#), "{line}");
    assert!(!line.contains("backend stderr secret"), "{line}");

    let decoded: SafeRuntimeEvent =
        serde_json::from_str(&line).expect("safe runtime event round trips");
    assert_eq!(decoded.fsm, "turn");
    assert_eq!(decoded.transition, "fail");
    assert_eq!(decoded.sequence, 7);
    assert_eq!(decoded.from_phase, "awaiting_model");
    assert_eq!(decoded.to_phase, "failed");
    assert_eq!(decoded.event.kind(), "model.request_failed");
}

#[test]
fn safe_jsonl_keeps_argument_shape_without_values() {
    let event = AgentEvent::ToolCallStarted {
        turn_id: "turn-1".to_string(),
        name: "read_file".to_string(),
        arguments: json!({
            "path": "SECRET.md",
            "line": 42
        }),
    };

    let line = event.to_safe_jsonl_line().expect("event serializes");

    assert!(line.contains(r#""keys":["line","path"]"#), "{line}");
    assert!(!line.contains("SECRET.md"), "{line}");
    assert!(!line.contains("42"), "{line}");
}
