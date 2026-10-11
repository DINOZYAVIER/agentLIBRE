//! Checks that llama-server accepts the rendered grammars and samples text
//! that matches them (D71); see [`crate::test_server`].

use serde_json::{Value, json};

use super::matcher::matches;
use super::{Expr, Grammar, int_range, pattern};
use crate::test_server;

/// Sends a `/completion` request; returns the status and the body.
fn complete(prompt: &str, grammar: &str, n_predict: u32) -> (u16, String) {
    let body = json!({
        "prompt": prompt,
        "grammar": grammar,
        "n_predict": n_predict,
        "temperature": 1.0,
        "cache_prompt": false,
    });
    test_server::post("/completion", &body)
}

/// A grammar to send, with a prompt that invites the model to fill it.
struct Case {
    name: &'static str,
    prompt: &'static str,
    grammar: Grammar,
    root: Expr,
}

fn case(name: &'static str, prompt: &'static str, root: Expr) -> Case {
    Case {
        name,
        prompt,
        grammar: Grammar::new(),
        root,
    }
}

fn cases() -> Vec<Case> {
    let mut cases = vec![
        case(
            "int",
            "Pick a temperature in Celsius between -50 and 50: ",
            int_range(-50, 50).unwrap(),
        ),
        case(
            "int-wide",
            "Read the file from line ",
            int_range(1, 600).unwrap(),
        ),
        case(
            "enum",
            "Which file should be read? ",
            Expr::one_of(&[
                "src/main.rs",
                "Cargo.toml",
                "a \"quoted\" \\ path",
                "tab\there",
            ]),
        ),
        case(
            "pattern",
            "Run the tests: ",
            pattern::parse("cargo (test|check)( --?[a-z-]+)*").unwrap(),
        ),
        case(
            "pattern-email",
            "Contact address: ",
            pattern::parse(r"[\w.-]+@[a-z]+\.(com|org)").unwrap(),
        ),
        case(
            "empty-alternative",
            "Write b or ab: ",
            pattern::parse("(|a)b").unwrap(),
        ),
    ];

    let mut call = case(
        "call",
        "List the files in the current directory with a bash tool call.\n",
        Expr::Seq(Vec::new()),
    );
    let value = call.grammar.text_without("value", "\n</parameter>", false);
    call.root = Expr::Seq(vec![
        Expr::literal("<tool_call>\n<function=bash>\n<parameter=command>\n"),
        value,
        Expr::literal("\n</parameter>\n</function>\n</tool_call>"),
    ]);
    cases.push(call);

    let mut answer = case(
        "answer",
        "Explain in one sentence what a Rust Vec<&str> is: ",
        Expr::Seq(Vec::new()),
    );
    answer.root = answer.grammar.text_without("answer", "<tool_call>", true);
    cases.push(answer);
    cases
}

#[test]
#[ignore = "needs a llama-server at AGL_TEST_SERVER"]
fn llama_server_accepts_grammars() {
    test_server::with_big_stack(check_server);
}

fn check_server() {
    // Without a visible rejection the checks below would prove nothing.
    let (status, body) = complete("Hi", "root ::= (", 4);
    assert_ne!(status, 200, "a broken grammar was accepted: {body}");

    for case in cases() {
        let grammar = case.grammar.render(&case.root);
        let (status, body) = complete(case.prompt, &grammar, 128);
        assert_eq!(status, 200, "{}: {body}\n{grammar}", case.name);
        let body: Value = serde_json::from_str(&body).unwrap();
        let content = body["content"].as_str().unwrap();
        let truncated = body["stop_type"] == "limit";
        eprintln!(
            "{}: {content:?}{}",
            case.name,
            if truncated { " (limit)" } else { "" }
        );
        assert!(!content.is_empty(), "{}", case.name);
        if case.name == "answer" {
            assert!(!content.contains("<tool_call>"));
        }
        if !truncated {
            assert!(
                matches(&case.grammar, &case.root, content),
                "{}: {content:?}\n{grammar}",
                case.name
            );
        }
    }
}
