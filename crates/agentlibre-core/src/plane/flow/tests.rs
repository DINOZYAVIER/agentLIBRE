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
      action.wake = "agl:dev";
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
            Action::Wake {
                session: "agl:dev".into(),
                text: None,
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
        run.on_event(decl, Event::Verdict {
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
    run.on_event(decl, Event::Verdict {
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

/// A flow whose commands are the shell's own, so a test runs them (D5).
const WORK: &str = r#"{ agl, ... }: {
  flow.work = {
    initial = "build";
    state.build = {
      action.command = { cmd = "true"; timeout = 10; };
      next.exit.ok = "check";
      next.exit.any = "halted";
    };
    state.check = {
      action.review = { function = "reviewer"; events = [ "ok" "broken" ]; };
      next.review.any = "halted";
    };
    state.halted = { };
  };
}
"#;

/// Two states that only run a command and hand the run to each other (D9).
const CHASE: &str = r#"{ agl, ... }: {
  flow.chase = {
    initial = "one";
    max_steps_total = 3;
    state.one = {
      action.command = { cmd = "true"; timeout = 9; };
      next.exit.any = "two";
    };
    state.two = {
      action.command = { cmd = "true"; timeout = 9; };
      next.exit.any = "one";
    };
  };
  flow.tail = {
    initial = "one";
    state.one = {
      action.command = { cmd = "true"; timeout = 9; };
      next.exit.any = "two";
    };
    state.two = {
      action.command = { cmd = "true"; timeout = 9; };
      next.exit.any = "one";
    };
  };
}
"#;

#[test]
fn a_flow_that_was_never_started_has_no_place() {
    let dir = TempDir::new();
    let flows = read(&dir, WORK).unwrap();
    let place = drive::place(flows.find("work").unwrap(), &dir.0).unwrap();
    assert_eq!(place, drive::Place::default());
}

#[test]
fn a_states_command_runs_and_its_exit_moves_the_flow() {
    let dir = TempDir::new();
    let flows = read(&dir, WORK).unwrap();
    let place = drive::start(flows.find("work").unwrap(), &dir.0, &dir.0, None).unwrap();
    assert_eq!(place.state.as_deref(), Some("check"));
    assert_eq!(place.tail, ["started build", "exit.ok -> check"]);
    // The review turn is the plane's to start; the flow waits for it (D6).
    assert_eq!(
        place.waiting,
        [drive::Waiting::Review {
            function: "reviewer".into(),
            events: vec!["ok".into(), "broken".into()],
        }]
    );
}

#[test]
fn a_failing_command_takes_the_transition_written_for_it() {
    let dir = TempDir::new();
    // The same flow with a command that ends badly.
    fs::write(
        dir.0.join(FLOW_FILE),
        WORK.replace("cmd = \"true\"", "cmd = \"exit 7\""),
    )
    .unwrap();
    let flows = FlowFile::load(&dir.0).unwrap();
    let place = drive::start(flows.find("work").unwrap(), &dir.0, &dir.0, None).unwrap();
    assert_eq!(place.state.as_deref(), Some("halted"));
    assert_eq!(place.tail, ["started build", "exit.fail -> halted"]);
}

#[test]
fn a_stopped_flow_goes_on_from_where_it_stood() {
    let dir = TempDir::new();
    let flows = read(&dir, WORK).unwrap();
    let decl = flows.find("work").unwrap();
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    let place = drive::stop(decl, &dir.0, None).unwrap();
    assert!(place.stopped);
    assert_eq!(place.state.as_deref(), Some("check"));
    let place = drive::start(decl, &dir.0, &dir.0, None).unwrap();
    assert!(!place.stopped, "the flow moves again (D7)");
    assert_eq!(place.state.as_deref(), Some("check"));
    assert_eq!(
        FlowRun::trail(&dir.0, decl).unwrap(),
        // started, the exit, the stop that moved nothing, the start again.
        ["build", "check", "check"]
    );
}

#[test]
fn the_steps_of_a_whole_run_bound_it() {
    let dir = TempDir::new();
    let flows = read(&dir, CHASE).unwrap();
    let decl = flows.find("chase").unwrap();
    let place = drive::start(decl, &dir.0, &dir.0, None).unwrap();
    // `max_steps_total = 3`: three commands, and the run stops where the
    // third left it (D33).
    assert_eq!(place.state.as_deref(), Some("two"));
    assert_eq!(place.tail.len(), 4);
}

#[test]
fn a_declaration_that_chases_its_tail_is_refused() {
    let dir = TempDir::new();
    let flows = read(&dir, CHASE).unwrap();
    let decl = flows.find("tail").unwrap();
    let error = drive::start(decl, &dir.0, &dir.0, None).unwrap_err();
    assert!(
        matches!(&error, Error::PlaneFile { detail, .. } if detail.contains("max_visits")),
        "{error:?}"
    );
}

/// A verb's answer as one string: its told lines in order.
fn told(said: crate::plane::Text) -> String {
    said.lines
        .iter()
        .map(|line| match line {
            crate::plane::Told::Output(text)
            | crate::plane::Told::Notice(text)
            | crate::plane::Told::Refusal(text)
            | crate::plane::Told::Fault(text)
            | crate::plane::Told::Pending(text) => text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_verb_answers_what_the_flow_does_and_where_it_stands() {
    let dir = TempDir::new();
    read(&dir, WORK).unwrap();
    let run = |verb: Verb| drive::run(&dir.0, &dir.0, &verb, crate::i18n::Lang::En, None);
    assert_eq!(told(run(Verb::List)), "work: not started");
    assert_eq!(
        told(run(Verb::Start { name: "work".into() })).replace('\n', " | "),
        "work: check |   waits for review reviewer: ok|broken |   started build |   exit.ok -> check"
    );
    assert_eq!(told(run(Verb::List)), "work: check");
    assert_eq!(
        told(run(Verb::Status { name: "nope".into() })),
        "no flow `nope` in `flow.nix`"
    );
    assert!(told(run(Verb::Reset { name: "work".into() })).starts_with("work: reset to `build`"));
    assert_eq!(told(run(Verb::List)), "work: not started");
}

/// A flow that stands in `wait` while its `timer` runs (D9, D16).
const CLOCK: &str = r#"{ agl, ... }: {
  flow.clock = {
    initial = "wait";
    state.wait = { timer = 1; next.timer = "ring"; };
    state.ring = {
      action.command = { cmd = "true"; timeout = 9; };
      next.exit.any = "done";
    };
    state.done = { };
  };
}
"#;

#[test]
fn a_states_timer_is_read_and_its_zero_is_named() {
    let dir = TempDir::new();
    let flows = read(&dir, CLOCK).unwrap();
    let flow = flows.find("clock").expect("the declared flow");
    let at = |name: &str| flow.states().iter().position(|s| s.name == name).unwrap();
    assert_eq!(flow.timer(at("wait")), Some(Duration::from_secs(1)));
    assert_eq!(flow.timer(at("ring")), None);
    let (key, problem) = problem_of(
        r#"{ agl, ... }: {
  flow.clock = {
    initial = "wait";
    state.wait = { timer = 0; next.timer = "wait"; };
  };
}
"#,
    );
    assert_eq!(
        (key.as_str(), problem),
        ("flow.clock.state.wait.timer", FunctionProblem::NotPositive)
    );
}

#[test]
fn a_due_timer_fires_and_moves_the_flow() {
    let dir = TempDir::new();
    let flows = read(&dir, CLOCK).unwrap();
    let decl = flows.find("clock").unwrap();
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    // Not yet run out: the flow stands where it stood (D16).
    assert!(drive::tick(&dir.0, &dir.0, None).is_empty());
    assert_eq!(drive::place(decl, &dir.0).unwrap().state.as_deref(), Some("wait"));
    // Age the journal's last line past the timer, and the tick fires: the
    // `timer` event moves it, and the state it lands on goes on by itself.
    let file = journal::path(&dir.0, "clock");
    let aged = std::fs::read_to_string(&file)
        .unwrap()
        .replace(&format!("\"time\":{}", journal::now()), "\"time\":1");
    fs::write(&file, aged).unwrap();
    assert_eq!(drive::tick(&dir.0, &dir.0, None), ["clock"]);
    assert_eq!(drive::place(decl, &dir.0).unwrap().state.as_deref(), Some("done"));
    // Done has no timer: the next tick has nothing to fire.
    assert!(drive::tick(&dir.0, &dir.0, None).is_empty());
}

#[test]
fn a_stopped_flow_waits_for_no_timer() {
    let dir = TempDir::new();
    let flows = read(&dir, CLOCK).unwrap();
    let decl = flows.find("clock").unwrap();
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    drive::stop(decl, &dir.0, None).unwrap();
    // The human holds a stopped flow: its timer stands with it (D7).
    let file = journal::path(&dir.0, "clock");
    let aged = std::fs::read_to_string(&file)
        .unwrap()
        .replace(&format!("\"time\":{}", journal::now()), "\"time\":1");
    fs::write(&file, aged).unwrap();
    assert!(drive::tick(&dir.0, &dir.0, None).is_empty());
    assert_eq!(drive::place(decl, &dir.0).unwrap().state.as_deref(), Some("wait"));
}

/// A flow whose second state waits for a wake (D6, D20).
const TALK: &str = r#"{ agl, ... }: {
  flow.talk = {
    initial = "ask";
    state.ask = {
      action.command = { cmd = "true"; timeout = 9; };
      next.exit.ok = "stood";
      next.exit.any = "end";
    };
    state.stood = { action.wake = "last"; next.message.done = "end"; };
    state.end = { };
  };
}
"#;

#[test]
fn a_say_waits_until_its_wake_is_delivered() {
    use crate::i18n::Lang;
    let dir = TempDir::new();
    let flows = read(&dir, TALK).unwrap();
    let decl = flows.find("talk").unwrap();
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    // The flow stands in `stood`, and its wake names the session and the
    // line D14 gives it: itself and where it stands.
    assert_eq!(
        drive::wakeups(&dir.0),
        [drive::Wake {
            flow: "talk".into(),
            session: "last".into(),
            text: "flow `talk` stands in `stood`; read `flow status talk`".into(),
        }]
    );
    // The delivery writes the journal's `said` line, and the wake is no
    // longer pending (D14, D20).
    let file = journal::path(&dir.0, "talk");
    journal::append(
        &file,
        &journal::Entry::Woke {
            time: journal::now(),
            session: "last".into(),
            text: "woken".into(),
        },
    )
    .unwrap();
    assert!(drive::wakeups(&dir.0).is_empty());
    // A stopped flow holds its wake with it (D7).
    drive::stop(decl, &dir.0, None).unwrap();
    assert!(drive::wakeups(&dir.0).is_empty());
    let _ = Lang::En;
}

#[test]
fn a_say_table_overrides_the_wakes_line() {
    let dir = TempDir::new();
    fs::write(
        dir.0.join(FLOW_FILE),
        TALK.replace(
            "action.wake = \"last\"",
            "action.wake = { session = \"agl:dev\"; text = \"the build broke\"; }",
        ),
    )
    .unwrap();
    let flows = FlowFile::load(&dir.0).unwrap();
    let decl = flows.find("talk").unwrap();
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    assert_eq!(
        drive::wakeups(&dir.0),
        [drive::Wake {
            flow: "talk".into(),
            session: "agl:dev".into(),
            text: "the build broke".into(),
        }]
    );
}

#[test]
fn the_holder_asks_once_for_each_wake_and_records_the_delivery() {
    use super::holder::{self, Letter, Need};
    use crate::i18n::Lang;
    use std::time::Duration;

    let dir = TempDir::new();
    let (to, from) = std::sync::mpsc::channel();
    let handle = holder::Holder::spawn(dir.0.clone(), dir.0.clone(), Some(to));
    read(&dir, TALK).unwrap();
    handle.ask(Verb::Start { name: "talk".into() }, Lang::En);
    // The verb that left a flow standing in a `say` asked for its wake (D18).
    let need = from.recv_timeout(Duration::from_secs(5)).expect("no need sent");
    let Need::Wake { plane_dir, wake } = need else {
        panic!("not a wake");
    };
    assert_eq!(plane_dir, dir.0);
    assert_eq!(wake.session, "last");
    // A poke while the wake is in flight asks nothing twice (D20).
    handle.post(Letter::Poke);
    // A second ask would wait behind nothing; `wakeups` answers the double.
    drop(from.recv_timeout(Duration::from_millis(300)));
    assert!(drive::wakeups(&dir.0).len() == 1, "asked twice");
    // The delivery comes back, the `said` line goes to the journal, and no
    // wake stands (D14, D20); a later ask waits behind it, so the journal
    // is written by then.
    handle.post(Letter::Delivered {
        flow: wake.flow.clone(),
        session: wake.session,
        text: wake.text.clone(),
    });
    let status = handle.ask(Verb::Status { name: "talk".into() }, Lang::En);
    assert!(status.content.as_deref().is_some_and(|t| t.contains("stood")), "no status");
    assert!(drive::wakeups(&dir.0).is_empty());
    let entries = journal::read(&journal::path(&dir.0, "talk")).unwrap();
    assert!(
        matches!(entries.last(), Some(journal::Entry::Woke { text, .. }) if text == &wake.text),
        "{entries:?}"
    );
}

#[test]
fn a_review_verdict_moves_the_flow_and_names_its_session() {
    use super::holder::{self, Letter, Need};
    use crate::i18n::Lang;
    use std::time::Duration;

    let dir = TempDir::new();
    let (to, from) = std::sync::mpsc::channel();
    let handle = holder::Holder::spawn(dir.0.clone(), dir.0.clone(), Some(to));
    read(&dir, WORK).unwrap();
    handle.ask(Verb::Start { name: "work".into() }, Lang::En);
    // The check state asks the daemon for its turn (D19).
    let need = from.recv_timeout(Duration::from_secs(5)).expect("no review asked");
    let Need::Review { review, .. } = need else { panic!("not a review") };
    assert_eq!(
        (review.flow.as_str(), review.function.as_str(), review.state.as_str()),
        ("work", "reviewer", "check")
    );
    assert_eq!(review.events, ["ok", "broken"]);
    // The verdict names one of the events; the journal keeps the session
    // that turned beside the step (D5).
    handle.post(Letter::Verdict {
        flow: "work".into(),
        event: Event::Verdict { check: "review".into(), answer: "ok".into() },
        note: Some("session s1".into()),
    });
    let status = handle.ask(Verb::Status { name: "work".into() }, Lang::En);
    assert!(status.content.as_deref().is_some_and(|t| t.contains("halted")), "no move");
    assert!(drive::reviews(&dir.0).is_empty(), "asked again");
    let entries = journal::read(&journal::path(&dir.0, "work")).unwrap();
    assert!(
        matches!(
            entries.last(),
            Some(journal::Entry::Event { name, detail, note, into, .. })
                if name == "review" && detail.as_deref() == Some("ok")
                    && into == "halted" && note.as_deref() == Some("session s1")
        ),
        "{entries:?}"
    );
}

/// A state that names the nudges its agent may deliver (D15).
const GATE: &str = r#"{ agl, ... }: {
  flow.gate = {
    initial = "stood";
    state.stood = { nudges = [ "go" ]; next.message.go = "gone"; next.turn.end = "gone"; };
    state.gone = { };
  };
}
"#;

#[test]
fn a_nudge_delivers_where_the_state_names_it() {
    use crate::i18n::Lang;
    let dir = TempDir::new();
    let flows = read(&dir, GATE).unwrap();
    let decl = flows.find("gate").unwrap();
    let at = decl.states().iter().position(|s| s.name == "stood").unwrap();
    assert_eq!(decl.nudges(at), ["go"]);
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    let run = |verb: Verb| drive::run(&dir.0, &dir.0, &verb, Lang::En, None);
    // The human delivers what the machine declared for it, bounded by no
    // list; an undeclared event simply moves nothing (D21).
    assert!(told(run(Verb::Message { name: "gate".into(), event: "jump".into() })).contains("stood"));
    // The agent's way is bounded by `nudges` (D15).
    let out = told(run(Verb::Nudge { name: "gate".into(), event: "jump".into() }));
    assert!(out.contains("`jump` is no nudge of `stood`"), "{out}");
    // And the declared one moves the machine through the ordinary table.
    let out = told(run(Verb::Nudge { name: "gate".into(), event: "go".into() }));
    assert!(out.contains("gone"), "{out}");
}

#[test]
fn a_woken_sessions_turn_end_moves_the_flow() {
    // D22: the end of the woken session's turn arrives as `turn.end` and
    // moves the flow through the ordinary table.
    let dir = TempDir::new();
    let flows = read(&dir, GATE).unwrap();
    let decl = flows.find("gate").unwrap();
    drive::start(decl, &dir.0, &dir.0, None).unwrap();
    drive::settle(&dir.0, &dir.0, "gate", Event::TurnEnd, None, None);
    assert_eq!(drive::place(decl, &dir.0).unwrap().state.as_deref(), Some("gone"));
}

// ——— A command off the holder's thread (AGL-121, D35, D57) —————

/// A flow whose command sleeps: it writes its own pid and the pid of the
/// child it leaves behind, so a test can ask of each whether it lives (D57).
const SLEEP: &str = r#"{ agl, ... }: {
  flow.long = {
    initial = "build";
    state.build = {
      action.command = { cmd = "echo $$ > pid; sleep 300 & echo $! > child; wait"; timeout = 400; };
      next.exit.any = "halted";
    };
    state.halted = { };
  };
}
"#;

/// The pid a test's command wrote in `file`, once it has.
fn pid_of(dir: &TempDir, file: &str) -> Option<u32> {
    fs::read_to_string(dir.0.join(file)).ok()?.trim().parse().ok()
}

/// Whether the process lives: a signal of 0 only asks (D35).
fn alive(pid: u32) -> bool {
    // SAFETY: `pid` names a process this test started; 0 asks and harms not.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Wait until `what` holds: a command's word comes on its watcher's thread,
/// not at the call (D35).
fn until(what: impl Fn() -> bool) {
    let stop = Instant::now() + Duration::from_secs(10);
    while Instant::now() < stop && !what() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(what(), "never held within 10 s");
}

#[test]
fn a_states_command_moves_the_flow_when_its_word_comes() {
    // The holder answers `start` where the command stands, and the exit
    // line arrives with the word from the watcher thread (D35).
    let dir = TempDir::new();
    fs::write(
        dir.0.join(FLOW_FILE),
        r#"{ agl, ... }: {
  flow.quick = {
    initial = "build";
    state.build = {
      action.command = { cmd = "sleep 0.3; true"; timeout = 10; };
      next.exit.ok = "done";
      next.exit.any = "halted";
    };
    state.done = { };
    state.halted = { };
  };
}
"#,
    )
    .unwrap();
    let holder = holder::Holder::spawn(dir.0.clone(), dir.0.clone(), None);
    let ask = |verb: Verb| told(holder.ask(verb, crate::i18n::Lang::En));
    assert_eq!(ask(Verb::Start { name: "quick".into() }), "quick: build\n  started build");
    until(|| {
        ask(Verb::Status { name: "quick".into() })
            == "quick: done\n  started build\n  exit.ok -> done"
    });
}

#[test]
fn a_stop_ends_the_whole_group_and_writes_stopped_alone() {
    let dir = TempDir::new();
    fs::write(dir.0.join(FLOW_FILE), SLEEP).unwrap();
    let holder = holder::Holder::spawn(dir.0.clone(), dir.0.clone(), None);
    let ask = |verb: Verb| told(holder.ask(verb, crate::i18n::Lang::En));
    // `start` answers at once, where the command stands (D35).
    assert_eq!(ask(Verb::Start { name: "long".into() }), "long: build\n  started build");
    until(|| pid_of(&dir, "pid").is_some() && pid_of(&dir, "child").is_some());
    // `status` answers while it runs, and a second `start` runs no second
    // command (D35).
    let first = pid_of(&dir, "pid").unwrap();
    assert_eq!(ask(Verb::Status { name: "long".into() }), "long: build\n  started build");
    assert_eq!(ask(Verb::Start { name: "long".into() }), "long: build\n  started build");
    assert_eq!(pid_of(&dir, "pid").unwrap(), first, "the same command, not a second one");
    // `stop` ends the group whole: the sleeper and its child both (D57).
    assert_eq!(
        ask(Verb::Stop { name: "long".into() }),
        "long: build (stopped)\n  started build\n  stopped build"
    );
    until(|| !alive(first) && !alive(pid_of(&dir, "child").unwrap()));
    // The killed command's word finds nothing: the journal keeps `stopped`
    // alone, with no exit line (D57).
    assert_eq!(
        ask(Verb::Status { name: "long".into() }),
        "long: build (stopped)\n  started build\n  stopped build"
    );
    // A later `start` runs the state's command again (D57).
    assert_eq!(
        ask(Verb::Start { name: "long".into() }),
        "long: build\n  started build\n  stopped build\n  started build"
    );
    until(|| pid_of(&dir, "pid").unwrap() != first);
    // And out of the test's way again.
    ask(Verb::Stop { name: "long".into() });
    until(|| !alive(pid_of(&dir, "pid").unwrap()));
}
