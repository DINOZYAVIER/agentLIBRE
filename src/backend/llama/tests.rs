use super::*;
use crate::backend::{TOOL_DOCS, ToolCall};
use crate::format::tests::{gemma, qwen};

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: vec![
            ("command".into(), Value::Text("ls \"a b\"".into())),
            ("timeout".into(), Value::Int(30)),
        ],
    }
}

#[test]
fn a_runtime_message_is_a_prefixed_user_message() {
    let messages = [
        Message::System("s".into()),
        Message::Runtime("It was cut off.".into()),
    ];
    let format = qwen();
    let prompt = Prompt {
        messages: &messages,
        instruction: None,
        tools: &[],
        preserve_thinking: true,
        reasoning: None,
        format: &format,
    };
    assert_eq!(
        Json::Array(messages_json(&prompt)),
        json!([
            {"role": "system", "content": "s"},
            {"role": "user", "content": "[agentLIBRE runtime] It was cut off."},
        ])
    );
}

#[test]
fn messages_in_template_form() {
    let messages = [
        Message::System("s".into()),
        Message::User("u".into()),
        Message::Assistant {
            thinking: Some("hm".into()),
            text: None,
            call: Some(call("c1")),
        },
        Message::Tool {
            id: "c1".into(),
            content: "out".into(),
        },
        Message::Assistant {
            thinking: None,
            text: Some("done".into()),
            call: None,
        },
    ];
    let format = qwen();
    let prompt = Prompt {
        messages: &messages,
        instruction: Some("Answer now."),
        tools: &[],
        preserve_thinking: true,
        reasoning: None,
        format: &format,
    };
    assert_eq!(
        Json::Array(messages_json(&prompt)),
        json!([
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            {
                "role": "assistant",
                "content": "",
                "reasoning_content": "hm",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\":\"ls \\\"a b\\\"\",\"timeout\":30}"},
                }],
            },
            {"role": "tool", "tool_call_id": "c1", "content": "out"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "Answer now."},
        ])
    );
}

#[test]
fn tool_schemas() {
    assert_eq!(
        tool_json(&TOOL_DOCS[0]),
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command with `bash -c` in the workspace and return \
                                its output. A long output shows its start and its end and \
                                names the file that holds all of it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "integer", "description": "Seconds; default 60."},
                    },
                    "required": ["command"],
                },
            },
        })
    );
    let edit = tool_json(&TOOL_DOCS[1]);
    assert_eq!(
        edit["function"]["parameters"]["required"],
        json!(["path", "old", "new"])
    );
    let forge = tool_json(&TOOL_DOCS[2]);
    assert_eq!(forge["function"]["name"], "forge");
    let properties = &forge["function"]["parameters"]["properties"];
    let mut names = properties
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        ["action", "name", "new", "old", "section", "state", "text", "title"]
    );
    // Every field is a string with a description that says which action
    // it is for.
    for name in &names {
        assert_eq!(properties[name]["type"], "string", "{name}");
        assert!(properties[name]["description"].is_string(), "{name}");
    }
    let description = |name: &str| properties[name]["description"].as_str().unwrap();
    assert!(description("name").contains("not its title"));
    assert!(description("new").contains("empty deletes"));
    assert!(description("section").contains("Not for edit"));
    assert_eq!(
        forge["function"]["parameters"]["required"],
        json!(["action"])
    );
}

#[test]
fn only_set_sampling_is_sent() {
    let mut body = json!({});
    add_sampling(
        &mut body,
        &Sampling {
            temperature: Some(0.6),
            top_k: Some(20),
            ..Sampling::default()
        },
    );
    assert_eq!(body, json!({"temperature": 0.6, "top_k": 20}));
}

#[test]
fn the_reasoning_level_sets_the_template_kwargs() {
    let messages = [Message::System("s".into())];
    let format = qwen();
    let body = |reasoning| {
        apply_template_body(&Prompt {
            messages: &messages,
            instruction: None,
            tools: &[],
            preserve_thinking: true,
            reasoning,
            format: &format,
        })
    };
    // Unset: no `chat_template_kwargs`, the template keeps its default.
    assert_eq!(
        body(None),
        json!({"messages": [{"role": "system", "content": "s"}], "tools": []})
    );
    // `Off` silences the thinking with the template kwarg.
    assert_eq!(
        body(Some(ReasoningLevel::Off))["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    // A level sets the template's `reasoning_effort`.
    for (level, effort) in [
        (ReasoningLevel::Low, "low"),
        (ReasoningLevel::Medium, "medium"),
        (ReasoningLevel::Xhigh, "xhigh"),
    ] {
        assert_eq!(
            body(Some(level))["chat_template_kwargs"],
            json!({"reasoning_effort": effort})
        );
    }
}

#[test]
fn unreachable_server() {
    // Port 1 is reserved and refuses connections.
    let llama = Llama::new("http://127.0.0.1:1/");
    assert!(matches!(
        llama.context_size(),
        Err(Error::ServerUnreachable { url, .. }) if url == "http://127.0.0.1:1"
    ));
}

#[test]
fn the_text_before_a_call_stays_out_of_gemma_history() {
    let messages = [
        Message::Assistant {
            thinking: None,
            text: Some("I will look.".into()),
            call: Some(ToolCall {
                id: "c".into(),
                name: "bash".into(),
                args: vec![],
            }),
        },
        Message::Assistant {
            thinking: None,
            text: Some("Done.".into()),
            call: None,
        },
    ];
    let contents = |format: &crate::format::Format| {
        let prompt = Prompt {
            messages: &messages,
            instruction: None,
            tools: &[],
            preserve_thinking: true,
            reasoning: None,
            format,
        };
        messages_json(&prompt)
            .iter()
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(contents(&gemma()), ["", "Done."]);
    assert_eq!(contents(&qwen()), ["I will look.", "Done."]);
}

#[test]
fn a_transcription_names_no_language_and_does_not_think() {
    let body = transcribe_body(b"RIFF");
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["input_audio"]["data"], crate::media::base64(b"RIFF"));
    assert_eq!(content[0]["input_audio"]["format"], "wav");
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
}

#[test]
fn a_transcript_holds_no_mark_of_the_model() {
    assert_eq!(before_its_marks("Words."), "Words.");
    assert_eq!(before_its_marks("<|channel>thought\n<channel|><|channel>thought"), "");
    assert_eq!(before_its_marks("Words.<|channel>thought"), "Words.");
}
