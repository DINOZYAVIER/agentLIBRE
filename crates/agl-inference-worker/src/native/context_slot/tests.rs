use std::ffi::CStr;

use agl_actions::ParsedModelOutput;
use agl_config::{ModelDialect, ToolCallFormat};
use agl_content::Content;
use agl_oven::{RenderedMessageRole, RenderedTool, RenderedToolCall};
use serde_json::json;

use super::decode::*;
use super::mtp::committed_verified_prefix;
use super::prompt::*;
use super::sampler::grammar_trigger_inputs;
use super::*;

fn text(value: impl Into<String>) -> Option<Content> {
    Some(Content::text(value).unwrap())
}

#[test]
fn context_caches_keep_conversation_state_isolated() {
    let mut first = ContextCache {
        cache_matches_transcript: true,
        ..ContextCache::default()
    };
    let second = ContextCache {
        cache_matches_transcript: true,
        ..ContextCache::default()
    };

    first.messages.push(RenderedMessage {
        role: RenderedMessageRole::User,
        content: text("first conversation"),
        name: None,
        tool_calls: Vec::new(),
    });
    first.token_history.extend([11, 12, 13]);
    first.formatted_history.push_str("first conversation");
    first.rendered_message_history_len = 1;

    assert!(second.messages.is_empty());
    assert!(second.token_history.is_empty());
    assert!(second.formatted_history.is_empty());
    assert_eq!(second.rendered_message_history_len, 0);
    assert!(second.cache_matches_transcript);
}

#[test]
fn rendered_message_content_serializes_tool_calls_without_text() {
    let message = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: None,
        name: None,
        tool_calls: vec![RenderedToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.md"}),
        }],
    };

    let content = rendered_message_content(&message).unwrap();

    assert!(content.contains("\"name\":\"read_file\""));
    assert!(content.contains("\"path\":\"README.md\""));
}

#[test]
fn rendered_message_content_keeps_canonical_text_when_tool_calls_are_structured() {
    let message = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text("<|tool_call>call:screen.capture{}<tool_call|>"),
        name: Some("screen.capture".to_string()),
        tool_calls: vec![RenderedToolCall {
            name: "screen.capture".to_string(),
            arguments: json!({}),
        }],
    };

    let content = rendered_message_content(&message).unwrap();

    assert_eq!(content, "<|tool_call>call:screen.capture{}<tool_call|>");
}

#[test]
fn rendered_history_matches_only_isolated_semantic_tool_calls() {
    let recorded = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(format!(
            "{DISABLED_THINKING_PREFILL}{}",
            r#"<tool_call>{"name":"core.workspace:fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#,
        )),
        name: None,
        tool_calls: Vec::new(),
    };
    let canonical = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(
            r#"<tool_call>{"arguments":{"limit_lines":20,"path":"facts.txt"},"name":"core.workspace:fs.read"}</tool_call>"#,
        ),
        name: Some("core.workspace:fs.read".to_string()),
        tool_calls: Vec::new(),
    };
    let mut changed = canonical.clone();
    changed.content = text(
        r#"<tool_call>{"arguments":{"limit_lines":20,"path":"other.txt"},"name":"core.workspace:fs.read"}</tool_call>"#,
    );
    let mut with_prose = canonical.clone();
    with_prose.content = text(format!(
        "calling now\n{}",
        rendered_message_content(&canonical).unwrap()
    ));
    let mut user_call = canonical.clone();
    user_call.role = RenderedMessageRole::User;
    let mut user_call_reordered = user_call.clone();
    user_call_reordered.content = text(
        r#"<tool_call>{"name":"core.workspace:fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#,
    );
    let repaired = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(
            r#"<tool_call>"{\"name\":\"core.workspace:fs.read\",\"arguments\":{\"path\":\"facts.txt\",\"limit_lines\":20}}"</tool_call>"#,
        ),
        name: None,
        tool_calls: Vec::new(),
    };
    let mut same_prefill = canonical.clone();
    same_prefill.content = text(format!(
        "{DISABLED_THINKING_PREFILL}{}",
        rendered_message_content(&canonical).unwrap()
    ));

    assert!(rendered_history_is_prefix(
        std::slice::from_ref(&recorded),
        std::slice::from_ref(&canonical),
        1,
    ));
    assert!(!rendered_history_is_prefix(
        std::slice::from_ref(&recorded),
        std::slice::from_ref(&changed),
        1,
    ));
    assert!(rendered_history_is_prefix(
        std::slice::from_ref(&same_prefill),
        std::slice::from_ref(&same_prefill),
        1,
    ));
    assert!(rendered_history_is_prefix(&[repaired], &[canonical], 1));
    assert!(!rendered_history_is_prefix(
        &[user_call],
        &[user_call_reordered],
        1,
    ));
    assert!(!rendered_history_is_prefix(&[recorded], &[with_prose], 1));
}

