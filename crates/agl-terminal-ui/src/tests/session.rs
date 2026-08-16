use super::*;

#[test]
fn assembled_snapshot_replacement_is_installed_as_one_projection() {
    let session_id = SessionId::generate();
    let terminal = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    let terminal_id = terminal.terminal_id.clone();
    let mut state = test_ui_state(session_id, vec![terminal]);
    let run_id = RunId::generate();
    let message_id = MessageId::generate();
    assert_eq!(
        append_assistant_delta(
            &mut state.assistant_deltas,
            run_id,
            message_id,
            1,
            "partial",
        ),
        AssistantDeltaApply::Applied
    );
    let mut replacement = state.snapshot.clone();
    replacement.cursor.revision += 1;
    replacement.header.cwd = test_display_path("/workspace/replaced");
    replacement.terminals.clear();

    install_presentation_snapshot(&mut state, replacement.clone());

    assert_eq!(state.snapshot, replacement);
    assert!(state.assistant_deltas.is_empty());
    assert_eq!(
        terminal_prompt_from_snapshot(&state.snapshot, &terminal_id),
        TerminalPromptState::Unavailable
    );
}

#[test]
fn authoritative_snapshots_mark_existing_terminals_for_tail_reattach() {
    let session_id = SessionId::generate();
    let terminal = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    let terminal_id = terminal.terminal_id.clone();
    let mut state = test_ui_state(session_id, Vec::new());
    let mut snapshot = state.snapshot.clone();
    snapshot.terminals.push(terminal);

    install_presentation_snapshot(&mut state, snapshot.clone());
    assert!(state.seen_terminals.contains(&terminal_id));

    state.seen_terminals.clear();
    install_session_switch(
        &mut state,
        snapshot,
        Vec::new(),
        InputHistory {
            root: None,
            prompt: Vec::new(),
        },
        Vec::new(),
    );
    assert!(state.seen_terminals.contains(&terminal_id));
}

#[test]
fn prompt_lifecycle_events_keep_peer_ui_state_and_counts_coherent() {
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let mut state = test_ui_state(session_id, Vec::new());

    apply_presentation_event(
        &mut state,
        agl_protocol::SessionPresentationEventPayload::PromptQueued {
            prompt: agl_protocol::QueuedPromptView {
                run_id: run_id.clone(),
                ordinal: 1,
            },
        },
    );
    assert_eq!(state.snapshot.header.queued_prompt_count, 1);
    assert_eq!(state.snapshot.command_context.active_or_queued_turns, 1);

    apply_presentation_event(
        &mut state,
        agl_protocol::SessionPresentationEventPayload::PromptActivated {
            run_id: run_id.clone(),
        },
    );
    assert_eq!(state.active_run.as_ref(), Some(&run_id));
    assert_eq!(state.snapshot.header.active_run_count, 1);
    assert_eq!(state.snapshot.header.queued_prompt_count, 0);

    apply_presentation_event(
        &mut state,
        agl_protocol::SessionPresentationEventPayload::PromptFinished {
            run_id,
            state: "answered".to_owned(),
        },
    );
    assert!(state.active_run.is_none());
    assert_eq!(state.snapshot.header.active_run_count, 0);
    assert_eq!(state.snapshot.command_context.active_or_queued_turns, 0);
}
