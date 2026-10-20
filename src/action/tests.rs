use serde_json::Value as Json;

use super::*;
use crate::format::tests::qwen;
use crate::grammar::matcher::matches;
use crate::grammar::pattern;
use crate::test_server::with_big_stack;

/// How `format` writes `action`; only test outputs need this.
fn render(format: &Format, action: &Action) -> String {
    let (text, tool, args) = match action {
        Action::Answer(text) => return text.clone(),
        Action::Call { text, tool, args } => (text, tool, args),
    };
    let call = &format.call;
    let mut out = text
        .as_ref()
        .map(|t| format!("{t}\n\n"))
        .unwrap_or_default();
    out += &format!("{}{tool}{}", call.begin_before, call.begin_after);
    for (name, value) in args {
        let value = match value {
            Value::Text(text) => text.clone(),
            Value::Int(n) => n.to_string(),
        };
        out.push_str(&format!(
            "{}{name}{}{value}{}",
            call.param_before, call.param_between, call.param_after
        ));
    }
    out + &call.end
}

/// Checks that the grammar accepts `output` and that it parses as `expected`.
fn check(actions: &Actions, output: &str, expected: &Action) {
    let format = qwen();
    let (grammar, root) = grammar(&format, actions);
    assert!(
        matches(&grammar, &root, output),
        "grammar rejects {output:?}"
    );
    assert_eq!(parse(&format, actions, output).as_ref(), Ok(expected));
}

fn accepts(actions: &Actions, output: &str) -> bool {
    let (grammar, root) = grammar(&qwen(), actions);
    matches(&grammar, &root, output)
}

fn call(tool: &str, args: &[(&str, Value)]) -> Action {
    Action::Call {
        text: None,
        tool: tool.into(),
        args: args
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect(),
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.into())
}

fn builtin(answer: bool) -> Actions {
    Actions {
        tools: builtin_tools(),
        answer,
    }
}

#[test]
fn bash_with_and_without_timeout() {
    let actions = builtin(true);
    let plain = call("bash", &[("command", text("ls -la"))]);
    check(&actions, &render(&qwen(), &plain), &plain);
    let timed = call(
        "bash",
        &[
            ("command", text("cargo test")),
            ("timeout", Value::Int(600)),
        ],
    );
    check(&actions, &render(&qwen(), &timed), &timed);
    let too_long = call(
        "bash",
        &[("command", text("x")), ("timeout", Value::Int(601))],
    );
    assert!(!accepts(&actions, &render(&qwen(), &too_long)));
}

#[test]
fn edit_with_code() {
    let edit = call(
        "edit",
        &[
            ("path", text("src/lib.rs")),
            ("old", text("fn f() -> Vec<&str> {\n    vec![]\n}")),
            ("new", text("")),
        ],
    );
    check(&builtin(false), &render(&qwen(), &edit), &edit);
}

#[test]
fn value_ends_only_at_its_end() {
    let actions = builtin(false);
    let tricky = call("bash", &[("command", text("echo a\n</parameter>b"))]);
    check(&actions, &render(&qwen(), &tricky), &tricky);
    // Followed by the real end, this value would end early.
    let early = call("bash", &[("command", text("echo a\n</parameter>"))]);
    assert!(!accepts(&actions, &render(&qwen(), &early)));
}

#[test]
fn answers() {
    let actions = builtin(true);
    for answer in [
        "Done.",
        "A `Vec<&str>` holds &str.",
        "<tool_call> is Qwen's tag",
    ] {
        check(&actions, answer, &Action::Answer(answer.into()));
    }
    assert!(!accepts(&actions, ""));
    assert!(!accepts(&actions, "Sure: <tool_call>\n<function=bash>\n"));
    let answer_only = Actions {
        tools: Vec::new(),
        answer: true,
    };
    check(&answer_only, "ok", &Action::Answer("ok".into()));
    assert!(!accepts(
        &answer_only,
        &render(&qwen(), &call("bash", &[("command", text("ls"))]))
    ));
}