#[test]
fn cached_gemma_history_restores_structured_tool_calls_from_the_transcript() {
    let native_call = "<|tool_call>call:core.process:process.exec{args:[<|\"|>ok<|\"|>],program:<|\"|>/usr/bin/printf<|\"|>}<tool_call|>";
    let mut cached = vec![RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(native_call),
        name: None,
        tool_calls: Vec::new(),
    }];
    let incoming = vec![RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(native_call),
        name: Some("core.process:process.exec".to_string()),
        tool_calls: vec![RenderedToolCall {
            name: "core.process:process.exec".to_string(),
            arguments: json!({
                "args": ["ok"],
                "program": "/usr/bin/printf",
            }),
        }],
    }];

    restore_cached_gemma_tool_calls(&mut cached, &incoming, 1);

    assert_eq!(cached[0].name.as_deref(), Some("core.process:process.exec"));
    assert_eq!(cached[0].tool_calls, incoming[0].tool_calls);
    assert_eq!(cached[0].content, text(native_call));
}

#[test]
fn stop_marker_truncates_generated_user_continuation() {
    let mut content = "hello\n\nUser:\nnext".to_string();

    assert!(trim_generated_continuation(&mut content));
    assert_eq!(content, "hello\n");
}

#[test]
fn stop_marker_truncates_generated_assistant_continuation() {
    let mut content = "hello\nAssistant:\nnext".to_string();

    assert!(trim_generated_continuation(&mut content));
    assert_eq!(content, "hello");
}

#[test]
fn stop_marker_truncates_generated_tool_continuation() {
    let mut content = "hello\nTool:\nnext".to_string();

    assert!(trim_generated_continuation(&mut content));
    assert_eq!(content, "hello");
}

#[test]
fn disables_qwen_thinking_prefill() {
    let mut prompt =
        "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n<think>\n".to_string();

    assert_eq!(
        disable_qwen_thinking(&mut prompt),
        Some(DISABLED_THINKING_PREFILL)
    );
    assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
}

#[test]
fn disables_qwen_thinking_after_plain_assistant_header() {
    let mut prompt = "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n".to_string();

    assert_eq!(
        disable_qwen_thinking(&mut prompt),
        Some(DISABLED_THINKING_PREFILL)
    );
    assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
}

#[test]
fn normalizes_qwen_thinking_and_maps_the_append_boundary() {
    let exact_history = format!(
        "system\n{QWEN_DISABLED_THINKING_PREFIX}tool call<|im_end|>\n{QWEN_DISABLED_THINKING_PREFIX}answer"
    );
    let normalized_history = normalize_assistant_context(&exact_history);
    let incoming = format!(
        "system\n{QWEN_ASSISTANT_HEADER}tool call<|im_end|>\n{QWEN_ASSISTANT_HEADER}answer<|im_end|>\nuser\n{QWEN_DISABLED_THINKING_PREFIX}"
    );
    let normalized_incoming = normalize_assistant_context(&incoming);

    assert!(normalized_incoming.starts_with(&normalized_history));
    let boundary =
        source_index_after_normalized_prefix(&incoming, normalized_history.len()).unwrap();
    assert_eq!(
        &incoming[boundary..],
        format!("<|im_end|>\nuser\n{QWEN_DISABLED_THINKING_PREFIX}")
    );
}

