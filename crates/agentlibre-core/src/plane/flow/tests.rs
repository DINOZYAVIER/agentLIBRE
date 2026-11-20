use super::*;
use super::journal;
use std::time::Instant;
use crate::machine::Event;
use crate::forge::testutil::TempDir;
use std::fs;

/// A flow of the shape D9 gives, as a file: `nix::eval_file` reads a file as
/// a function of `{ agl, ... }`, as it does a Function.
const DEV: &str = r#"{ agl, ... }: {
  flow.dev-loop = {
    initial = "build";
    max_steps_total = 40;
    state.build = {
      action.command = { cmd = "cargo build"; timeout = 900; };
      next.exit.ok = "check";
      next.exit.any = "rollback";
    };
    state.check = {
      action.review = { function = "reviewer"; events = [ "ok" "flaky" "broken" ]; };
      next.review.ok = "launch";
      next.review.any = "rollback";
    };
    state.launch = { next.otherwise = "halted"; };
    state.rollback = {
      action.artifact = { dir = "result"; keep = 3; };
      action.say = "agl:dev";
      max_visits = 2;
      on_limit = "halted";
      next.otherwise = "halted";
    };
    state.halted = { };
  };
}
"#;

/// Writes `text` as the plane's `flow.nix` and reads it back; `dir` outlives
/// the read, so a test that fails keeps its file for the message.
fn read(dir: &TempDir, text: &str) -> Result<FlowFile, Error> {
    fs::write(dir.0.join(FLOW_FILE), text).unwrap();
    FlowFile::load(&dir.0)
}

fn problem_of(text: &str) -> (String, FunctionProblem) {
    let dir = TempDir::new();
    let error = read(&dir, text).expect_err("a flow that does not load");
    match error {
        Error::Function { key, problem, .. } => (key, problem),
        other => panic!("not a load problem: {other:?}"),
    }
}

#[test]
fn no_file_is_no_flows() {
    let dir = TempDir::new();
    assert!(FlowFile::load(&dir.0).unwrap().flows.is_empty());
}

#[test]
fn a_flow_loads_with_its_states_and_actions() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let flow = flows.find("dev-loop").expect("the declared flow");
    assert_eq!(flow.initial(), "build");
    assert_eq!(flow.states().len(), 5);
    let at = |name: &str| flow.states().iter().position(|s| s.name == name).unwrap();
    assert_eq!(
        flow.actions(at("build")),
        [Action::Command {
            cmd: "cargo build".into(),
            timeout: Duration::from_secs(900),
        }]
    );
    assert_eq!(
        flow.actions(at("rollback")),
        [
            Action::Artifact {
                dir: PathBuf::from("result"),
                keep: 3,
            },
            Action::Say {
                session: "agl:dev".into()
            },
        ]
    );
}

#[test]
fn the_table_moves_on_the_flows_own_events() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let flow = flows.find("dev-loop").unwrap();
    let states = flow.states();
    let at = |name: &str| states.iter().position(|s| s.name == name).unwrap();
    // `exit.any` covers the three outcomes the file does not name (D43).
    assert_eq!(states[at("build")].next_named("exit", Some("ok")), at("check"));
    assert_eq!(
        states[at("build")].next_named("exit", Some("timeout")),
        at("rollback")
    );
    assert_eq!(
        states[at("check")].next_named("review", Some("flaky")),
        at("rollback")
    );
    assert_eq!(flow.table().states.len(), 5);
}

#[test]
fn a_command_without_its_exit_transition_is_named() {
    let (key, problem) = problem_of(
        r#"{ agl, ... }: { flow.one = { initial = "build"; state.build.action.command = { cmd = "true"; timeout = 1; }; }; }"#,
    );
    assert_eq!(key, "flow.one.state.build.next");
    assert_eq!(
        problem,
        FunctionProblem::MissingTransition {
            tool: "exit".into(),
            outcome: "ok".into(),
        }
    );
}

#[test]
fn a_review_needs_a_transition_for_each_of_its_events() {
    let (key, problem) = problem_of(
        r#"{ agl, ... }: { flow.one = { initial = "check"; state.check = { action.review = { function = "r"; events = [ "ok" "broken" ]; }; next.review.ok = "check"; }; }; }"#,
    );
    assert_eq!(key, "flow.one.state.check.next");
    assert_eq!(
        problem,
        FunctionProblem::MissingTransition {
            tool: "review".into(),
            outcome: "broken".into(),
        }
    );
}

