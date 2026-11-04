use std::fs;

use super::*;
use crate::forge::testutil::TempDir;
use crate::grammar::pattern::{PatternError, PatternErrorKind};

/// The model of the test Functions: the built-in Qwen3.8 format on the
/// llama kind (AGL-039, D17, D18).
pub const MODEL: &str = r#"{ kind = "llama"; format = agl.formats."qwen3.8"; weights = "m.gguf"; }"#;

/// A multi-state policy used by the load and transition tests (AGL-039,
/// D40: the nested transitions).
pub const CODER: &str = r#"{
  system = "You are a coding agent working in this repository.";
  max_steps_total = 50;
  initial = "explore";
  state.explore = {
    instruction = "Explore the code with bash. Call edit once you know what to change.";
    tools = { bash = { }; edit = { }; };
    max_steps = 15;
    max_visits = 3;
    on_limit = "answer";
    next = { edit.any = "verify"; otherwise = "explore"; };
  };
  state.verify = {
    instruction = "Only verification is available now: cargo test or cargo check with flags.";
    tools.bash.command.pattern = "cargo (test|check)( --?[a-z-]+)*";
    think = false;
    max_visits = 3;
    on_limit = "answer";
    next.bash = { ok = "answer"; fail = "explore"; timeout = "answer"; };
  };
  state.answer = {
    instruction = "Tools are not available. Summarize what was done and the check result.";
    answer = true;
  };
}"#;

/// Evaluates the Nix Function `source`, a function of `{ agl, ... }` (D8).
pub fn evaluate(source: &str) -> Result<Function, Error> {
    let path = Path::new("f.nix");
    let value = nix::eval_with_agl(path, &format!("({source}) {{ inherit agl; }}"))?;
    Function::from_value(path, value)
}

/// A Function whose policy is the Nix set `policy`, on `model`.
pub fn function_on(policy: &str, model: &str) -> Result<Function, Error> {
    evaluate(&format!("{{ agl, ... }}: {{ policy = {policy}; model = {model}; }}"))
}

/// A Function whose policy is the Nix set `policy`, on the test model.
pub fn function(policy: &str) -> Result<Function, Error> {
    function_on(policy, MODEL)
}

