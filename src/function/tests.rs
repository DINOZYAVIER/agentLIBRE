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
fn builtin_minimal() {
    let none = Path::new("/nonexistent");
    let function = Function::load(none, none, none, "minimal").unwrap();
    assert_eq!(
        function.system,
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
    assert!(function.preserve_thinking);
    assert_eq!(function.max_steps_total, Some(50));
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
    // The early compaction is off unless set (AGL-038, D9).
    assert!(!function.compact_early);
    assert_eq!(function.max_record_seconds, 60);
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
    // The implicit forge (AGL-011): a tool of the tool states, keeping
    // the state on any outcome.
    assert_eq!(verify.actions.tools.len(), 2);
    assert!(verify.actions.tools.iter().any(|t| t.name == "forge"));
    assert_eq!(verify.next("forge", Outcome::Ok), index("verify"));
    assert_eq!(verify.next("forge", Outcome::Fail), index("verify"));
    let bash = &verify.actions.tools[0];
    assert!(matches!(bash.params[0].constraint, Constraint::Pattern(_)));
    assert_eq!(
        bash.params[1].constraint,
        Constraint::Int { min: 1, max: 600 }
    );
    assert!(answer.actions.tools.is_empty());
}

#[test]
fn the_forge_has_no_switch() {
    // AGL-017, D5: `forge = …` is an unknown key, whatever its value.
    for value in ["false", "true", "\"on\""] {
        let result = function(&format!(
            "format = \"qwen3.8\"\nforge = {value}\nsystem = \"s\"\ninitial = \"a\"\n\
             [state.a]\ntools.bash = {{}}\nnext = {{ else = \"a\" }}\n"
        ));
        assert!(
            matches!(&result, Err(Error::UnknownKey { key, .. }) if key == "forge"),
            "{value}: {result:?}"
        );
    }
    // And the forge is there without it.
    let one = one_state("tools.bash = {}\nnext = { else = \"a\" }").unwrap();
    assert!(
        state(&one, "a")
            .actions
            .tools
            .iter()
            .any(|t| t.name == "forge")
    );
}

#[test]
fn the_forge_tool_follows_the_states() {
    // Every state that allows a tool allows the forge; an answer-only state
    // stays answer-only.
    let function = function(CODER).unwrap();
    for name in ["explore", "verify"] {
        assert!(
            state(&function, name)
                .actions
                .tools
                .iter()
                .any(|t| t.name == "forge"),
            "{name}"
        );
    }
    assert!(state(&function, "answer").actions.tools.is_empty());
}

#[test]
fn the_implicit_forge_keeps_the_state() {
    // A `next` without `else` loads: the forge's outcomes are covered by
    // the loop, a step in the same visit (Q2).
    let one = one_state("tools.bash = {}\nnext = { bash = \"a\" }").unwrap();
    let a = state(&one, "a");
    assert_eq!(a.next("forge", Outcome::Ok), 0);
    assert_eq!(a.next("forge", Outcome::Fail), 0);
    // A named forge key applies like any tool's; the unnamed outcomes
    // keep the state.
    let two = with_states(
        "[state.a]\ntools.bash = {}\nnext = { bash = \"a\", \"forge.ok\" = \"b\", else = \"a\" }\n\
         [state.b]\nanswer = true\n",
    )
    .unwrap();
    let a = state(&two, "a");
    assert_eq!(a.next("forge", Outcome::Ok), 1);
    assert_eq!(a.next("forge", Outcome::Fail), 0);
}

#[test]
fn the_explicit_forge_tool() {
    // An explicit `tools.forge` is accepted: it is the way to give forge its
    // own transitions (AGL-011, Q3; AGL-017, D5).
    let function = one_state("tools.forge = {}\nnext = { forge = \"a\", else = \"a\" }").unwrap();
    let a = state(&function, "a");
    assert_eq!(a.actions.tools.len(), 1);
    assert_eq!(a.actions.tools[0].name, "forge");
    assert_eq!(a.next("forge", Outcome::Ok), 0);
}

#[test]
fn files_and_names() {
    let dir = std::env::temp_dir().join(format!("agentLIBRE-function-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("minimal.toml"), CODER).unwrap();
    let none = Path::new("/nonexistent");
    let replaced = Function::load(&dir, none, none, "minimal");
    let missing = Function::load(&dir, none, none, "coder");
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(replaced.unwrap().states.len(), 3);
    assert!(matches!(missing, Err(Error::FunctionNotFound { name, .. }) if name == "coder"));
    assert!(matches!(
        Function::load(none, none, none, "../default"),
        Err(Error::InvalidName(_))
    ));
}

#[test]
fn the_load_tiers() {
    // AGL-028, D3: the workspace's functions directory, then the user's
    // directory, then the built-ins; an absent directory is skipped.
    let workspace = std::env::temp_dir().join(format!(
        "agentLIBRE-function-tiers-w-{}",
        std::process::id()
    ));
    let user = std::env::temp_dir().join(format!(
        "agentLIBRE-function-tiers-u-{}",
        std::process::id()
    ));
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&user).unwrap();
    let text = |system: &str| {
        format!("format = \"qwen3.8\"\nsystem = \"{system}\"\ninitial = \"a\"\n[state.a]\nanswer = true\n")
    };
    fs::write(workspace.join("same.toml"), text("workspace")).unwrap();
    fs::write(user.join("same.toml"), text("user")).unwrap();
    fs::write(user.join("user-only.toml"), text("user only")).unwrap();
    let none = Path::new("/nonexistent");
    let workspace_wins = Function::load(&workspace, &user, none, "same").unwrap();
    assert_eq!(workspace_wins.system, "workspace");
    let user_over_builtin = Function::load(&workspace, &user, none, "user-only").unwrap();
    assert_eq!(user_over_builtin.system, "user only");
    let built_in = Function::load(&workspace, &user, none, "minimal").unwrap();
    assert_eq!(
        built_in.system,
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
    let skipped = Function::load(none, &user, none, "same").unwrap();
    assert_eq!(skipped.system, "user");
    let missing = Function::load(&workspace, &user, none, "no-such");
    assert!(matches!(
        missing,
        Err(Error::FunctionNotFound { ref name, ref paths })
            if name == "no-such" && paths[0] == *workspace && paths[1] == *user
    ));
    fs::remove_dir_all(&workspace).unwrap();
    fs::remove_dir_all(&user).unwrap();
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
    // 0 is the no limit (AGL-021, D2); the absent key keeps the 50 (D40).
    assert_eq!(
        function(
            "format = \"qwen3.8\"\nsystem = \"s\"\nmax_steps_total = 0\ninitial = \"a\"\n\
             [state.a]\nanswer = true"
        )
        .unwrap()
        .max_steps_total,
        None
    );
    assert_eq!(
        problem_of(function(
            "format = \"qwen3.8\"\nsystem = \"s\"\nmax_steps_total = -1\ninitial = \"a\"\n\
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
    // `compact_early` (AGL-038, D9): a boolean.
    let early = function(
        "format = \"qwen3.8\"\nsystem = \"s\"\ncompact_early = true\ninitial = \"a\"\n\
         [state.a]\nanswer = true",
    )
    .unwrap();
    assert!(early.compact_early);
    assert!(function(
        "format = \"qwen3.8\"\nsystem = \"s\"\ncompact_early = \"yes\"\ninitial = \"a\"\n\
         [state.a]\nanswer = true"
    )
    .is_err());
    assert_eq!(
        problem_of(function(
            "format = \"qwen3.8\"\nsystem = \"s\"\nmax_compact_tokens = 0\ninitial = \"a\"\n\
             [state.a]\nanswer = true"
        )),
        ("max_compact_tokens".into(), FunctionProblem::NotPositive)
    );
    // The longest voice recording (AGL-016, D5, D8): 60 s unless set.
    assert_eq!(function(CODER).unwrap().max_record_seconds, 60);
    let recording = function(
        "format = \"qwen3.8\"\nsystem = \"s\"\nmax_record_seconds = 15\ninitial = \"a\"\n\
         [state.a]\nanswer = true",
    )
    .unwrap();
    assert_eq!(recording.max_record_seconds, 15);
    assert_eq!(
        problem_of(function(
            "format = \"qwen3.8\"\nsystem = \"s\"\nmax_record_seconds = 0\ninitial = \"a\"\n\
             [state.a]\nanswer = true"
        )),
        ("max_record_seconds".into(), FunctionProblem::NotPositive)
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
    let dir = std::env::temp_dir().join(format!("agentLIBRE-plain-{}", std::process::id()));
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

#[test]
fn the_system_text_gets_the_two_words_of_the_runtime_and_keeps_other_braces() {
    let f = function(
        "format = \"qwen3.8\"\nmax_action_tokens = 500\n\
         system = \"Prefix {runtime_prefix}, at most {max_action_tokens} tokens, keep {x} and {{y}}.\"\n\
         initial = \"a\"\n[state.a]\nanswer = true",
    )
    .unwrap();
    assert_eq!(
        f.system,
        "Prefix [agentLIBRE runtime], at most 500 tokens, keep {x} and {{y}}."
    );
    // Without the default limit set, the default is told.
    let g = function(
        "format = \"qwen3.8\"\nsystem = \"{max_action_tokens}\"\ninitial = \"a\"\n\
         [state.a]\nanswer = true",
    )
    .unwrap();
    assert_eq!(g.system, "8192");
}

#[test]
fn the_built_in_functions_differ_in_the_system_and_the_step_limit() {
    let none = Path::new("/nonexistent");
    let minimal = Function::load(none, none, none, "minimal").unwrap();
    let agentlibre = Function::load(none, none, none, "agentLIBRE").unwrap();
    // `minimal` is laconic and nameless (AGL-018, D1, AGL-020, D1).
    // `agentLIBRE` is the persona and the runtime part: the prefix and the
    // size of an action (AGL-009, AGL-018, D2, AGL-020, D2).
    assert_eq!(
        minimal.system,
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
    assert!(agentlibre.system.starts_with("You are DinoBot"));
    assert!(agentlibre.system.contains(
        "Messages that start with [agentLIBRE runtime] come from the agentLIBRE runtime, \
         not from the human. One action holds at most 8192 tokens; write a large file in parts."
    ));
    assert!(!agentlibre.system.contains('{'));
    // The step limit differs too: `minimal` keeps the 50 of the absent key
    // (D40); `agentLIBRE` is unlimited (AGL-021, D1).
    assert_eq!(minimal.max_steps_total, Some(50));
    assert_eq!(agentlibre.max_steps_total, None);
    // The rest is the same: the states and the tools.
    assert_eq!(minimal.states.len(), agentlibre.states.len());
    let tools = |f: &Function| -> Vec<String> {
        f.states[f.initial]
            .actions
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect()
    };
    assert_eq!(tools(&minimal), tools(&agentlibre));
    assert_eq!(minimal.sampling, agentlibre.sampling);
}

#[test]
fn the_search_tool_is_read_with_its_parameters_and_constraints() {
    let function = one_state("tools.search = {}\nanswer = true\nnext = { else = \"a\" }").unwrap();
    let tool = function.states[0]
        .actions
        .tools
        .iter()
        .find(|tool| tool.name == "search")
        .unwrap();
    // The parameters are in the alphabetical order of the template, the
    // grammar holds the enums and the range (AGL-019, D6).
    let names: Vec<&str> = tool.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["language", "limit", "query", "source"]);
    let required: Vec<bool> = tool.params.iter().map(|p| p.required).collect();
    assert_eq!(required, [false, false, true, true]);
    assert_eq!(
        tool.params[0].constraint,
        Constraint::OneOf(vec!["en".into(), "fr".into(), "ru".into(), "uk".into()])
    );
    assert_eq!(tool.params[1].constraint, Constraint::Int { min: 1, max: 10 });
    assert_eq!(
        tool.params[3].constraint,
        Constraint::OneOf(vec!["web".into(), "wikipedia".into()])
    );
    // A Function may narrow them, as it narrows the others.
    let narrowed = one_state(
        "tools.search = { source = { enum = [\"wikipedia\"] }, limit = { int = { min = 1, max = 3 } } }\nanswer = true\nnext = { else = \"a\" }",
    )
    .unwrap();
    let tool = narrowed.states[0].actions.tools.iter().find(|t| t.name == "search").unwrap();
    assert_eq!(tool.params[3].constraint, Constraint::OneOf(vec!["wikipedia".into()]));
    assert_eq!(tool.params[1].constraint, Constraint::Int { min: 1, max: 3 });
    // A parameter the tool has not is an error, as for the others.
    assert!(one_state("tools.search = { path = {} }\nanswer = true").is_err());
}

#[test]
fn the_built_in_functions_have_the_search_tool() {
    for name in ["minimal", "agentLIBRE"] {
        let function = Function::load(Path::new("/none"), Path::new("/none"), Path::new("/none"), name).unwrap();
        let tools: Vec<&str> = function.states[function.initial]
            .actions
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(tools.contains(&"search"), "{name}: {tools:?}");
    }
}

#[test]
fn without_a_tool_the_state_loses_it_and_keeps_an_action() {
    let mut function = with_states(
        "[state.a]\ntools.search = {}\nnext = { else = \"b\" }\n\
         [state.b]\ntools.bash = {}\ntools.search = {}\nanswer = true\nnext = { else = \"b\" }\n",
    )
    .unwrap();
    function.without_tool("search");
    let names = |state: &State| -> Vec<String> {
        state.actions.tools.iter().map(|t| t.name.clone()).collect()
    };
    // `a` had the search and the forge the runtime adds: the forge stays.
    assert_eq!(names(&function.states[0]), ["forge"]);
    assert!(names(&function.states[1]).contains(&"bash".to_owned()));
    assert!(!names(&function.states[1]).contains(&"search".to_owned()));
    for state in &function.states {
        assert!(state.actions.answer || !state.actions.tools.is_empty());
    }
    // A state whose only tool goes gets the answer in its place.
    let mut only = with_states("[state.a]\ntools.search = {}\nnext = { else = \"a\" }\n").unwrap();
    only.states[0].actions.tools.retain(|t| t.name == "search");
    only.without_tool("search");
    assert!(only.states[0].actions.tools.is_empty());
    assert!(only.states[0].actions.answer);
}