#[test]
fn normalizes_gemma_thought_channel_and_maps_the_append_boundary() {
    let exact_history = format!(
        "system\n{GEMMA_THOUGHT_PREFIX}<|tool_call>call:core.workspace:fs.read{{}}<tool_call|><turn|>\n{GEMMA_THOUGHT_PREFIX}answer"
    );
    let normalized_history = normalize_assistant_context(&exact_history);
    let incoming = format!(
        "system\n<|tool_call>call:core.workspace:fs.read{{}}<tool_call|><turn|>\nanswer<turn|>\n<|turn>user\nnext\n{GEMMA_THOUGHT_PREFIX}"
    );
    let normalized_incoming = normalize_assistant_context(&incoming);

    assert!(normalized_incoming.starts_with(&normalized_history));
    let boundary =
        source_index_after_normalized_prefix(&incoming, normalized_history.len()).unwrap();
    assert_eq!(
        &incoming[boundary..],
        format!("<turn|>\n<|turn>user\nnext\n{GEMMA_THOUGHT_PREFIX}")
    );
}

#[test]
fn strips_generated_gemma_thought_channel_from_public_content() {
    let mut content = format!("{GEMMA_THOUGHT_CHANNEL_PREFIX}G4V-7Q4M-9281");

    strip_generated_assistant_prefix(&mut content);

    assert_eq!(content, "G4V-7Q4M-9281");
}

#[test]
fn normalizes_standalone_generated_gemma_thought_channel() {
    let source = format!("prompt{GEMMA_THOUGHT_CHANNEL_PREFIX}answer");
    let normalized = normalize_assistant_context(&source);

    assert_eq!(normalized, "promptanswer");
    assert_eq!(
        source_index_after_normalized_prefix(&source, normalized.len()),
        Some(source.len())
    );
}

#[test]
fn prepared_chat_messages_use_llama_roles() {
    let rendered = RenderedModelRequest {
        run_id: agl_ids::RunId::generate(),
        turn_id: agl_ids::TurnId::generate(),
        request_index: 0,
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
        messages: vec![
            RenderedMessage {
                role: RenderedMessageRole::System,
                content: text("demo system"),
                name: None,
                tool_calls: Vec::new(),
            },
            RenderedMessage {
                role: RenderedMessageRole::User,
                content: text("hello"),
                name: None,
                tool_calls: Vec::new(),
            },
        ],
        tools: vec![RenderedTool {
            name: "unused".to_string(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }],
    };

    let prepared =
        PreparedChatMessages::new(&rendered.messages, rendered.tool_call_format).unwrap();

    assert_eq!(prepared.messages.len(), 2);
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[0].role) }
            .to_str()
            .unwrap(),
        "system"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[0].content) }
            .to_str()
            .unwrap(),
        "demo system"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[1].role) }
            .to_str()
            .unwrap(),
        "user"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[1].content) }
            .to_str()
            .unwrap(),
        "hello"
    );
}

#[test]
fn prepared_gemma_messages_preserve_tool_call_and_observation_fields() {
    let messages = vec![
        RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: text("<|tool_call>call:screen.capture{}<tool_call|>"),
            name: Some("screen.capture".to_string()),
            tool_calls: vec![RenderedToolCall {
                name: "screen.capture".to_string(),
                arguments: json!({}),
            }],
        },
        RenderedMessage {
            role: RenderedMessageRole::Tool,
            content: text(r#"{"status":"ok"}<__media__>"#),
            name: Some("screen.capture".to_string()),
            tool_calls: Vec::new(),
        },
    ];

    let prepared = PreparedChatMessages::new(&messages, ToolCallFormat::GemmaFunctionCall).unwrap();

    assert_eq!(prepared.messages.len(), 2);
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[0].content) }
            .to_str()
            .unwrap(),
        ""
    );
    assert_eq!(prepared.messages[0].n_tool_calls, 1);
    let prepared_tool_call = unsafe { &*prepared.messages[0].tool_calls };
    assert_eq!(
        unsafe { CStr::from_ptr(prepared_tool_call.name) }
            .to_str()
            .unwrap(),
        "screen.capture"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(prepared_tool_call.arguments) }
            .to_str()
            .unwrap(),
        "{}"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[1].name) }
            .to_str()
            .unwrap(),
        "screen.capture"
    );
    assert_eq!(
        unsafe { CStr::from_ptr(prepared.messages[1].content) }
            .to_str()
            .unwrap(),
        r#"{"status":"ok"}<__media__>"#
    );
}

#[test]
fn prefill_chunk_count_splits_prompt_by_batch_size() {
    assert_eq!(prefill_chunk_count(0, 1024).unwrap(), 0);
    assert_eq!(prefill_chunk_count(1, 1024).unwrap(), 1);
    assert_eq!(prefill_chunk_count(1024, 1024).unwrap(), 1);
    assert_eq!(prefill_chunk_count(1025, 1024).unwrap(), 2);
    assert_eq!(prefill_chunk_count(4096, 1024).unwrap(), 4);
}

