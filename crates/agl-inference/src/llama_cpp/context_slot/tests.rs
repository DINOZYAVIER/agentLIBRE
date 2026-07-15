use std::ffi::CStr;

use agl_config::{ModelDialect, ToolCallFormat};
use agl_content::Content;
use agl_oven::{RenderedMessageRole, RenderedTool, RenderedToolCall};
use serde_json::json;

use super::decode::*;
use super::prompt::*;
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
            r#"<tool_call>{"name":"fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#,
        )),
        name: None,
        tool_calls: Vec::new(),
    };
    let canonical = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(
            r#"<tool_call>{"arguments":{"limit_lines":20,"path":"facts.txt"},"name":"fs.read"}</tool_call>"#,
        ),
        name: Some("fs.read".to_string()),
        tool_calls: Vec::new(),
    };
    let mut changed = canonical.clone();
    changed.content = text(
        r#"<tool_call>{"arguments":{"limit_lines":20,"path":"other.txt"},"name":"fs.read"}</tool_call>"#,
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
        r#"<tool_call>{"name":"fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#,
    );
    let repaired = RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: text(
            r#"<tool_call>"{\"name\":\"fs.read\",\"arguments\":{\"path\":\"facts.txt\",\"limit_lines\":20}}"</tool_call>"#,
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
        "system\n{GEMMA_THOUGHT_PREFIX}<|tool_call>call:fs.read{{}}<tool_call|><turn|>\n{GEMMA_THOUGHT_PREFIX}answer"
    );
    let normalized_history = normalize_assistant_context(&exact_history);
    let incoming = format!(
        "system\n<|tool_call>call:fs.read{{}}<tool_call|><turn|>\nanswer<turn|>\n<|turn>user\nnext\n{GEMMA_THOUGHT_PREFIX}"
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
