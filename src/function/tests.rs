use std::fs;

use super::*;
use crate::grammar::pattern::{PatternError, PatternErrorKind};

/// A multi-state Function used by the load and transition tests.
pub const CODER: &str = r#"
format = "qwen3.8"
system = "You are a coding agent working in this repository."
preserve_thinking = true
max_steps_total = 50
initial = "explore"

[sampling]
temperature = 1.0
top_p = 0.95
top_k = 20

[state.explore]
instruction = "Explore the code with bash. Call edit once you know what to change."
tools.bash = {}
tools.edit = {}
max_steps = 15
max_visits = 3
on_limit = "answer"
next = { edit = "verify", else = "explore" }

[state.verify]
instruction = "Only verification is available now: cargo test or cargo check with flags."
tools.bash = { command = { pattern = "cargo (test|check)( --?[a-z-]+)*" } }
think = false
max_visits = 3
on_limit = "answer"
next = { "bash.ok" = "answer", "bash.fail" = "explore", "bash.timeout" = "answer" }

[state.answer]
instruction = "Tools are not available. Summarize what was done and the check result."
answer = true
"#;

/// Loads `text` as a Function with the built-in formats.
pub fn function(text: &str) -> Result<Function, Error> {
    let path = Path::new("f.toml");
    Function::from_table(
        path,
        tomlfile::parse(path, text)?,
        Path::new("/nonexistent"),
    )
}

/// A Function whose states are `states`, starting in `a`.
pub fn with_states(states: &str) -> Result<Function, Error> {
    function(&format!(
        "format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"a\"\n{states}"
    ))
}

/// A one-state Function with `body` in `[state.a]`.
fn one_state(body: &str) -> Result<Function, Error> {
    with_states(&format!("[state.a]\n{body}\n"))
}

fn problem_of(result: Result<Function, Error>) -> (String, FunctionProblem) {
    match result {
        Err(Error::Function { key, problem, .. }) => (key, problem),
        other => panic!("expected a Function problem, got {other:?}"),
    }
}

fn state<'f>(function: &'f Function, name: &str) -> &'f State {
    function.states.iter().find(|s| s.name == name).unwrap()
}

#[test]
fn builtin_default() {
    let none = Path::new("/nonexistent");
    let function = Function::load(none, none, "default").unwrap();
    assert!(
        function
            .system
            .starts_with("You are a coding agent working in the user's workspace.")
    );
    assert!(function.preserve_thinking);
    assert_eq!(function.max_steps_total, 50);
    assert_eq!(
        (function.max_think_tokens, function.max_action_tokens),
        (4096, 8192)
    );
    assert!(
        function
            .compact
            .starts_with("Summarize the conversation so far")
    );
    assert!(function.compact.ends_with("Write only the summary."));
    assert_eq!(function.max_compact_tokens, 4096);
    assert_eq!(
        function.sampling,
        Sampling {
            temperature: Some(1.0),
            top_p: Some(0.95),
            top_k: Some(20),
            ..Sampling::default()
        }
    );
    let work = &function.states[function.initial];
    assert_eq!(work.name, "work");
    assert_eq!(work.instruction, None);
    assert!(function.format.think.is_some());
    assert_eq!(work.actions.tools, action::builtin_tools());
    assert!(work.actions.answer && work.think);
    assert_eq!(work.next("bash", Outcome::Timeout), function.initial);
}

#[test]
fn spec_example() {
    let function = function(CODER).unwrap();
    let explore = state(&function, "explore");
    let verify = state(&function, "verify");
    let answer = state(&function, "answer");
    let index = |name| function.states.iter().position(|s| s.name == name).unwrap();
    assert_eq!(function.initial, index("explore"));
    assert!(explore.think && !verify.think && answer.think);
    assert!(
        verify
            .instruction
            .as_deref()
            .unwrap()
            .starts_with("Only verification")
    );
    assert!(!explore.actions.answer && answer.actions.answer);
    assert_eq!((explore.max_steps, explore.max_visits), (Some(15), Some(3)));
    assert_eq!(explore.on_limit, Some(index("answer")));
    assert_eq!(explore.next("edit", Outcome::Fail), index("verify"));
    assert_eq!(explore.next("bash", Outcome::Ok), index("explore"));
    assert_eq!(verify.next("bash", Outcome::Ok), index("answer"));
    assert_eq!(verify.next("bash", Outcome::Fail), index("explore"));
    assert_eq!(verify.actions.tools.len(), 1);
    let bash = &verify.actions.tools[0];
    assert!(matches!(bash.params[0].constraint, Constraint::Pattern(_)));
    assert_eq!(
        bash.params[1].constraint,
        Constraint::Int { min: 1, max: 600 }
    );
    assert!(answer.actions.tools.is_empty());
}