#[test]
fn constrained_parameters() {
    let verify = pattern::parse("cargo (test|check)( --?[a-z-]+)*").unwrap();
    let actions = Actions {
        tools: vec![
            Tool {
                name: "bash".into(),
                params: vec![Param {
                    name: "command".into(),
                    kind: Kind::Text,
                    required: true,
                    constraint: Constraint::Pattern(verify),
                }],
            },
            Tool {
                name: "read".into(),
                params: vec![
                    Param {
                        name: "path".into(),
                        kind: Kind::Text,
                        required: true,
                        constraint: Constraint::OneOf(vec![
                            "src/main.rs".into(),
                            "src/main.rs.bak".into(),
                        ]),
                    },
                    Param {
                        name: "offset".into(),
                        kind: Kind::Int,
                        required: false,
                        constraint: Constraint::Int { min: -5, max: 50 },
                    },
                ],
            },
        ],
        answer: false,
    };
    let test = call("bash", &[("command", text("cargo test --release"))]);
    check(&actions, &render(&qwen(), &test), &test);
    assert!(!accepts(
        &actions,
        &render(&qwen(), &call("bash", &[("command", text("ls"))]))
    ));
    let read = call(
        "read",
        &[
            ("path", text("src/main.rs.bak")),
            ("offset", Value::Int(-5)),
        ],
    );
    check(&actions, &render(&qwen(), &read), &read);
    let read = call("read", &[("path", text("src/main.rs"))]);
    check(&actions, &render(&qwen(), &read), &read);
    assert!(!accepts(&actions, "Done."));
}

#[test]
fn cut_off_calls_are_incomplete() {
    let actions = builtin(false);
    let output = render(
        &qwen(),
        &call(
            "bash",
            &[("command", text("ls")), ("timeout", Value::Int(30))],
        ),
    );
    for (end, _) in output.char_indices() {
        assert_eq!(
            parse(&qwen(), &actions, &output[..end]),
            Err(ParseError::Incomplete),
            "{:?}",
            &output[..end]
        );
    }
    // Text that never reached a call was cut off too (D114).
    assert_eq!(
        parse(&qwen(), &actions, "hello"),
        Err(ParseError::Incomplete)
    );
    assert_eq!(
        parse(&qwen(), &actions, "hi <tool_call>\n<function=nope>\n"),
        Err(ParseError::Unexpected { offset: 25 })
    );
    let with_answer = builtin(true);
    assert_eq!(
        parse(&qwen(), &with_answer, &output[..30]),
        Err(ParseError::Incomplete)
    );
}

#[test]
fn rendered_grammar() {
    let actions = Actions {
        tools: vec![Tool {
            name: "edit".into(),
            params: vec![Param {
                name: "path".into(),
                kind: Kind::Text,
                required: true,
                constraint: Constraint::OneOf(vec!["a.rs".into()]),
            }],
        }],
        answer: false,
    };
    let (grammar, root) = grammar(&qwen(), &actions);
    let rendered = grammar.render(&root);
    assert!(
        rendered.starts_with("root ::= before-2-0 call-1\n"),
        "{rendered}"
    );
    assert!(rendered.contains(
        "call-1 ::= \"<tool_call>\\n<function=edit>\\n\" \"<parameter=path>\\n\" \"a.rs\" \"\\n</parameter>\\n\" \"</function>\\n</tool_call>\"\n"
    ));
}

#[test]
fn text_before_a_call() {
    let with_answer = builtin(true);
    let output = format!(
        "The split yields one empty item, so I will filter it.\n\n{}",
        render(&qwen(), &call("bash", &[("command", text("cargo test"))]))
    );
    let expected = Action::Call {
        text: Some("The split yields one empty item, so I will filter it.".into()),
        tool: "bash".into(),
        args: vec![("command".into(), text("cargo test"))],
    };
    check(&with_answer, &output, &expected);
    // The same text alone is an answer only where answers are allowed.
    let calls_only = builtin(false);
    check(&calls_only, &output, &expected);
    assert!(!accepts(&calls_only, "Just text."));
    assert!(accepts(&with_answer, "Just text."));
    // A fake call after text cannot sneak through the answer (B9).
    assert!(!accepts(
        &with_answer,
        "Fixing:\n<tool_call>\n<function=edit>\n"
    ));
}

