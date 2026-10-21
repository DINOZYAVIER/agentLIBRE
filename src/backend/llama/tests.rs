use super::*;
use crate::backend::{TOOL_DOCS, ToolCall};

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
    let prompt = Prompt {
        messages: &messages,
        instruction: None,
        tools: &[],
        preserve_thinking: true,
        reasoning: None,
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
    let prompt = Prompt {
        messages: &messages,
        instruction: Some("Answer now."),
        tools: &[],
        preserve_thinking: true,
        reasoning: None,
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
                                its output (the end is kept when it is long).",
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
        ["action", "name", "new", "old", "section", "text", "title"]
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
    let body = |reasoning| {
        apply_template_body(&Prompt {
            messages: &messages,
            instruction: None,
            tools: &[],
            preserve_thinking: true,
            reasoning,
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
