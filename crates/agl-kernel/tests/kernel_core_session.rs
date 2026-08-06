#[path = "core/support/session.rs"]
mod session_support;

use session_support::{ProductionSessionMachine, SessionRecordView, SessionTransition};

fn apply_path(
    machine: &mut ProductionSessionMachine,
    transitions: &[SessionTransition],
) -> Vec<SessionRecordView> {
    transitions
        .iter()
        .map(|transition| {
            machine
                .apply(*transition)
                .unwrap_or_else(|error| panic!("{transition:?}: {error}"))
        })
        .collect()
}

fn target_states(records: &[SessionRecordView]) -> Vec<&str> {
    records.iter().map(|record| record.to.as_str()).collect()
}

// KCT-SESSION-001. Mutation: remove or redirect one accepted answer-path edge.
#[test]
fn session_answer_path_records_every_accepted_transition() {
    let mut machine = ProductionSessionMachine::new("session-core-1");
    let transitions = [
        SessionTransition::StartNew,
        SessionTransition::PromptForInput,
        SessionTransition::ReadUserMessage,
        SessionTransition::RecordUserMessage,
        SessionTransition::LinkModelAttempt,
        SessionTransition::RecordAssistantToolCall,
        SessionTransition::RecordToolMessage,
        SessionTransition::RecordAssistantAnswer,
        SessionTransition::PromptForInput,
    ];
    let expected_states = [
        "started",
        "awaiting_input",
        "recording_user_message",
        "running_turn",
        "running_turn",
        "running_turn",
        "running_turn",
        "recording_assistant_message",
        "awaiting_input",
    ];

    let records = apply_path(&mut machine, &transitions);
    assert_eq!(target_states(&records), expected_states);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.session_id, "session-core-1");
        assert_eq!(record.sequence, index as u64 + 1);
        assert_eq!(record.transition, transitions[index]);
        assert_eq!(
            record.from,
            if index == 0 {
                "uninitialized"
            } else {
                records[index - 1].to.as_str()
            }
        );
    }
    assert_eq!(machine.sequence(), transitions.len() as u64);
    assert_eq!(machine.state(), "awaiting_input");
}

// KCT-SESSION-001. Mutation: omit clear, continuation, stop or terminal edges.
#[test]
fn session_command_continuation_stop_and_terminal_paths_are_explicit() {
    let mut clear = ProductionSessionMachine::new("session-clear");
    assert_eq!(
        target_states(&apply_path(
            &mut clear,
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
                SessionTransition::ReadCommandClear,
                SessionTransition::ClearContext,
                SessionTransition::PromptForInput,
                SessionTransition::ReadCommandExit,
            ],
        )),
        [
            "started",
            "awaiting_input",
            "handling_command",
            "context_cleared",
            "awaiting_input",
            "finished",
        ]
    );

    let mut continuation = ProductionSessionMachine::new("session-continuation");
    assert_eq!(
        target_states(&apply_path(
            &mut continuation,
            &[
                SessionTransition::Resume,
                SessionTransition::PromptForInput,
                SessionTransition::BeginIncompleteContinuation,
                SessionTransition::RecordAssistantStop,
                SessionTransition::PromptForInput,
                SessionTransition::Finish,
            ],
        )),
        [
            "started",
            "awaiting_input",
            "running_turn",
            "recording_assistant_message",
            "awaiting_input",
            "finished",
        ]
    );

    let mut failed = ProductionSessionMachine::new("session-failed");
    let records = apply_path(
        &mut failed,
        &[SessionTransition::StartNew, SessionTransition::Fail],
    );
    assert_eq!(target_states(&records), ["started", "failed"]);
    assert_eq!(failed.state(), "failed");
}