#[test]
fn files_and_names() {
    let dir = std::env::temp_dir().join(format!("agl-lite-function-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("default.toml"), CODER).unwrap();
    let none = Path::new("/nonexistent");
    let replaced = Function::load(&dir, none, "default");
    let missing = Function::load(&dir, none, "coder");
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(replaced.unwrap().states.len(), 3);
    assert!(matches!(missing, Err(Error::FunctionNotFound { name, .. }) if name == "coder"));
    assert!(matches!(
        Function::load(none, none, "../default"),
        Err(Error::InvalidName(_))
    ));
}

#[test]
fn top_level_errors() {
    assert!(matches!(
        function("system = \"s\"\ninitial = \"a\"\n[state.a]\nanswer = true"),
        Err(Error::MissingKey { key, .. }) if key == "format"
    ));
    assert!(matches!(
        function("format = \"qwen3.8\"\ninitial = \"a\"\n[state.a]\nanswer = true"),
        Err(Error::MissingKey { key, .. }) if key == "system"
    ));
    assert!(matches!(
        function("format = \"gemma\"\nsystem = \"s\"\ninitial = \"a\"\n[state.a]\nanswer = true"),
        Err(Error::FormatNotFound { .. })
    ));
    assert!(matches!(
        one_state("answer = true\nmodel = 1"),
        Err(Error::UnknownKey { key, .. }) if key == "state.a.model"
    ));
    assert_eq!(
        problem_of(function(
            "format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"b\"\n[state.a]\nanswer = true"
        )),
        ("initial".into(), FunctionProblem::UnknownState("b".into()))
    );
    let sampling = |body: &str| {
        function(&format!(
            "format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"a\"\n[sampling]\n{body}\n\
             [state.a]\nanswer = true"
        ))
    };
    assert!(matches!(
        sampling("seed = 1"),
        Err(Error::UnknownKey { key, .. }) if key == "sampling.seed"
    ));
    assert!(matches!(
        sampling("top_k = 2.5"),
        Err(Error::WrongType { key, .. }) if key == "sampling.top_k"
    ));
    assert_eq!(
        sampling("temperature = 1").unwrap().sampling.temperature,
        Some(1.0)
    );
    assert_eq!(
        problem_of(function(
            "format = \"qwen3.8\"\nsystem = \"s\"\nmax_steps_total = 0\ninitial = \"a\"\n\
             [state.a]\nanswer = true"
        )),
        ("max_steps_total".into(), FunctionProblem::NotPositive)
    );
    let compaction = function(
        "format = \"qwen3.8\"\nsystem = \"s\"\ncompact = \"Sum up.\"\nmax_compact_tokens = 512\n\
         initial = \"a\"\n[state.a]\nanswer = true",
    )
    .unwrap();
    assert_eq!(
        (compaction.compact.as_str(), compaction.max_compact_tokens),
        ("Sum up.", 512)
    );
    assert_eq!(
        problem_of(function(
            "format = \"qwen3.8\"\nsystem = \"s\"\nmax_compact_tokens = 0\ninitial = \"a\"\n\
             [state.a]\nanswer = true"
        )),
        ("max_compact_tokens".into(), FunctionProblem::NotPositive)
    );
}

#[test]
fn state_errors() {
    use FunctionProblem::*;
    let anchor = PatternError {
        offset: 0,
        kind: PatternErrorKind::Anchor,
    };
    let cases = [
        ("instruction = \"x\"", "state.a", NoActions),
        (
            "answer = true\nnext = { else = \"a\" }",
            "state.a.next",
            NextWithoutTools,
        ),
        (
            "tools.bash = {}\nnext = { cat = \"a\" }",
            "state.a.next.cat",
            BadTransitionKey,
        ),
        (
            "tools.bash = {}\nnext = { \"bash.oops\" = \"a\" }",
            "state.a.next.bash.oops",
            BadTransitionKey,
        ),
        (
            "tools.edit = {}\nnext = { \"edit.timeout\" = \"a\" }",
            "state.a.next.edit.timeout",
            BadTransitionKey,
        ),
        (
            "tools.bash = {}\nnext = { edit = \"a\", else = \"a\" }",
            "state.a.next.edit",
            BadTransitionKey,
        ),
        (
            "tools.bash = {}\nnext = { \"bash.ok\" = \"a\" }",
            "state.a.next",
            MissingTransition {
                tool: "bash".into(),
                outcome: "fail",
            },
        ),
        (
            "tools.bash = {}",
            "state.a.next",
            MissingTransition {
                tool: "bash".into(),
                outcome: "ok",
            },
        ),
        (
            "tools.bash = {}\nnext = { else = \"b\" }",
            "state.a.next.else",
            UnknownState("b".into()),
        ),
        (
            "answer = true\nmax_steps = 0\non_limit = \"a\"",
            "state.a.max_steps",
            NotPositive,
        ),
        (
            "answer = true\non_limit = \"a\"",
            "state.a.on_limit",
            OnLimitWithoutLimit,
        ),
        (
            "answer = true\nmax_visits = 1\non_limit = \"a\"",
            "state.a.on_limit",
            OnLimitCycle,
        ),
        (
            "tools.bash = { command = { pattern = \"^ls\" } }\nnext = { else = \"a\" }",
            "state.a.tools.bash.command.pattern",
            Pattern(anchor),
        ),
        (
            "tools.bash = { command = { pattern = \"[^;]*\" } }\nnext = { else = \"a\" }",
            "state.a.tools.bash.command.pattern",
            PatternMayEndValue { end: '\n' },
        ),
        (
            "tools.edit = { path = { enum = [] } }\nnext = { else = \"a\" }",
            "state.a.tools.edit.path.enum",
            EmptyEnum,
        ),
        (
            "tools.edit = { path = { enum = [\"a\"], pattern = \"a\" } }\nnext = { else = \"a\" }",
            "state.a.tools.edit.path",
            ConstraintKeys,
        ),
        (
            "tools.edit = { path = {} }\nnext = { else = \"a\" }",
            "state.a.tools.edit.path",
            ConstraintKeys,
        ),
        (
            "tools.bash = { timeout = { int = { min = 0, max = 5 } } }\nnext = { else = \"a\" }",
            "state.a.tools.bash.timeout.int",
            BadRange,
        ),
        (
            "tools.bash = { timeout = { int = { min = 9, max = 5 } } }\nnext = { else = \"a\" }",
            "state.a.tools.bash.timeout.int",
            BadRange,
        ),
    ];
    for (body, key, expected) in cases {
        assert_eq!(
            problem_of(one_state(body)),
            (key.to_string(), expected),
            "{body}"
        );
    }
}

#[test]
fn key_errors() {
    for (body, key) in [
        ("tools.cat = {}", "state.a.tools.cat"),
        ("tools.bash = { cmd = {} }", "state.a.tools.bash.cmd"),
        (
            "tools.edit = { path = { regex = \"a\" } }",
            "state.a.tools.edit.path.regex",
        ),
        (
            "tools.bash = { timeout = { pattern = \"1\" } }",
            "state.a.tools.bash.timeout.pattern",
        ),
    ] {
        let result = one_state(&format!("{body}\nnext = {{ else = \"a\" }}"));
        assert!(
            matches!(&result, Err(Error::UnknownKey { key: k, .. }) if k == key),
            "{body}: {result:?}"
        );
    }
    assert!(matches!(
        one_state("answer = true\nmax_steps = 2"),
        Err(Error::MissingKey { key, .. }) if key == "state.a.on_limit"
    ));
    assert!(matches!(
        one_state("tools.bash = { timeout = { int = { min = 1 } } }\nnext = { else = \"a\" }"),
        Err(Error::MissingKey { key, .. }) if key == "state.a.tools.bash.timeout.int.max"
    ));
}

#[test]
fn constraints_are_read() {
    let function = one_state(
        "tools.bash = { timeout = { int = { min = 5, max = 3600 } } }\n\
         tools.edit = { path = { enum = [\"src/lib.rs\"] } }\n\
         next = { else = \"a\" }",
    )
    .unwrap();
    let tools = &function.states[0].actions.tools;
    assert_eq!(
        tools[0].params[1].constraint,
        Constraint::Int { min: 5, max: 3600 }
    );
    assert_eq!(
        tools[1].params[0].constraint,
        Constraint::OneOf(vec!["src/lib.rs".into()])
    );
    assert_eq!(tools[1].params[1].constraint, Constraint::Text);
}

#[test]
fn graph_errors() {
    assert_eq!(
        problem_of(with_states(
            "[state.a]\nanswer = true\nmax_visits = 1\non_limit = \"b\"\n\
             [state.b]\nanswer = true\nmax_visits = 1\non_limit = \"a\""
        )),
        ("state.a.on_limit".into(), FunctionProblem::OnLimitCycle)
    );
    assert_eq!(
        problem_of(with_states(
            "[state.a]\nanswer = true\n[state.z]\nanswer = true"
        )),
        ("state.z".into(), FunctionProblem::Unreachable)
    );
    // A state reached only through `on_limit` is reachable.
    with_states(
        "[state.a]\ntools.bash = {}\nmax_steps = 2\non_limit = \"z\"\nnext = { else = \"a\" }\n\
         [state.z]\nanswer = true",
    )
    .unwrap();
}

#[test]
fn thinking_and_formats() {
    let dir = std::env::temp_dir().join(format!("agl-lite-plain-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plain.toml"),
        "[call]\nbegin = \"<c {name}>\"\nparam = \"<p {name}>{value}</p>\"\nend = \"</c>\"\n",
    )
    .unwrap();
    let load = |think: &str| {
        let text = format!(
            "format = \"plain\"\nsystem = \"s\"\ninitial = \"a\"\n[state.a]\nanswer = true\n{think}"
        );
        let path = Path::new("f.toml");
        Function::from_table(path, tomlfile::parse(path, &text).unwrap(), &dir)
    };
    let default = load("");
    let explicit = load("think = true");
    fs::remove_dir_all(&dir).unwrap();
    assert!(!default.unwrap().states[0].think);
    assert_eq!(
        problem_of(explicit),
        (
            "state.a.think".into(),
            FunctionProblem::NoThinking {
                format: "plain".into()
            }
        )
    );
}