/// A Function whose states are the Nix set `states`, starting in `a`.
pub fn with_states(states: &str) -> Result<Function, Error> {
    function(&format!(r#"{{ system = "s"; initial = "a"; state = {states}; }}"#))
}

/// A one-state Function with the attributes `body` in `state.a`.
fn one_state(body: &str) -> Result<Function, Error> {
    with_states(&format!("{{ a = {{ {body} }}; }}"))
}

/// A one-state answer-only policy with `extra` attributes.
fn plain(extra: &str) -> String {
    format!(r#"{{ system = "s"; initial = "a"; state.a.answer = true; {extra} }}"#)
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

fn none() -> &'static Path {
    Path::new("/nonexistent")
}

#[test]
fn builtin_gpu_high_minimal() {
    let function = Function::load(none(), none(), "agentLIBRE-gpu-high-minimal").unwrap();
    assert_eq!(
        function.system,
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
    assert!(function.preserve_thinking);
    assert_eq!(function.max_steps_total, Some(50));
    assert_eq!(
        (function.max_think_tokens, function.max_action_tokens),
        (4096, None)
    );
    // Unset, the action's limit is an eighth of the context (AGL-043 F14).
    assert_eq!(function.action_tokens(131_072), 16_384);
    assert_eq!(function.action_tokens(65_536), 8_192);
    assert_eq!(function.action_tokens(262_144), 32_768);
    assert!(
        function
            .compact
            .starts_with("Summarize the conversation so far")
    );
    assert!(function.compact.ends_with("Write only the summary."));
    // Unset, the summary's limit is a sixteenth of the context between
    // 4096 and 16384 (AGL-043 F6).
    assert_eq!(function.max_compact_tokens, None);
    assert_eq!(function.compact_tokens(131_072), 8_192);
    assert_eq!(function.compact_tokens(65_536), 4_096);
    assert_eq!(function.compact_tokens(32_768), 4_096);
    assert_eq!(function.compact_tokens(262_144), 16_384);
    assert_eq!(function.compact_tokens(1_048_576), 16_384);
    // The early compaction is off unless set (AGL-038, D9).
    assert!(!function.compact_early);
    assert_eq!(
        function.sampling,
        Sampling {
            temperature: Some(1.0),
            top_p: Some(0.95),
            top_k: Some(20),
            ..Sampling::default()
        }
    );
    let inference = &function.states[function.initial];
    assert_eq!(inference.name, "inference");
    assert_eq!(inference.instruction, None);
    assert!(function.format.think.is_some());
    assert_eq!(inference.actions.tools, action::builtin_tools());
    assert!(inference.actions.answer && inference.think);
    assert_eq!(inference.next("bash", Outcome::Timeout), function.initial);
    // The model of D33 and D44 (AGL-039).
    assert_eq!(
        function.model,
        ServerFile {
            kind: Some("llama".into()),
            model: Some("Qwen3.8-27B-UD-IQ4_XS.gguf".into()),
            context: Some(131072),
            spec_type: Some("draft-mtp".into()),
            args: Some(["-ngl", "999", "-fa", "on", "-ctk", "q8_0", "-ctv", "q8_0"].map(String::from).to_vec()),
            mmproj: Some("mmproj-Qwen3.8-27B-F16.gguf".into()),
            mmproj_offload: Some(false),
            ..ServerFile::default()
        }
    );
    assert_eq!(function.path, Path::new("<built-in>/functions/agentLIBRE-gpu-high-minimal.nix"));
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
fn the_transitions_take_the_outcome_then_any_then_otherwise() {
    // AGL-039, D40.
    let function = with_states(
        r#"{
          a = { tools = { bash = { }; edit = { }; }; next = { bash = { ok = "b"; any = "c"; }; otherwise = "a"; }; };
          b.answer = true;
          c.answer = true;
        }"#,
    )
    .unwrap();
    let a = state(&function, "a");
    assert_eq!(a.next("bash", Outcome::Ok), 1);
    assert_eq!(a.next("bash", Outcome::Fail), 2);
    assert_eq!(a.next("bash", Outcome::Timeout), 2);
    assert_eq!(a.next("edit", Outcome::Ok), 0);
}

#[test]
fn the_forge_has_no_switch() {
    // AGL-017, D5: `forge = …` is an unknown key, whatever its value.
    for value in ["false", "true", "\"on\""] {
        let result = function(&format!(
            r#"{{ forge = {value}; system = "s"; initial = "a"; state.a = {{ tools.bash = {{ }}; next.otherwise = "a"; }}; }}"#
        ));
        assert!(
            matches!(&result, Err(Error::UnknownKey { key, .. }) if key == "policy.forge"),
            "{value}: {result:?}"
        );
    }
    // And the forge is there without it.
    let one = one_state(r#"tools.bash = { }; next.otherwise = "a";"#).unwrap();
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
    // A `next` without `otherwise` loads: the forge's outcomes are covered
    // by the loop, a step in the same visit (Q2).
    let one = one_state(r#"tools.bash = { }; next.bash.any = "a";"#).unwrap();
    let a = state(&one, "a");
    assert_eq!(a.next("forge", Outcome::Ok), 0);
    assert_eq!(a.next("forge", Outcome::Fail), 0);
    // A named forge outcome applies like any tool's; the unnamed outcomes
    // keep the state.
    let two = with_states(
        r#"{ a = { tools.bash = { }; next = { bash.any = "a"; forge.ok = "b"; otherwise = "a"; }; }; b.answer = true; }"#,
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
    let function = one_state(r#"tools.forge = { }; next = { forge.any = "a"; otherwise = "a"; };"#).unwrap();
    let a = state(&function, "a");
    assert_eq!(a.actions.tools.len(), 1);
    assert_eq!(a.actions.tools[0].name, "forge");
    assert_eq!(a.next("forge", Outcome::Ok), 0);
}

/// A Function file of the policy `policy` on the test model.
fn file_text(policy: &str) -> String {
    format!("{{ agl, ... }}: {{ policy = {policy}; model = {MODEL}; }}")
}

#[test]
fn files_and_names() {
    let dir = TempDir::new();
    fs::write(dir.0.join("agentLIBRE-gpu-high.nix"), file_text(CODER)).unwrap();
    let replaced = Function::load(&dir.0, none(), "agentLIBRE-gpu-high");
    let missing = Function::load(&dir.0, none(), "coder");
    assert_eq!(replaced.unwrap().states.len(), 3);
    assert!(matches!(missing, Err(Error::FunctionNotFound { name, .. }) if name == "coder"));
    assert!(matches!(
        Function::load(none(), none(), "../default"),
        Err(Error::InvalidName(_))
    ));
    // A file that is not a function of `{ agl, ... }` (AGL-039, D8).
    fs::write(dir.0.join("plain.nix"), format!("{{ policy = {CODER}; }}")).unwrap();
    assert!(matches!(
        Function::load(&dir.0, none(), "plain"),
        Err(Error::NixNotAFunction { .. })
    ));
    // A file that does not evaluate names the file.
    fs::write(dir.0.join("broken.nix"), "{ agl, ... }: { policy = ; }").unwrap();
    assert!(matches!(
        Function::load(&dir.0, none(), "broken"),
        Err(Error::NixEval { path, .. }) if path == dir.0.join("broken.nix")
    ));
}

#[test]
fn the_load_tiers() {
    // AGL-028, D3; AGL-039, D10: the workspace's functions directory, then
    // the user's directory, then the built-ins; an absent directory is
    // skipped.
    let workspace = TempDir::new();
    let user = TempDir::new();
    let text = |system: &str| {
        file_text(&format!(r#"{{ system = "{system}"; initial = "a"; state.a.answer = true; }}"#))
    };
    fs::write(workspace.0.join("same.nix"), text("workspace")).unwrap();
    fs::write(user.0.join("same.nix"), text("user")).unwrap();
    fs::write(user.0.join("user-only.nix"), text("user only")).unwrap();
    let workspace_wins = Function::load(&workspace.0, &user.0, "same").unwrap();
    assert_eq!(workspace_wins.system, "workspace");
    let user_over_builtin = Function::load(&workspace.0, &user.0, "user-only").unwrap();
    assert_eq!(user_over_builtin.system, "user only");
    let built_in = Function::load(&workspace.0, &user.0, "agentLIBRE-gpu-high-minimal").unwrap();
    assert_eq!(
        built_in.system,
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
    let skipped = Function::load(none(), &user.0, "same").unwrap();
    assert_eq!(skipped.system, "user");
    let missing = Function::load(&workspace.0, &user.0, "no-such");
    assert!(matches!(
        missing,
        Err(Error::FunctionNotFound { ref name, ref paths })
            if name == "no-such" && paths[0] == workspace.0 && paths[1] == user.0
    ));
}

#[test]
fn top_level_errors() {
    // Exactly `policy` and `model`, both required (AGL-039, D17, D21).
    assert!(matches!(
        evaluate(&format!("{{ agl, ... }}: {{ policy = {}; }}", plain(""))),
        Err(Error::MissingKey { key, .. }) if key == "model"
    ));
    assert!(matches!(
        evaluate(&format!("{{ agl, ... }}: {{ model = {MODEL}; }}")),
        Err(Error::MissingKey { key, .. }) if key == "policy"
    ));
    assert!(matches!(
        evaluate(&format!("{{ agl, ... }}: {{ policy = {}; model = {MODEL}; server = {{ }}; }}", plain(""))),
        Err(Error::UnknownKey { key, .. }) if key == "server"
    ));
    assert!(matches!(
        function(r#"{ initial = "a"; state.a.answer = true; }"#),
        Err(Error::MissingKey { key, .. }) if key == "policy.system"
    ));
    assert!(matches!(
        function_on(&plain(""), r#"{ kind = "llama"; weights = "m.gguf"; }"#),
        Err(Error::MissingKey { key, .. }) if key == "model.format"
    ));
    assert!(matches!(
        one_state("answer = true; model = 1;"),
        Err(Error::UnknownKey { key, .. }) if key == "policy.state.a.model"
    ));
    assert_eq!(
        problem_of(function(r#"{ system = "s"; initial = "b"; state.a.answer = true; }"#)),
        ("policy.initial".into(), FunctionProblem::UnknownState("b".into()))
    );
    let sampling = |body: &str| {
        function_on(
            &plain(""),
            &format!(r#"{{ format = agl.formats."qwen3.8"; weights = "m.gguf"; sampling = {{ {body} }}; }}"#),
        )
    };
    assert!(matches!(
        sampling("seed = 1;"),
        Err(Error::UnknownKey { key, .. }) if key == "model.sampling.seed"
    ));
    assert!(matches!(
        sampling("top_k = 2.5;"),
        Err(Error::WrongType { key, .. }) if key == "model.sampling.top_k"
    ));
    assert_eq!(
        sampling("temperature = 1;").unwrap().sampling.temperature,
        Some(1.0)
    );
    // 0 is the no limit (AGL-021, D2); the absent key keeps the 50 (D40).
    assert_eq!(function(&plain("max_steps_total = 0;")).unwrap().max_steps_total, None);
    assert_eq!(
        problem_of(function(&plain("max_steps_total = -1;"))),
        ("policy.max_steps_total".into(), FunctionProblem::NotPositive)
    );
    let compaction = function(&plain(r#"compact = "Sum up."; max_compact_tokens = 512;"#)).unwrap();
    assert_eq!(
        (compaction.compact.as_str(), compaction.max_compact_tokens),
        ("Sum up.", Some(512))
    );
    // A set limit is the limit at any context.
    assert_eq!(compaction.compact_tokens(1_048_576), 512);
    assert_eq!(
        problem_of(function(&plain("max_compact_tokens = 0;"))),
        ("policy.max_compact_tokens".into(), FunctionProblem::NotPositive)
    );
    // The keys of the model (AGL-039, D2): `compact_early` a boolean.
    let model = |extra: &str| {
        function_on(
            &plain(""),
            &format!(r#"{{ format = agl.formats."qwen3.8"; weights = "m.gguf"; {extra} }}"#),
        )
    };
    assert!(model("compact_early = true;").unwrap().compact_early);
    assert!(model(r#"compact_early = "yes";"#).is_err());
    assert_eq!(model("max_stirs = 0;").unwrap().max_stirs, 0);
    assert!(!model("preserve_thinking = false;").unwrap().preserve_thinking);
    // The policy has none of them, nor the voice limit (AGL-039, D20).
    for key in ["compact_early = true;", "max_record_seconds = 15;", "sampling = { };"] {
        assert!(
            matches!(function(&plain(key)), Err(Error::UnknownKey { .. })),
            "{key}"
        );
    }
}

#[test]
fn the_model_is_checked_against_its_kind() {
    // AGL-033, D1; AGL-036, D5; AGL-039, D19, D21, D24.
    let model = |body: &str| function_on(&plain(""), &format!(r#"{{ format = agl.formats."qwen3.8"; {body} }}"#));
    let llama = model(r#"weights = "m.gguf"; context = 4096; mmproj = "p.gguf";"#).unwrap();
    assert_eq!(llama.model.kind(), ServerKind::Llama);
    assert_eq!(llama.model.model.as_deref(), Some("m.gguf"));
    assert_eq!(
        problem_of(model("context = 4096;")),
        ("model.weights".into(), FunctionProblem::ServerModelMissing)
    );
    assert_eq!(
        problem_of(model(r#"weights = "m.gguf"; strata_config = "/s.json";"#)),
        (
            "model.strata_config".into(),
            FunctionProblem::ServerKeyOfOtherKind("strata_config".into())
        )
    );
    let strata = model(r#"kind = "strata"; strata_config = "/s.json"; compact_early = true;"#).unwrap();
    assert_eq!(strata.model.kind(), ServerKind::Strata);
    assert_eq!(strata.model.strata_config.as_deref(), Some("/s.json"));
    assert_eq!(
        problem_of(model(r#"kind = "strata"; strata_config = "/s.json"; weights = "m.gguf";"#)),
        ("model.weights".into(), FunctionProblem::ServerKeyOfOtherKind("weights".into()))
    );
    assert_eq!(
        problem_of(model(r#"kind = "strata";"#)),
        ("model.strata_config".into(), FunctionProblem::ServerStrataConfigMissing)
    );
    assert_eq!(
        problem_of(model(r#"kind = "strata"; strata_config = "s.json";"#)),
        ("model.strata_config".into(), FunctionProblem::NotAbsolute)
    );
    assert!(matches!(model(r#"kind = "gemma"; weights = "m.gguf";"#), Err(Error::UnknownServerKind { .. })));
    // A weights file is a file name or an absolute path (D24a).
    assert!(model(r#"weights = "/w/m.gguf";"#).is_ok());
    for name in ["dir/m.gguf", ".m", ""] {
        assert_eq!(
            problem_of(model(&format!(r#"weights = "{name}";"#))),
            ("model.weights".into(), FunctionProblem::BadWeightsName),
            "{name}"
        );
    }
    // `source` names the files the model sets (D5).
    assert!(model(r#"weights = "m.gguf"; mmproj = "p.gguf"; source = { weights = "https://x/m"; mmproj = "https://x/p"; };"#).is_ok());
    assert_eq!(
        problem_of(model(r#"weights = "m.gguf"; source.draft = "https://x/d";"#)),
        ("model.source.draft".into(), FunctionProblem::SourceWithoutFile)
    );
    assert!(matches!(
        model(r#"weights = "m.gguf"; source.model = "https://x/m";"#),
        Err(Error::UnknownKey { key, .. }) if key == "model.source.model"
    ));
}

#[test]
fn state_errors() {
    use FunctionProblem::*;
    let anchor = PatternError {
        offset: 0,
        kind: PatternErrorKind::Anchor,
    };
    let cases = [
        (r#"instruction = "x";"#, "policy.state.a", NoActions),
        (
            r#"answer = true; next.otherwise = "a";"#,
            "policy.state.a.next",
            NextWithoutTools,
        ),
        (
            r#"tools.bash = { }; next.cat.any = "a";"#,
            "policy.state.a.next.cat",
            BadTransitionKey,
        ),
        (
            r#"tools.bash = { }; next.bash.oops = "a";"#,
            "policy.state.a.next.bash.oops",
            BadTransitionKey,
        ),
        (
            r#"tools.edit = { }; next.edit.timeout = "a";"#,
            "policy.state.a.next.edit.timeout",
            BadTransitionKey,
        ),
        (
            r#"tools.bash = { }; next = { edit.any = "a"; otherwise = "a"; };"#,
            "policy.state.a.next.edit",
            BadTransitionKey,
        ),
        (
            r#"tools.bash = { }; next.bash.ok = "a";"#,
            "policy.state.a.next",
            MissingTransition {
                tool: "bash".into(),
                outcome: "fail",
            },
        ),
        (
            "tools.bash = { };",
            "policy.state.a.next",
            MissingTransition {
                tool: "bash".into(),
                outcome: "ok",
            },
        ),
        (
            r#"tools.bash = { }; next.otherwise = "b";"#,
            "policy.state.a.next.otherwise",
            UnknownState("b".into()),
        ),
        (
            r#"answer = true; max_steps = 0; on_limit = "a";"#,
            "policy.state.a.max_steps",
            NotPositive,
        ),
        (
            r#"answer = true; on_limit = "a";"#,
            "policy.state.a.on_limit",
            OnLimitWithoutLimit,
        ),
        (
            r#"answer = true; max_visits = 1; on_limit = "a";"#,
            "policy.state.a.on_limit",
            OnLimitCycle,
        ),
        (
            r#"tools.bash.command.pattern = "^ls"; next.otherwise = "a";"#,
            "policy.state.a.tools.bash.command.pattern",
            Pattern(anchor),
        ),
        (
            r#"tools.bash.command.pattern = "[^;]*"; next.otherwise = "a";"#,
            "policy.state.a.tools.bash.command.pattern",
            PatternMayEndValue { end: '\n' },
        ),
        (
            r#"tools.edit.path.enum = [ ]; next.otherwise = "a";"#,
            "policy.state.a.tools.edit.path.enum",
            EmptyEnum,
        ),
        (
            r#"tools.edit.path = { enum = [ "a" ]; pattern = "a"; }; next.otherwise = "a";"#,
            "policy.state.a.tools.edit.path",
            ConstraintKeys,
        ),
        (
            r#"tools.edit.path = { }; next.otherwise = "a";"#,
            "policy.state.a.tools.edit.path",
            ConstraintKeys,
        ),
        (
            r#"tools.bash.timeout.int = { min = 0; max = 5; }; next.otherwise = "a";"#,
            "policy.state.a.tools.bash.timeout.int",
            BadRange,
        ),
        (
            r#"tools.bash.timeout.int = { min = 9; max = 5; }; next.otherwise = "a";"#,
            "policy.state.a.tools.bash.timeout.int",
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
    // `next.<tool>` is always a set (AGL-039, D41).
    assert!(matches!(
        one_state(r#"tools.bash = { }; next.bash = "a";"#),
        Err(Error::WrongType { key, .. }) if key == "policy.state.a.next.bash"
    ));
}

#[test]
fn key_errors() {
    for (body, key) in [
        ("tools.cat = { };", "policy.state.a.tools.cat"),
        ("tools.bash.cmd = { };", "policy.state.a.tools.bash.cmd"),
        (
            r#"tools.edit.path.regex = "a";"#,
            "policy.state.a.tools.edit.path.regex",
        ),
        (
            r#"tools.bash.timeout.pattern = "1";"#,
            "policy.state.a.tools.bash.timeout.pattern",
        ),
    ] {
        let result = one_state(&format!(r#"{body} next.otherwise = "a";"#));
        assert!(
            matches!(&result, Err(Error::UnknownKey { key: k, .. }) if k == key),
            "{body}: {result:?}"
        );
    }
    assert!(matches!(
        one_state("answer = true; max_steps = 2;"),
        Err(Error::MissingKey { key, .. }) if key == "policy.state.a.on_limit"
    ));
    assert!(matches!(
        one_state(r#"tools.bash.timeout.int.min = 1; next.otherwise = "a";"#),
        Err(Error::MissingKey { key, .. }) if key == "policy.state.a.tools.bash.timeout.int.max"
    ));
}

#[test]
fn constraints_are_read() {
    let function = one_state(
        r#"tools.bash.timeout.int = { min = 5; max = 3600; };
           tools.edit.path.enum = [ "src/lib.rs" ];
           next.otherwise = "a";"#,
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
            r#"{ a = { answer = true; max_visits = 1; on_limit = "b"; }; b = { answer = true; max_visits = 1; on_limit = "a"; }; }"#
        )),
        ("policy.state.a.on_limit".into(), FunctionProblem::OnLimitCycle)
    );
    assert_eq!(
        problem_of(with_states("{ a.answer = true; z.answer = true; }")),
        ("policy.state.z".into(), FunctionProblem::Unreachable)
    );
    // A state reached only through `on_limit` is reachable.
    with_states(
        r#"{ a = { tools.bash = { }; max_steps = 2; on_limit = "z"; next.otherwise = "a"; }; z.answer = true; }"#,
    )
    .unwrap();
}

#[test]
fn thinking_and_formats() {
    // A format of the Function's own, without thinking (AGL-039, D18).
    let plain_format = r#"{ call = { begin = "<c {name}>"; param = "<p {name}>{value}</p>"; end = "</c>"; }; }"#;
    let load = |think: &str| {
        function_on(
            &format!(r#"{{ system = "s"; initial = "a"; state.a = {{ answer = true; {think} }}; }}"#),
            &format!(r#"{{ format = {plain_format}; weights = "m.gguf"; }}"#),
        )
    };
    assert!(!load("").unwrap().states[0].think);
    assert_eq!(
        problem_of(load("think = true;")),
        (
            "policy.state.a.think".into(),
            FunctionProblem::NoThinking {
                format: "model.format".into()
            }
        )
    );
}

#[test]
fn the_system_text_gets_the_two_words_of_the_runtime_and_keeps_other_braces() {
    let f = function(&format!(
        "{{ max_action_tokens = 500; system = {}; initial = \"a\"; state.a.answer = true; }}",
        nix::string("Prefix {runtime_prefix}, at most {max_action_tokens} tokens, keep {x} and {{y}}.")
    ))
    .unwrap();
    // The prefix is filled at load, the action's limit where the context
    // is known (AGL-043 F14).
    assert_eq!(
        f.system,
        "Prefix [agentLIBRE runtime], at most {max_action_tokens} tokens, keep {x} and {{y}}."
    );
    assert_eq!(
        f.system_at(131_072),
        "Prefix [agentLIBRE runtime], at most 500 tokens, keep {x} and {{y}}."
    );
    // Without the limit set, the share of the context is told.
    let g = function(r#"{ system = "{max_action_tokens}"; initial = "a"; state.a.answer = true; }"#).unwrap();
    assert_eq!(g.system_at(131_072), "16384");
}

#[test]
fn the_built_in_functions_differ_in_the_system_and_the_step_limit() {
    let high = Function::load(none(), none(), "agentLIBRE-gpu-high").unwrap();
    let minimal = Function::load(none(), none(), "agentLIBRE-gpu-high-minimal").unwrap();
    // `minimal` is laconic and nameless (AGL-018, D1; AGL-039, D31). The
    // other is the persona and the runtime part: the prefix and the size
    // of an action (AGL-009, AGL-018, D2).
    assert_eq!(
        minimal.system,
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
    assert!(high.system.starts_with("You are DinoBot"));
    // The action's limit is told at the context of the run (AGL-043 F14).
    let told = high.system_at(131_072);
    assert!(told.contains(
        "Messages that start with [agentLIBRE runtime] come from the agentLIBRE runtime, \
         not from the human. One action holds at most 16384 tokens; write a large file in parts."
    ));
    assert!(!told.contains('{'));
    // The step limit differs too: `minimal` keeps the 50 of the absent key
    // (D40); the other is unlimited (AGL-021, D1).
    assert_eq!(minimal.max_steps_total, Some(50));
    assert_eq!(high.max_steps_total, None);
    // The rest is the same: the states, the tools, the model.
    assert_eq!(minimal.states.len(), high.states.len());
    let tools = |f: &Function| -> Vec<String> {
        f.states[f.initial]
            .actions
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect()
    };
    assert_eq!(tools(&minimal), tools(&high));
    assert_eq!(minimal.sampling, high.sampling);
    assert_eq!(minimal.model, high.model);
    // They are the built-ins there are (AGL-039, D28).
    assert_eq!(
        builtin_names().unwrap(),
        ["agentLIBRE-gpu-high", "agentLIBRE-gpu-high-minimal"]
    );
}

#[test]
fn the_search_tool_is_read_with_its_parameters_and_constraints() {
    let function = one_state(r#"tools.search = { }; answer = true; next.otherwise = "a";"#).unwrap();
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
        r#"tools.search = { source.enum = [ "wikipedia" ]; limit.int = { min = 1; max = 3; }; }; answer = true; next.otherwise = "a";"#,
    )
    .unwrap();
    let tool = narrowed.states[0].actions.tools.iter().find(|t| t.name == "search").unwrap();
    assert_eq!(tool.params[3].constraint, Constraint::OneOf(vec!["wikipedia".into()]));
    assert_eq!(tool.params[1].constraint, Constraint::Int { min: 1, max: 3 });
    // A parameter the tool has not is an error, as for the others.
    assert!(one_state("tools.search.path = { }; answer = true;").is_err());
}

#[test]
fn the_built_in_functions_have_the_search_tool() {
    for name in builtin_names().unwrap() {
        let function = Function::load(none(), none(), &name).unwrap();
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
        r#"{
          a = { tools.search = { }; next.otherwise = "b"; };
          b = { tools = { bash = { }; search = { }; }; answer = true; next.otherwise = "b"; };
        }"#,
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
    let mut only = with_states(r#"{ a = { tools.search = { }; next.otherwise = "a"; }; }"#).unwrap();
    only.states[0].actions.tools.retain(|t| t.name == "search");
    only.without_tool("search");
    assert!(only.states[0].actions.tools.is_empty());
    assert!(only.states[0].actions.answer);
}

#[test]
fn a_tools_output_follows_the_context_unless_the_function_says() {
    // AGL-049, D3b.
    let plain = function(r#"{ system = "s"; initial = "a"; state.a.answer = true; }"#).unwrap();
    assert_eq!(plain.output_tokens(128 * 1024), 4096);
    assert_eq!(plain.output_tokens(256 * 1024), 8192);
    assert_eq!(plain.output_tokens(64 * 1024), 2048);
    let set = function(
        r#"{ system = "s"; max_tool_output_tokens = 1000; initial = "a"; state.a.answer = true; }"#,
    )
    .unwrap();
    assert_eq!(set.output_tokens(256 * 1024), 1000);
    assert!(
        function(
            r#"{ system = "s"; max_tool_output_tokens = 0; initial = "a"; state.a.answer = true; }"#
        )
        .is_err()
    );
}