// KCT-SESSION-001. Mutation: add, remove or redirect any edge in the selected table.
#[test]
fn session_transition_table_covers_every_current_legal_edge() {
    let cases: &[(&str, &[SessionTransition], SessionTransition, &str)] = &[
        ("start-new", &[], SessionTransition::StartNew, "started"),
        ("resume", &[], SessionTransition::Resume, "started"),
        (
            "started-prompt",
            &[SessionTransition::StartNew],
            SessionTransition::PromptForInput,
            "awaiting_input",
        ),
        (
            "started-finish",
            &[SessionTransition::StartNew],
            SessionTransition::Finish,
            "finished",
        ),
        (
            "read-user",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
            ],
            SessionTransition::ReadUserMessage,
            "recording_user_message",
        ),
        (
            "read-clear",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
            ],
            SessionTransition::ReadCommandClear,
            "handling_command",
        ),
        (
            "read-exit",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
            ],
            SessionTransition::ReadCommandExit,
            "finished",
        ),
        (
            "continue",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
            ],
            SessionTransition::BeginIncompleteContinuation,
            "running_turn",
        ),
        (
            "awaiting-finish",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
            ],
            SessionTransition::Finish,
            "finished",
        ),
        (
            "record-user",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
                SessionTransition::ReadUserMessage,
            ],
            SessionTransition::RecordUserMessage,
            "running_turn",
        ),
        (
            "clear-context",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
                SessionTransition::ReadCommandClear,
            ],
            SessionTransition::ClearContext,
            "context_cleared",
        ),
        (
            "prompt-after-clear",
            &[
                SessionTransition::StartNew,
                SessionTransition::PromptForInput,
                SessionTransition::ReadCommandClear,
                SessionTransition::ClearContext,
            ],
            SessionTransition::PromptForInput,
            "awaiting_input",
        ),
    ];
    for (label, prefix, transition, expected) in cases {
        let mut machine = ProductionSessionMachine::new(label);
        apply_path(&mut machine, prefix);
        let record = machine
            .apply(*transition)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(record.to, *expected, "{label}");
    }

    let running_prefix = [
        SessionTransition::StartNew,
        SessionTransition::PromptForInput,
        SessionTransition::ReadUserMessage,
        SessionTransition::RecordUserMessage,
    ];
    for (transition, expected) in [
        (SessionTransition::LinkModelAttempt, "running_turn"),
        (SessionTransition::RecordAssistantToolCall, "running_turn"),
        (SessionTransition::RecordToolMessage, "running_turn"),
        (
            SessionTransition::RecordAssistantAnswer,
            "recording_assistant_message",
        ),
        (
            SessionTransition::RecordAssistantStop,
            "recording_assistant_message",
        ),
        (SessionTransition::PromptForInput, "awaiting_input"),
    ] {
        let mut machine = ProductionSessionMachine::new("running-edge");
        apply_path(&mut machine, &running_prefix);
        assert_eq!(machine.apply(transition).unwrap().to, expected);
    }

    for prefix in [
        vec![],
        vec![SessionTransition::StartNew],
        vec![
            SessionTransition::StartNew,
            SessionTransition::PromptForInput,
        ],
        vec![
            SessionTransition::StartNew,
            SessionTransition::PromptForInput,
            SessionTransition::ReadCommandClear,
        ],
        vec![
            SessionTransition::StartNew,
            SessionTransition::PromptForInput,
            SessionTransition::ReadUserMessage,
        ],
        running_prefix.to_vec(),
        vec![
            SessionTransition::StartNew,
            SessionTransition::PromptForInput,
            SessionTransition::ReadUserMessage,
            SessionTransition::RecordUserMessage,
            SessionTransition::RecordAssistantAnswer,
        ],
        vec![
            SessionTransition::StartNew,
            SessionTransition::PromptForInput,
            SessionTransition::ReadCommandClear,
            SessionTransition::ClearContext,
        ],
    ] {
        let mut machine = ProductionSessionMachine::new("fail-edge");
        apply_path(&mut machine, &prefix);
        assert_eq!(machine.apply(SessionTransition::Fail).unwrap().to, "failed");
    }
}

// KCT-SESSION-002. Mutation: mutate state or sequence while rejecting an edge.
#[test]
fn illegal_and_post_terminal_session_transitions_do_not_mutate_state() {
    let mut uninitialized = ProductionSessionMachine::new("session-invalid");
    let before = (uninitialized.state(), uninitialized.sequence());
    assert!(
        uninitialized
            .apply(SessionTransition::RecordAssistantAnswer)
            .is_err()
    );
    assert_eq!((uninitialized.state(), uninitialized.sequence()), before);

    let mut finished = ProductionSessionMachine::new("session-finished");
    apply_path(
        &mut finished,
        &[SessionTransition::StartNew, SessionTransition::Finish],
    );
    let before = (finished.state(), finished.sequence());
    for transition in [
        SessionTransition::PromptForInput,
        SessionTransition::Fail,
        SessionTransition::Finish,
    ] {
        assert!(
            finished.apply(transition).is_err(),
            "accepted {transition:?}"
        );
        assert_eq!((finished.state(), finished.sequence()), before);
    }

    let mut failed = ProductionSessionMachine::new("session-terminal-failed");
    apply_path(
        &mut failed,
        &[SessionTransition::StartNew, SessionTransition::Fail],
    );
    let before = (failed.state(), failed.sequence());
    for transition in [
        SessionTransition::PromptForInput,
        SessionTransition::Fail,
        SessionTransition::Finish,
    ] {
        assert!(failed.apply(transition).is_err(), "accepted {transition:?}");
        assert_eq!((failed.state(), failed.sequence()), before);
    }
}

// KCT-SESSION-003. Mutation: require agl-session storage to restore the pure machine.
#[test]
fn session_machine_round_trips_without_store_or_filesystem_state() {
    let mut original = ProductionSessionMachine::new("session-round-trip");
    apply_path(
        &mut original,
        &[
            SessionTransition::StartNew,
            SessionTransition::PromptForInput,
            SessionTransition::ReadUserMessage,
            SessionTransition::RecordUserMessage,
        ],
    );
    let bytes = original.checkpoint_bytes();
    let restored = ProductionSessionMachine::restore(&bytes).expect("Session restores");
    assert_eq!(restored.state(), original.state());
    assert_eq!(restored.sequence(), original.sequence());
    assert_eq!(restored.checkpoint_bytes(), bytes);
}
