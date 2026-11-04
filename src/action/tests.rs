use super::*;
use crate::format::tests::{gemma, qwen};
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
    call.write_args(&mut out, args);
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

/// The tool set the corpus below exercises: every parameter kind the model
/// reads — required and optional text, optional ints, a value that holds the
/// call's own tags — plus the `answer` as a tool.
fn corpus_tools() -> Actions {
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

/// The shapes of the outputs the model produced in the grammar experiments,
/// rebuilt in code (the recorded jsonl left the repository, AGL-042): calls
/// with every parameter kind and the gnarly values, the answers as text and
/// as the `answer` tool, and text before a call.
fn corpus() -> Vec<Action> {
    let bash = |command: &str| call("bash", &[("command", text(command))]);
    let read = |path: &str| call("read", &[("path", text(path))]);
    vec![
        // bash: from the trivial to the gnarly.
        bash("ls -la"),
        bash("git log --oneline -- src/action | head -20"),
        bash("echo \"a && b\" | sed 's/a/b/' && true"),
        bash("printf 'x\\n</parameter>\\ny'"),
        bash("cargo test --release -- --test-threads=1"),
        // read: with and without the optional ints.
        read("src/action/tests.rs"),
        call("read", &[
            ("path", text("src/lib.rs")),
            ("offset", Value::Int(0)),
            ("limit", Value::Int(100_000)),
        ]),
        call("read", &[
            ("path", text("src/lib.rs")),
            ("offset", Value::Int(30)),
        ]),
        // edit: multi-line values, an empty new, a value that holds a tag.
        call("edit", &[
            ("path", text("src/lib.rs")),
            ("old", text("fn f() -> Vec<&str> {\n    vec![]\n}")),
            ("new", text("")),
        ]),
        call("edit", &[
            ("path", text("src/lib.rs")),
            ("old", text("let x = 1;")),
            ("new", text("let x = 1;\nlet y = 2; // </parameter> in a comment")),
        ]),
        // the answer as a tool.
        call("answer", &[("text", text("Done. The build is green."))]),
        call("answer", &[("text", text("The split yields one empty item, so I will filter it."))]),
    ]
}

#[test]
fn the_corpus_round_trips() {
    with_big_stack(|| {
        let actions = corpus_tools();
        for action in corpus() {
            check(&actions, &render(&qwen(), &action), &action);
        }
        // The same text as a plain answer where the answer is text.
        for answer in [
            "Done.",
            "A `Vec<&str>` holds &str.",
            "The split yields one empty item, so I will filter it.",
            "Sure: <tool_call> is Qwen's tag",
        ] {
            check(&actions, answer, &Action::Answer(answer.into()));
        }
        // Text before a call, the way the model wrote it.
        let plain = call("bash", &[("command", text("cargo test"))]);
        let expected = Action::Call {
            text: Some("The split yields one empty item, so I will filter it.".into()),
            tool: "bash".into(),
            args: vec![("command".into(), text("cargo test"))],
        };
        let output = format!(
            "The split yields one empty item, so I will filter it.\n\n{}",
            render(&qwen(), &plain)
        );
        check(&actions, &output, &expected);
    });
}

#[test]
fn a_cut_off_call_is_incomplete_at_every_prefix() {
    with_big_stack(|| {
        let format = qwen();
        let cut_off = [
            // A call with the optional ints, cut anywhere.
            render(
                &format,
                &call(
                    "read",
                    &[
                        ("path", text("src/lib.rs")),
                        ("offset", Value::Int(10)),
                        ("limit", Value::Int(20)),
                    ],
                ),
            ),
            // The answer as a tool, cut mid-text.
            render(
                &format,
                &call(
                    "answer",
                    &[("text", text("The split yields one empty item, so I will filter it."))],
                ),
            ),
        ];
        // Where only calls are allowed, every prefix is incomplete.
        let calls_only = Actions {
            tools: corpus_tools().tools,
            answer: false,
        };
        for output in &cut_off {
            for (end, _) in output.char_indices() {
                assert_eq!(
                    parse(&format, &calls_only, &output[..end]),
                    Err(ParseError::Incomplete),
                    "{:?}",
                    &output[..end]
                );
            }
        }
        // Where the answer is allowed too, the cut-off is incomplete from the
        // moment the call's function name is being read (before that the
        // prefix is a plain answer).
        let actions = corpus_tools();
        for output in &cut_off {
            let committed = output.find("<function=").unwrap() + "<function=".len();
            for (end, _) in output.char_indices().skip(committed) {
                assert_eq!(
                    parse(&format, &actions, &output[..end]),
                    Err(ParseError::Incomplete),
                    "{:?}",
                    &output[..end]
                );
            }
        }
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

/// Checks a Gemma call against the grammar and the parser (AGL-050).
fn check_gemma(actions: &Actions, output: &str, expected: &Action) {
    let format = gemma();
    let (grammar, root) = grammar(&format, actions);
    assert!(matches(&grammar, &root, output), "grammar rejects {output:?}");
    assert_eq!(parse(&format, actions, output).as_ref(), Ok(expected));
    assert_eq!(render(&format, expected), output);
}

fn gemma_accepts(actions: &Actions, output: &str) -> bool {
    let (grammar, root) = grammar(&gemma(), actions);
    matches(&grammar, &root, output)
}

#[test]
fn gemma_writes_integers_bare_after_a_comma() {
    with_big_stack(|| {
        let actions = builtin(true);
        check_gemma(
            &actions,
            "<|tool_call>call:bash{command:<|\"|>ls -a<|\"|>,timeout:5}<tool_call|>",
            &call("bash", &[("command", text("ls -a")), ("timeout", Value::Int(5))]),
        );
        check_gemma(
            &actions,
            "<|tool_call>call:bash{command:<|\"|>ls<|\"|>}<tool_call|>",
            &call("bash", &[("command", text("ls"))]),
        );
        // Marks around an integer, or no comma, are not the model's language.
        for output in [
            "<|tool_call>call:bash{command:<|\"|>ls<|\"|>,timeout:<|\"|>5<|\"|>}<tool_call|>",
            "<|tool_call>call:bash{command:<|\"|>ls<|\"|>timeout:5}<tool_call|>",
            "<|tool_call>call:bash{,command:<|\"|>ls<|\"|>}<tool_call|>",
        ] {
            assert!(!gemma_accepts(&actions, output), "grammar accepts {output:?}");
        }
    });
}

#[test]
fn gemma_has_no_comma_before_the_first_parameter_written() {
    with_big_stack(|| {
        // `language` and `limit` are optional and come before the required ones.
        let actions = builtin(true);
        check_gemma(
            &actions,
            "<|tool_call>call:search{query:<|\"|>q<|\"|>,source:<|\"|>wikipedia<|\"|>}<tool_call|>",
            &call("search", &[("query", text("q")), ("source", text("wikipedia"))]),
        );
        check_gemma(
            &actions,
            "<|tool_call>call:search{limit:3,query:<|\"|>q<|\"|>,source:<|\"|>wikipedia<|\"|>}<tool_call|>",
            &call(
                "search",
                &[("limit", Value::Int(3)), ("query", text("q")), ("source", text("wikipedia"))],
            ),
        );
        check_gemma(
            &actions,
            "<|tool_call>call:memory{action:<|\"|>list<|\"|>,text:<|\"|>a, b: c<|\"|>}<tool_call|>",
            &call("memory", &[("action", text("list")), ("text", text("a, b: c"))]),
        );
    });
}

#[test]
fn gemma_values_keep_what_looks_like_its_language() {
    with_big_stack(|| {
        let actions = builtin(true);
        let code = "fn f() { g(\"<|tool\", a, b) } // <tool_call";
        check_gemma(
            &actions,
            &format!("<|tool_call>call:view{{path:<|\"|>{code}<|\"|>}}<tool_call|>"),
            &call("view", &[("path", text(code))]),
        );
    });
}

#[test]
fn gemma_cut_off_calls_are_incomplete() {
    let format = gemma();
    let actions = builtin(true);
    for output in [
        "<|tool_call>call:bash{command:<|\"|>ls",
        "<|tool_call>call:bash{command:<|\"|>ls<|\"|>,timeout:5",
        "<|tool_call>call:bash{command:<|\"|>ls<|\"|>,time",
    ] {
        assert_eq!(parse(&format, &actions, output), Err(ParseError::Incomplete), "{output:?}");
    }
}

#[test]
fn gemma_answers_with_text_before_a_call() {
    with_big_stack(|| {
        let actions = builtin(true);
        check_gemma(&actions, "The answer is 42.", &Action::Answer("The answer is 42.".into()));
        let (format, output) = (gemma(), "I will look.<|tool_call>call:view{path:<|\"|>a<|\"|>}<tool_call|>");
        assert!(gemma_accepts(&actions, output));
        assert_eq!(
            parse(&format, &actions, output),
            Ok(Action::Call {
                text: Some("I will look.".into()),
                tool: "view".into(),
                args: vec![("path".into(), text("a"))],
            })
        );
    });
}
