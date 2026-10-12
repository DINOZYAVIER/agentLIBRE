//! Generates actions with a live model under state grammars and parses
//! them (D76); see [`crate::test_server`].

use serde_json::{Value as Json, json};

use super::*;
use crate::format::tests::qwen;
use crate::grammar::pattern;
use crate::test_server;

/// Tool descriptions the chat template lists in the system prompt (D14).
fn tool_schemas() -> Json {
    let function = |name: &str, description: &str, properties: Json, required: Json| {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": {"type": "object", "properties": properties, "required": required},
            },
        })
    };
    json!([
        function(
            "bash",
            "Run a shell command in the workspace and return its output.",
            json!({
                "command": {"type": "string"},
                "timeout": {"type": "integer", "description": "Seconds, 1-600; default 60."},
            }),
            json!(["command"]),
        ),
        function(
            "edit",
            "Replace the only occurrence of `old` in the file at `path` with `new`.",
            json!({
                "path": {"type": "string"},
                "old": {"type": "string"},
                "new": {"type": "string"},
            }),
            json!(["path", "old", "new"]),
        ),
    ])
}

/// Renders `messages` with the model's chat template, without thinking.
fn prompt(messages: &[Json]) -> String {
    let (status, body) = test_server::post(
        "/apply-template",
        &json!({
            "messages": messages,
            "tools": tool_schemas(),
            "chat_template_kwargs": {"enable_thinking": false},
        }),
    );
    assert_eq!(status, 200, "{body}");
    let body: Json = serde_json::from_str(&body).unwrap();
    body["prompt"].as_str().unwrap().to_owned()
}

/// Generates and parses one action; `None` when the token limit cut it.
fn act(actions: &Actions, messages: &[Json]) -> Option<Action> {
    let format = qwen();
    let (grammar, root) = grammar(&format, actions);
    let (status, body) = test_server::post(
        "/completion",
        &json!({
            "prompt": prompt(messages),
            "grammar": grammar.render(&root),
            "n_predict": 400,
            "temperature": 1.0,
            "top_p": 0.95,
            "top_k": 20,
        }),
    );
    assert_eq!(status, 200, "{body}");
    let body: Json = serde_json::from_str(&body).unwrap();
    let output = body["content"].as_str().unwrap();
    eprintln!("{output:?}");
    match parse(&format, actions, output) {
        Ok(action) => Some(action),
        Err(ParseError::Incomplete) if body["stop_type"] == "limit" => None,
        Err(error) => panic!("{error:?} in {output:?}"),
    }
}

fn system() -> Json {
    json!({"role": "system", "content": "You are a coding agent working in a Rust repository."})
}

fn user(text: &str) -> Json {
    json!({"role": "user", "content": text})
}

fn tool_name(action: &Option<Action>) -> Option<&str> {
    match action {
        Some(Action::Call { tool, .. }) => Some(tool),
        _ => None,
    }
}

#[test]
#[ignore = "needs a llama-server at AGL_TEST_SERVER"]
fn live_actions() {
    test_server::with_big_stack(check_actions);
}

fn check_actions() {
    let all = Actions {
        tools: builtin_tools(),
        answer: true,
    };
    let listing = act(
        &all,
        &[system(), user("List the files in the current directory.")],
    );
    assert_eq!(tool_name(&listing), Some("bash"));

    let timed = act(
        &all,
        &[
            system(),
            user("Run `sleep 5` with the bash tool and a timeout of 30 seconds."),
        ],
    );
    assert_eq!(tool_name(&timed), Some("bash"));

    let edit = act(
        &all,
        &[
            system(),
            user(
                "In src/lib.rs, replace `fn parse(input: &str) -> Vec<&str> {` with \
                 `pub fn parse(input: &str) -> Vec<&str> {` using the edit tool.",
            ),
        ],
    );
    if let Some(Action::Call { tool, args, .. }) = &edit
        && tool == "edit"
    {
        assert_eq!(args[0], ("path".into(), Value::Text("src/lib.rs".into())));
    }

    let mut verify = builtin_tools();
    verify.truncate(1);
    verify[0].params[0].constraint =
        Constraint::Pattern(pattern::parse("cargo (test|check)( --?[a-z-]+)*").unwrap());
    let verify = Actions {
        tools: verify,
        answer: false,
    };
    let instruction = "Only verification is available now: cargo test or cargo check with flags.";
    let checked = act(
        &verify,
        &[
            system(),
            user("Fix the failing test and verify it."),
            user(instruction),
        ],
    );
    // The model may keep adding flags until the token limit (B3, D18).
    assert!(checked.is_none() || tool_name(&checked) == Some("bash"));

    let answer_only = Actions {
        tools: Vec::new(),
        answer: true,
    };
    let answered = act(
        &answer_only,
        &[
            system(),
            user("Run `cargo test` now using the bash tool."),
            user("Tools are not available now. Answer the user in text."),
        ],
    );
    assert!(!matches!(answered, Some(Action::Call { .. })));
}