#[test]
fn an_unknown_target_names_the_key_a_human_would_edit() {
    let (key, problem) = problem_of(
        r#"{ agl, ... }: { flow.one = { initial = "a"; state.a = { next.otherwise = "gone"; }; }; }"#,
    );
    assert_eq!(key, "flow.one.state.a.next.otherwise");
    assert_eq!(problem, FunctionProblem::UnknownState("gone".into()));
}

#[test]
fn on_limit_without_a_limit_is_refused() {
    let (key, problem) = problem_of(
        r#"{ agl, ... }: { flow.one = { initial = "a"; state.a = { on_limit = "a"; next.otherwise = "a"; }; }; }"#,
    );
    assert_eq!(key, "flow.one.state.a.on_limit");
    assert_eq!(problem, FunctionProblem::OnLimitWithoutLimit);
}

#[test]
fn a_key_a_flow_has_not_is_refused() {
    // A flow's state has no tools: those belong to a turn (D4).
    let dir = TempDir::new();
    let error = read(
        &dir,
        r#"{ agl, ... }: { flow.one = { initial = "a"; state.a = { tools.bash = { }; next.otherwise = "a"; }; }; }"#,
    )
    .expect_err("a flow that does not load");
    assert!(
        matches!(&error, Error::UnknownKey { key, .. } if key == "flow.one.state.a.tools"),
        "{error:?}"
    );
}

#[test]
fn a_started_flow_writes_where_it_stands() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    let run = FlowRun::start(decl, &dir.0).unwrap();
    assert_eq!(run.state(), "build");
    let entries = journal::read(&journal::path(&dir.0, "dev-loop")).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(matches!(&entries[0], journal::Entry::Started { state, .. } if state == "build"));
}

#[test]
fn a_journal_replays_to_the_position_it_left() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    {
        let mut run = FlowRun::start(decl, &dir.0).unwrap();
        run.on_event(decl, Event::Exit(Outcome::Ok)).unwrap();
        run.on_event(decl, Event::Answer {
            check: "review".into(),
            answer: "flaky".into(),
        })
        .unwrap();
    }
    // The daemon restarts: the position comes from the file, not from memory.
    let entries = journal::read(&journal::path(&dir.0, "dev-loop")).unwrap();
    assert_eq!(
        journal::fold(decl, &journal::path(&dir.0, "dev-loop"), &entries)
            .unwrap()
            .state
            .as_deref(),
        Some("rollback")
    );
    let run = FlowRun::resume(decl, &dir.0).unwrap();
    assert_eq!(run.state(), "rollback");
}

#[test]
fn the_trail_names_the_states_the_journal_walked() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    let mut run = FlowRun::start(decl, &dir.0).unwrap();
    run.on_event(decl, Event::Exit(Outcome::Ok)).unwrap();
    run.on_event(decl, Event::Answer {
        check: "review".into(),
        answer: "broken".into(),
    })
    .unwrap();
    drop(run);
    assert_eq!(
        FlowRun::trail(&dir.0, decl).unwrap(),
        ["build", "check", "rollback"]
    );
}

#[test]
fn a_stopped_flow_keeps_where_it_stood() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    let mut run = FlowRun::start(decl, &dir.0).unwrap();
    run.on_event(decl, Event::Exit(Outcome::Fail)).unwrap();
    run.stop().unwrap();
    let run = FlowRun::resume(decl, &dir.0).unwrap();
    assert_eq!(run.state(), "rollback");
}

#[test]
fn a_line_that_is_not_an_entry_is_named() {
    let dir = TempDir::new();
    fs::create_dir_all(dir.0.join(journal::JOURNAL_DIR)).unwrap();
    fs::write(dir.0.join(journal::path(&dir.0, "one")), "not json\n").unwrap();
    let error = journal::read(&journal::path(&dir.0, "one")).unwrap_err();
    assert!(matches!(error, Error::PlaneFile { .. }), "{error:?}");
}

#[test]
fn an_event_the_state_has_no_transition_for_moves_nothing() {
    // `build` covers `exit.*` only: a turn's end there is written and ignored.
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    let mut run = FlowRun::start(decl, &dir.0).unwrap();
    run.on_event(decl, Event::TurnEnd).unwrap();
    assert_eq!(run.state(), "build");
    drop(run);
    assert_eq!(FlowRun::trail(&dir.0, decl).unwrap(), ["build", "build"]);
}

