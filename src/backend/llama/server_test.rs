//! Checks the client against a live llama-server; see
//! [`crate::test_server`].

use super::*;
use crate::action::{self, Action, Actions};
use crate::backend::{TOOL_DOCS, ToolCall};
use crate::format::tests::qwen;
use crate::test_server;

fn history() -> Vec<Message> {
    vec![
        Message::System("You are a coding agent working in a Rust repository.".into()),
        Message::User("What is in the current directory?".into()),
        Message::Assistant {
            thinking: Some("EARLIER-THOUGHT".into()),
            text: None,
            call: Some(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: vec![("command".into(), Value::Text("ls".into()))],
            }),
        },
        Message::Tool {
            id: "c1".into(),
            content: "Cargo.toml\nsrc".into(),
        },
        Message::Assistant {
            thinking: Some("EARLIER-ANSWER-THOUGHT".into()),
            text: Some("Cargo.toml and src.".into()),
            call: None,
        },
        Message::User("Show the manifest.".into()),
        Message::Assistant {
            thinking: Some("CURRENT-THOUGHT".into()),
            text: None,
            call: Some(ToolCall {
                id: "c2".into(),
                name: "bash".into(),
                args: vec![("command".into(), Value::Text("cat Cargo.toml".into()))],
            }),
        },
        Message::Tool {
            id: "c2".into(),
            content: "[package]\nname = \"demo\"".into(),
        },
    ]
}

#[test]
#[ignore = "needs a llama-server at AGENTLIBRE_TEST_SERVER"]
fn live_backend() {
    let llama = Llama::new(&test_server::url());
    assert!(llama.context_size().unwrap() >= 4096);
    assert!(llama.count_tokens("Hello, world!").unwrap() > 0);
    let tools: Vec<&ToolDoc> = TOOL_DOCS.iter().collect();

    // preserve_thinking (D98).
    let messages = history();
    let format = qwen();
    let render = |preserve_thinking, instruction| {
        llama
            .render(&Prompt {
                messages: &messages,
                instruction: Some(instruction),
                tools: &tools,
                preserve_thinking,
                reasoning: None,
                format: &format,
            })
            .unwrap()
    };
    let kept = render(true, "Answer the user in text.");
    let dropped = render(false, "Answer the user in text.");
    assert!(kept.contains("EARLIER-THOUGHT") && kept.contains("CURRENT-THOUGHT"));
    assert!(!dropped.contains("EARLIER-THOUGHT"));
    assert!(!dropped.contains("EARLIER-ANSWER-THOUGHT"));
    assert!(dropped.contains("CURRENT-THOUGHT"), "{dropped}");
    assert!(kept.contains("edit") && kept.contains("Seconds; default 60."));

    // Thinking on and off through format strings (D94, D99): with thinking
    // the prompt is the template's own; without, only the end differs, and
    // it matches the template's end for `enable_thinking = false`.
    let simple = [Message::System("s".into()), Message::User("Say hi.".into())];
    let rendered = llama
        .render(&Prompt {
            messages: &simple,
            instruction: None,
            tools: &tools,
            preserve_thinking: true,
            reasoning: None,
            format: &format,
        })
        .unwrap();
    let template = |thinking: bool| {
        let body = json!({
            "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "Say hi."}],
            "tools": tools.iter().map(|doc| tool_json(doc)).collect::<Vec<_>>(),
            "chat_template_kwargs": {"enable_thinking": thinking},
        });
        let (status, body) = test_server::post("/apply-template", &body);
        assert_eq!(status, 200);
        serde_json::from_str::<Json>(&body).unwrap()["prompt"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let on = format.start_step(rendered.clone(), true);
    let off = format.start_step(rendered, false);
    assert_eq!(on, template(true));
    let think = format.think.as_ref().unwrap();
    let shared = on.strip_suffix(think.open.as_str()).unwrap();
    assert_eq!(off, format!("{shared}{}{}", think.open, think.close));
    let tail = &off[off.rfind("<|im_start|>assistant").unwrap()..];
    assert!(template(false).ends_with(tail), "{tail:?}");

    // A step: streamed thinking, then an action under the state grammar.
    let sampling = Sampling {
        temperature: Some(1.0),
        top_p: Some(0.95),
        top_k: Some(20),
        ..Sampling::default()
    };
    let prompt = format.start_step(render(true, "Choose the next action."), true);
    let mut pieces = String::new();
    let mut count = 0;
    let thinking = llama
        .generate(
            &Request {
                prompt: &prompt,
                grammar: None,
                stop: &[think.close.trim(), format.call.begin_before.trim()],
                max_tokens: 1024,
                sampling: &sampling,
                media: &[],
            },
            &mut |piece| {
                pieces.push_str(piece);
                count += 1;
            },
        )
        .unwrap();
    eprintln!(
        "thinking ({count} pieces, {:?}): {:?}",
        thinking.stop, thinking.text
    );
    assert_eq!(pieces, thinking.text);
    assert!(count > 1);
    let actions = Actions {
        tools: action::builtin_tools(),
        answer: true,
    };
    let (grammar, root) = action::grammar(&format, &actions);
    let prompt = format!("{prompt}{}{}", thinking.text.trim_end(), think.close);
    let generated = llama
        .generate(
            &Request {
                prompt: &prompt,
                grammar: Some(&grammar.render(&root)),
                stop: &[],
                max_tokens: 512,
                sampling: &sampling,
                media: &[],
            },
            &mut |_| {},
        )
        .unwrap();
    eprintln!("action ({:?}): {:?}", generated.stop, generated.text);
    if generated.stop == Stop::End {
        let action = action::parse(&format, &actions, &generated.text).unwrap();
        assert!(matches!(action, Action::Call { .. } | Action::Answer(_)));
    }

    // A server error carries its message.
    let error = llama
        .generate(
            &Request {
                prompt: "x",
                grammar: Some("root ::= ("),
                stop: &[],
                max_tokens: 4,
                sampling: &sampling,
                media: &[],
            },
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(&error, Error::ServerStatus { status: 400, message, .. } if message.contains("grammar")),
        "{error:?}"
    );
}
