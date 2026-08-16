use super::*;

#[test]
fn command_finished_does_not_synthesize_a_trusted_prompt() {
    let session_id = SessionId::generate();
    let mut terminal = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    terminal.prompt_state = TerminalPromptState::ForegroundProcess;
    terminal.prompt_generation = None;

    let finished = agl_protocol::SessionPresentationEventPayload::TerminalCommandFinished {
        terminal_id: terminal.terminal_id.clone(),
        sequence: 1,
        exit_status: 0,
        cwd: test_display_path("/workspace"),
    };
    assert_eq!(
        terminal_prompt_from_event(&finished, &terminal.terminal_id),
        None
    );

    terminal.prompt_state = TerminalPromptState::Ready;
    terminal.prompt_generation = Some(2);
    let changed = agl_protocol::SessionPresentationEventPayload::TerminalChanged {
        terminal: terminal.clone(),
    };
    assert_eq!(
        terminal_prompt_from_event(&changed, &terminal.terminal_id),
        Some(TerminalPromptState::Ready)
    );
}