#[test]
fn prefill_chunk_count_rejects_zero_batch_size() {
    let err = prefill_chunk_count(1, 0).unwrap_err();

    assert!(
        format!("{err:#}").contains("llama.cpp prefill batch size cannot be zero"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn tool_schema_bridge_keeps_nested_schema_bytes_exact() {
    let schema = json!({
        "type": "object",
        "properties": {
            "request": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        },
        "required": ["request"],
        "additionalProperties": false
    });
    let prepared = PreparedTools::new(&[RenderedTool {
        name: "core.workspace:fs.read".to_string(),
        description: "Read a file".to_string(),
        input_schema: schema.clone(),
    }])
    .unwrap();
    let parameters = unsafe { CStr::from_ptr(prepared.tools[0].parameters) }
        .to_str()
        .unwrap();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(parameters).unwrap(),
        schema
    );
}

#[test]
fn tool_schema_bridge_inlines_local_definitions_for_the_model_boundary() {
    let schema = json!({
        "$defs": {
            "PatchEdit": {
                "type": "object",
                "properties": {
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"}
                },
                "required": ["old_text", "new_text"],
                "additionalProperties": false
            },
            "Operation": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "op": {"const": "create"},
                            "path": {"type": "string"},
                            "content": {"type": "string"},
                            "expected_absent": {"type": "boolean"}
                        },
                        "required": ["op", "path", "content", "expected_absent"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "op": {"const": "update"},
                            "path": {"type": "string"},
                            "expected_digest": {"type": "string"},
                            "edits": {
                                "type": "array",
                                "items": {"$ref": "#/$defs/PatchEdit"}
                            }
                        },
                        "required": ["op", "path", "expected_digest", "edits"],
                        "additionalProperties": false
                    }
                ]
            }
        },
        "type": "object",
        "properties": {
            "operations": {
                "type": "array",
                "items": {"$ref": "#/$defs/Operation"}
            }
        },
        "required": ["operations"],
        "additionalProperties": false
    });
    let prepared = PreparedTools::new(&[RenderedTool {
        name: "core.workspace:fs.apply_patch".to_string(),
        description: "Apply a patch".to_string(),
        input_schema: schema,
    }])
    .unwrap();
    let parameters = unsafe { CStr::from_ptr(prepared.tools[0].parameters) }
        .to_str()
        .unwrap();
    let projected = serde_json::from_str::<serde_json::Value>(parameters).unwrap();

    assert!(projected.get("$defs").is_none());
    assert!(
        projected["properties"]["operations"]["items"]
            .get("$ref")
            .is_none()
    );
    assert_eq!(
        projected["properties"]["operations"]["items"]["oneOf"][0]["required"],
        json!(["op", "path", "content", "expected_absent"])
    );
    let update = &projected["properties"]["operations"]["items"]["oneOf"][1]["properties"]["edits"];
    assert!(update["items"].get("$ref").is_none());
    assert_eq!(update["items"]["required"], json!(["old_text", "new_text"]));
    assert_eq!(update["items"]["additionalProperties"], json!(false));
}