/// The tools of experiments B2-B4.
fn experiment_actions() -> Actions {
    let param = |name: &str, kind, required| Param {
        name: name.into(),
        kind,
        required,
        constraint: match kind {
            Kind::Text => Constraint::Text,
            Kind::Int => Constraint::Int {
                min: 0,
                max: 100_000,
            },
        },
    };
    let tool = |name: &str, params| Tool {
        name: name.into(),
        params,
    };
    Actions {
        tools: vec![
            tool("bash", vec![param("command", Kind::Text, true)]),
            tool(
                "read",
                vec![
                    param("path", Kind::Text, true),
                    param("offset", Kind::Int, false),
                    param("limit", Kind::Int, false),
                ],
            ),
            tool(
                "edit",
                vec![
                    param("path", Kind::Text, true),
                    param("old", Kind::Text, true),
                    param("new", Kind::Text, true),
                ],
            ),
            tool("answer", vec![param("text", Kind::Text, true)]),
        ],
        answer: true,
    }
}

/// An output recorded in B2-B4.
enum Recorded {
    /// An action as the probe parsed it (D76).
    Action(Action),
    /// A raw call cut off by the token limit.
    CutOff(String),
}

/// The outputs recorded in B2-B4.
fn recorded_outputs() -> Vec<Recorded> {
    let rows = |text: &str| -> Vec<Json> {
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let args = |tool: &str, args: &Json| {
        let order: &[&str] = match tool {
            "read" => &["path", "offset", "limit"],
            _ => &["command"],
        };
        let args = order
            .iter()
            .filter_map(|&name| {
                let value = args.get(name)?.as_str()?;
                let value = match name {
                    "offset" | "limit" => Value::Int(value.parse().unwrap()),
                    _ => text(value),
                };
                Some((name.to_string(), value))
            })
            .collect();
        Recorded::Action(Action::Call {
            text: None,
            tool: tool.into(),
            args,
        })
    };
    let mut actions = Vec::new();
    for row in rows(include_str!("fixtures/b2-path-b.jsonl")) {
        actions.push(match &row["call"] {
            Json::Array(call) => args(call[0].as_str().unwrap(), &call[1]),
            _ => Recorded::Action(Action::Answer(row["text"].as_str().unwrap().into())),
        });
    }
    let b3 = include_str!("fixtures/b3-state-instruction.jsonl");
    let b4 = include_str!("fixtures/b4-answer-form.jsonl");
    for row in rows(b3).into_iter().chain(rows(b4)) {
        let answer = || row["answer"].as_str().unwrap().to_owned();
        let cut_off = row["stop"] == "limit" && answer().starts_with("<tool_call>");
        actions.push(match row["action"].as_str().unwrap() {
            "text" if cut_off => Recorded::CutOff(answer()),
            "text" => Recorded::Action(Action::Answer(answer())),
            "answer" => Recorded::Action(call("answer", &[("text", text(&answer()))])),
            tool => args(tool, &row["args"]),
        });
    }
    actions
}

#[test]
fn recorded_experiment_actions() {
    with_big_stack(|| {
        let format = qwen();
        let actions = experiment_actions();
        let recorded = recorded_outputs();
        assert_eq!(recorded.len(), 60 + 110 + 80);
        let mut cut_off = 0;
        for output in &recorded {
            match output {
                Recorded::Action(action) => check(&actions, &render(&format, action), action),
                Recorded::CutOff(text) => {
                    assert_eq!(parse(&format, &actions, text), Err(ParseError::Incomplete));
                    cut_off += 1;
                }
            }
        }
        assert_eq!(cut_off, 1);
    });
}

#[test]
fn a_tool_with_optional_parameters_is_in_the_order_the_model_reads_them() {
    // The template shows the parameters ordered by key, and the grammar
    // lets an optional parameter be skipped: a call in another order
    // than the model reads loses the parameter that comes "too late".
    for tool in builtin_tools() {
        let names: Vec<&str> = tool.params.iter().map(|p| p.name.as_str()).collect();
        let doc = crate::backend::TOOL_DOCS
            .iter()
            .find(|doc| doc.name == tool.name)
            .expect("every tool has its doc");
        let documented: Vec<&str> = doc.params.iter().map(|p| p.0).collect();
        assert_eq!(names, documented, "{}", tool.name);
        if tool.params.iter().any(|p| !p.required) {
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "{}", tool.name);
        }
    }
}