#[test]
fn a_commands_exit_code_is_the_verdict() {
    let dir = TempDir::new();
    assert_eq!(
        act::run(&dir.0, "true", Duration::from_secs(10)).unwrap(),
        Outcome::Ok
    );
    assert_eq!(
        act::run(&dir.0, "false", Duration::from_secs(10)).unwrap(),
        Outcome::Fail
    );
    assert_eq!(
        act::run(&dir.0, "exit 3", Duration::from_secs(10)).unwrap(),
        Outcome::Fail
    );
}

#[test]
fn a_command_that_outlives_its_timeout_is_a_timeout() {
    let dir = TempDir::new();
    let at = Instant::now();
    assert_eq!(
        act::run(&dir.0, "sleep 30", Duration::from_millis(200)).unwrap(),
        Outcome::Timeout
    );
    assert!(at.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_command_runs_in_the_directory_it_is_given() {
    let dir = TempDir::new();
    assert_eq!(
        act::run(&dir.0, "touch here", Duration::from_secs(10)).unwrap(),
        Outcome::Ok
    );
    assert!(dir.0.join("here").is_file());
}

/// `DEV` with `exit.ok` moved: what a human editing `flow.nix` under a
/// running flow leaves behind (D11).
const MOVED: &str = r#"{ agl, ... }: {
  flow.dev-loop = {
    initial = "build";
    state.build = {
      action.command = { cmd = "cargo build"; timeout = 9; };
      next.exit.ok = "halted";
      next.exit.any = "halted";
    };
    state.halted = { };
  };
}
"#;

/// A rollback that points at an artifact and launches it (D10).
const LAUNCH: &str = r#"{ agl, ... }: {
  flow.back = {
    initial = "rollback";
    state.rollback = {
      action.artifact = { dir = "result"; keep = 3; };
      action.launch = { cmd = "result-prev/bin/agl"; timeout = 30; };
      next.launch.ok = "halted";
      next.launch.fail = "halted";
      next.launch.timeout = "halted";
      next.launch.interrupted = "halted";
    };
    state.halted = { };
  };
}
"#;

#[test]
fn a_rollback_points_at_the_artifact_and_launches_it() {
    let dir = TempDir::new();
    let flows = read(&dir, LAUNCH).unwrap();
    let decl = flows.find("back").unwrap();
    let at = decl
        .states()
        .iter()
        .position(|s| s.name == "rollback")
        .unwrap();
    let actions = decl.actions(at);
    assert!(
        matches!(
            actions,
            [Action::Launch { cmd, timeout }, Action::Artifact { keep: 3, .. }]
              if cmd == "result-prev/bin/agl" && *timeout == Duration::from_secs(30)
        ),
        "{actions:?}"
    );
}

#[test]
fn a_launch_without_its_transition_is_named() {
    let (key, problem) = problem_of(
        r#"{ agl, ... }: { flow.one = { initial = "back"; state.back = { action.launch = { cmd = "run"; timeout = 1; }; }; }; }"#,
    );
    assert_eq!(key, "flow.one.state.back.next");
    assert_eq!(
        problem,
        FunctionProblem::MissingTransition {
            tool: "launch".into(),
            outcome: "ok".into(),
        }
    );
}

#[test]
fn an_edited_declaration_under_a_running_flow_is_refused() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    {
        let mut run = FlowRun::start(decl, &dir.0).unwrap();
        run.on_event(decl, Event::Exit(Outcome::Ok)).unwrap();
    }
    // The same file now says `exit.ok` goes to `halted`: the running flow must
    // not change shape under the human (D11).
    let moved = read(&dir, MOVED).unwrap();
    let moved = moved.find("dev-loop").unwrap();
    let error = FlowRun::resume(moved, &dir.0).unwrap_err();
    assert!(matches!(error, Error::PlaneFile { .. }), "{error:?}");
}

#[test]
fn a_reset_starts_again_and_keeps_the_journal() {
    let dir = TempDir::new();
    let flows = read(&dir, DEV).unwrap();
    let decl = flows.find("dev-loop").unwrap();
    {
        let mut run = FlowRun::start(decl, &dir.0).unwrap();
        run.on_event(decl, Event::Exit(Outcome::Ok)).unwrap();
    }
    let moved = read(&dir, MOVED).unwrap();
    let moved = moved.find("dev-loop").unwrap();
    journal::reset(&dir.0, "dev-loop").unwrap();
    let run = FlowRun::resume(moved, &dir.0).unwrap();
    assert_eq!(run.state(), "build");
    assert!(dir.0.join("flow").join("dev-loop.jsonl.old").is_file());
}
