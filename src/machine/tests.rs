use super::*;
use crate::function::tests::{CODER, function, with_states};
use crate::session::Unfinished;

fn name(machine: &Machine) -> String {
    machine.state().name.clone()
}

#[test]
fn spec_example_explore_limits() {
    let coder = function(CODER).unwrap();
    let mut machine = Machine::new(&coder);
    assert_eq!(name(&machine), "explore");
    // Staying in explore is one visit (D82); the 15th step uses it up.
    for _ in 0..14 {
        assert_eq!(machine.after_call("bash", Outcome::Ok), Progress::Continue);
        assert_eq!(name(&machine), "explore");
    }
    machine.after_call("bash", Outcome::Fail);
    assert_eq!(name(&machine), "answer");
}

#[test]
fn spec_example_visits() {
    let coder = function(CODER).unwrap();
    let mut machine = Machine::new(&coder);
    for _ in 0..2 {
        machine.after_call("edit", Outcome::Ok);
        assert_eq!(name(&machine), "verify");
        machine.after_call("bash", Outcome::Fail);
        assert_eq!(name(&machine), "explore");
    }
    // The third visit of verify is still allowed; a fourth of explore is not.
    machine.after_call("edit", Outcome::Ok);
    assert_eq!(name(&machine), "verify");
    machine.after_call("bash", Outcome::Fail);
    assert_eq!(name(&machine), "answer");
}

#[test]
fn spec_example_success() {
    let coder = function(CODER).unwrap();
    let mut machine = Machine::new(&coder);
    machine.after_call("bash", Outcome::Ok);
    machine.after_call("edit", Outcome::Fail);
    assert_eq!(name(&machine), "verify");
    machine.after_call("bash", Outcome::Timeout);
    assert_eq!(name(&machine), "answer");
    // A new message starts over (D44, D84).
    assert_eq!(name(&Machine::new(&coder)), "explore");
}

#[test]
fn step_limit() {
    let function = function(
        "format = \"qwen3.8\"\nsystem = \"s\"\nmax_steps_total = 3\ninitial = \"a\"\n\
         [state.a]\ntools.bash = {}\nanswer = true\nnext = { else = \"a\" }",
    )
    .unwrap();
    let mut machine = Machine::new(&function);
    assert_eq!(machine.after_call("bash", Outcome::Ok), Progress::Continue);
    assert_eq!(machine.after_call("bash", Outcome::Ok), Progress::Continue);
    assert_eq!(machine.after_call("bash", Outcome::Ok), Progress::StepLimit);
}

#[test]
fn on_limit_chain() {
    // b's visits run out, so entering it leads to its on_limit c.
    let function = with_states(
        "[state.a]\ntools.bash = {}\nnext = { else = \"b\" }\n\
         [state.b]\ntools.bash = {}\nmax_visits = 1\non_limit = \"c\"\nnext = { else = \"a\" }\n\
         [state.c]\nanswer = true",
    )
    .unwrap();
    let mut machine = Machine::new(&function);
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "b");
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "a");
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "c");
}

#[test]
fn leaving_resets_the_visit_steps() {
    let function = with_states(
        "[state.a]\ntools.bash = {}\ntools.edit = {}\nmax_steps = 2\non_limit = \"c\"\n\
         next = { edit = \"b\", else = \"a\" }\n\
         [state.b]\ntools.bash = {}\nnext = { else = \"a\" }\n\
         [state.c]\nanswer = true",
    )
    .unwrap();
    let mut machine = Machine::new(&function);
    machine.after_call("bash", Outcome::Ok);
    machine.after_call("edit", Outcome::Ok);
    assert_eq!(name(&machine), "b");
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "a");
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "a");
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "c");
}

#[test]
fn resuming_an_unfinished_turn() {
    let coder = function(CODER).unwrap();
    let turn = Unfinished {
        state: Some("explore".into()),
        visits: vec![("explore".into(), 3), ("verify".into(), 2)],
        steps_in_visit: 14,
    };
    let mut machine = Machine::resume(&coder, "coder", &turn).unwrap();
    assert_eq!(name(&machine), "explore");
    // One more step uses up the visit's 15 steps (D105).
    machine.after_call("bash", Outcome::Ok);
    assert_eq!(name(&machine), "answer");

    // Visits carry over: verify has one visit left, explore none.
    let turn = Unfinished {
        state: Some("explore".into()),
        visits: vec![("explore".into(), 3), ("verify".into(), 2)],
        steps_in_visit: 0,
    };
    let mut machine = Machine::resume(&coder, "coder", &turn).unwrap();
    machine.after_call("edit", Outcome::Ok);
    assert_eq!(name(&machine), "verify");
    machine.after_call("bash", Outcome::Fail);
    assert_eq!(name(&machine), "answer");
}

#[test]
fn resuming_counts_total_steps_anew() {
    let function = function(
        "format = \"qwen3.8\"\nsystem = \"s\"\nmax_steps_total = 2\ninitial = \"a\"\n\
         [state.a]\ntools.bash = {}\nanswer = true\nnext = { else = \"a\" }",
    )
    .unwrap();
    let turn = Unfinished {
        state: Some("a".into()),
        visits: vec![("a".into(), 1)],
        steps_in_visit: 2,
    };
    let mut machine = Machine::resume(&function, "f", &turn).unwrap();
    assert_eq!(machine.after_call("bash", Outcome::Ok), Progress::Continue);
    assert_eq!(machine.after_call("bash", Outcome::Ok), Progress::StepLimit);
}

#[test]
fn resuming_without_a_state_starts_in_initial() {
    let coder = function(CODER).unwrap();
    let turn = Unfinished {
        state: None,
        visits: Vec::new(),
        steps_in_visit: 0,
    };
    assert_eq!(
        name(&Machine::resume(&coder, "coder", &turn).unwrap()),
        "explore"
    );
    let gone = Unfinished {
        state: Some("review".into()),
        visits: vec![("review".into(), 1)],
        steps_in_visit: 0,
    };
    assert!(matches!(
        Machine::resume(&coder, "coder", &gone),
        Err(Error::SessionState { state, .. }) if state == "review"
    ));
}