#[test]
fn tool_schema_bridge_rejects_cyclic_local_references() {
    let error = match PreparedTools::new(&[RenderedTool {
        name: "bad".to_string(),
        description: "bad schema".to_string(),
        input_schema: json!({
            "$defs": {
                "Loop": {"$ref": "#/$defs/Loop"}
            },
            "type": "object",
            "properties": {
                "loop": {"$ref": "#/$defs/Loop"}
            },
            "additionalProperties": false
        }),
    }]) {
        Ok(_) => panic!("cyclic Tool schema was admitted"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("cyclic local schema reference"));
}

#[test]
fn tool_schema_bridge_bounds_projected_schema_nodes() {
    let error = project_tool_schema(&json!({
        "type": "object",
        "enum": vec![true; MAX_PROJECTED_TOOL_SCHEMA_NODES]
    }))
    .unwrap_err();

    assert!(error.contains("projected Tool schema exceeds"));
    assert!(error.contains("nodes"));
}

#[test]
fn tool_schema_bridge_bounds_projected_schema_bytes() {
    let error = match PreparedTools::new(&[RenderedTool {
        name: "bad".to_string(),
        description: "oversized schema".to_string(),
        input_schema: json!({
            "type": "object",
            "description": "x".repeat(MAX_PROJECTED_TOOL_SCHEMA_BYTES)
        }),
    }]) {
        Ok(_) => panic!("oversized Tool schema was admitted"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("projected Tool schema exceeds"));
    assert!(error.to_string().contains("bytes"));
}

#[test]
fn tool_schema_bridge_rejects_non_object_schema_roots() {
    let error = match PreparedTools::new(&[RenderedTool {
        name: "bad".to_string(),
        description: "bad schema".to_string(),
        input_schema: json!(true),
    }]) {
        Ok(_) => panic!("non-object Tool schema was admitted"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("root must be an object"));
}

#[test]
fn generation_plan_boundary_rejects_length_mismatch() {
    let value = std::ffi::CString::new("prompt").unwrap();

    assert!(
        raw_string(value.as_ptr(), 99, "prompt")
            .unwrap_err()
            .to_string()
            .contains("length mismatch")
    );
}

#[test]
fn lazy_grammar_triggers_translate_common_metadata() {
    let plan = GenerationPlan {
        prompt: "prompt".to_string(),
        grammar: "root ::= \"ok\"".to_string(),
        grammar_lazy: true,
        grammar_needs_prefill: false,
        grammar_triggers: vec![
            GrammarTrigger {
                kind: 0,
                value: String::new(),
                token: 17,
            },
            GrammarTrigger {
                kind: 1,
                value: "<tool.call>".to_string(),
                token: -1,
            },
            GrammarTrigger {
                kind: 2,
                value: "call:[a-z]+".to_string(),
                token: -1,
            },
            GrammarTrigger {
                kind: 3,
                value: "full".to_string(),
                token: -1,
            },
        ],
        grammar_prefill_tokens: Vec::new(),
        additional_stops: vec!["<stop>".to_string()],
        preserved_tokens: Vec::new(),
        generation_prompt: String::new(),
        format: 0,
        parser: String::new(),
    };
    let (patterns, tokens) = grammar_trigger_inputs(&plan).unwrap();
    let patterns = patterns
        .iter()
        .map(|pattern| pattern.to_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(tokens, vec![17]);
    assert_eq!(patterns, vec![r"<tool\.call>", "call:[a-z]+", "^full$"]);
}

#[test]
fn malformed_lazy_grammar_trigger_is_typed() {
    let plan = GenerationPlan {
        prompt: "prompt".to_string(),
        grammar: "root ::= \"ok\"".to_string(),
        grammar_lazy: true,
        grammar_needs_prefill: false,
        grammar_triggers: vec![GrammarTrigger {
            kind: 99,
            value: "bad".to_string(),
            token: -1,
        }],
        grammar_prefill_tokens: Vec::new(),
        additional_stops: Vec::new(),
        preserved_tokens: Vec::new(),
        generation_prompt: String::new(),
        format: 0,
        parser: String::new(),
    };

    assert!(
        grammar_trigger_inputs(&plan)
            .unwrap_err()
            .to_string()
            .contains("trigger type 99")
    );
}

#[test]
fn rejected_mtp_tail_is_not_part_of_the_committed_sequence() {
    let draft = [11, 12, 13];

    assert_eq!(
        committed_verified_prefix(&draft, &[10, 11, 99, 13]),
        vec![10, 11]
    );
    assert_eq!(
        committed_verified_prefix(&draft, &[10, 11, 12, 13]),
        vec![10, 11, 12, 13]
    );
}

#[test]
fn constrained_output_corpus_never_enters_repair() {
    let corpus = [
        "plain answer",
        r#"<tool_call>{"name":"core.workspace:fs.read","arguments":{"path":"README.md"}}</tool_call>"#,
        r#"<tool_call>{"name":"nested","arguments":{"request":{"path":"README.md","flags":["a","b"]}}}</tool_call>"#,
        r#"<|tool_call>call:screen.capture{}<tool_call|>"#,
    ];

    for output in corpus {
        assert!(
            !matches!(
                agl_actions::parse_model_output(output),
                ParsedModelOutput::MalformedToolCall(_)
            ),
            "constrained corpus output entered repair: {output}"
        );
    }
}
