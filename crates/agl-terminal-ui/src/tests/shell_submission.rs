use super::*;

#[test]
fn physical_bang_enters_shell_and_second_bang_escapes_to_prompt() {
    let mut composer = Composer::default();
    composer.insert_char('!');
    assert_eq!(composer.mode, ComposerMode::Shell);
    assert!(composer.buffer.is_empty());
    assert_eq!(composer.submit(), Some(ComposerSubmission::SwitchTerminal));

    composer.insert_char('!');
    composer.insert_text("ls");
    assert_eq!(
        composer.submit(),
        Some(ComposerSubmission::Shell("ls".to_owned()))
    );
    assert_eq!(composer.mode, ComposerMode::Shell);
    assert_eq!(composer.buffer, "ls");
    composer.reset();

    composer.insert_char('!');
    composer.insert_char('!');
    assert_eq!(composer.mode, ComposerMode::Prompt);
    assert_eq!(composer.buffer, "!");
    assert_eq!(
        composer.submit(),
        Some(ComposerSubmission::Prompt("!".to_owned()))
    );

    composer.insert_char('!');
    composer.backspace();
    assert_eq!(composer.mode, ComposerMode::Prompt);
    assert!(composer.buffer.is_empty());

    composer.insert_char('!');
    composer.insert_char('e');
    composer.insert_char('\n');
    assert_eq!(
        composer.submit(),
        Some(ComposerSubmission::Shell("e\n".to_owned()))
    );
    composer.reset();

    composer.insert_char('/');
    assert_eq!(composer.mode, ComposerMode::Command);
    composer.insert_char('/');
    assert_eq!(composer.mode, ComposerMode::Prompt);
    assert_eq!(composer.buffer, "/");
}

#[test]
fn empty_shell_editor_key_reducer_escapes_or_attaches_exactly() {
    let mut state = test_ui_state(SessionId::generate(), Vec::new());

    assert!(
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)
        )
        .is_none()
    );
    assert_eq!(state.composer.mode, ComposerMode::Shell);
    assert!(handle_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).is_none());
    assert_eq!(state.composer.mode, ComposerMode::Prompt);
    assert!(state.composer.buffer.is_empty());

    handle_key(
        &mut state,
        KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
    );
    assert!(
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
        )
        .is_none()
    );
    assert_eq!(state.composer.mode, ComposerMode::Prompt);
    assert!(state.composer.buffer.is_empty());

    handle_key(
        &mut state,
        KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
    );
    assert!(matches!(
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        ),
        Some(UiControl::Submission(ComposerSubmission::SwitchTerminal))
    ));
    assert_eq!(state.composer.mode, ComposerMode::Prompt);
    assert!(state.composer.buffer.is_empty());
}

#[test]
fn pasted_leading_sigils_remain_prompt_text() {
    let mut composer = Composer::default();
    composer.insert_paste("!printf pasted\n/also-literal");
    assert_eq!(composer.mode, ComposerMode::Prompt);
    assert_eq!(composer.buffer, "!printf pasted\n/also-literal");
    composer.reset();
    composer.insert_paste("!");
    assert_eq!(
        composer.submit(),
        Some(ComposerSubmission::Prompt("!".to_owned()))
    );

    composer.insert_char('!');
    composer.insert_paste("!printf shell-paste");
    assert_eq!(composer.mode, ComposerMode::Shell);
    assert_eq!(composer.buffer, "!printf shell-paste");
    assert_eq!(
        composer.submit(),
        Some(ComposerSubmission::Shell("!printf shell-paste".to_owned()))
    );
    assert_eq!(composer.buffer, "!printf shell-paste");
}

#[test]
fn sanitized_display_paths_mark_truncation_without_becoming_authority_paths() {
    let complete = test_display_path("/workspace/repository");
    assert_eq!(display_path(&complete), "/workspace/repository");
    assert_eq!(workspace_label(&complete), "repository");

    let truncated = agl_protocol::SanitizedDisplayPath {
        text: "/workspace/partial".to_owned(),
        truncated: true,
    };
    assert_eq!(display_path(&truncated), "/workspace/partial…");
    assert_eq!(workspace_label(&truncated), "partial…");
}

#[test]
fn shell_submission_keeps_exact_buffer_and_identity_until_explicit_acceptance() {
    let session_id = SessionId::generate();
    let terminal = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    let mut state = test_ui_state(session_id.clone(), vec![terminal.clone()]);
    let command = "printf 'λ'\nprintf done".to_owned();
    state.composer.mode = ComposerMode::Shell;
    state.composer.buffer = command.clone();
    state.composer.cursor = command.len();

    let first = begin_shell_submission(&session_id, &mut state, command.clone(), &None)
        .unwrap()
        .unwrap();
    let submission_id = first.client_submission_id.clone();
    let ensure_id = first.terminal_ensure_submission_id.clone();
    assert_eq!(first.command, command);
    assert_eq!(state.composer.buffer, command);
    assert_eq!(state.composer.mode, ComposerMode::Shell);

    let busy = shell_submission_failure(&first, Some(terminal.clone()), None, "busy", false);
    apply_shell_submission_completion(&mut state, &session_id, None, busy);
    let pending = state.pending_shell_submission.as_ref().unwrap();
    assert_eq!(pending.command, command);
    assert_eq!(pending.client_submission_id, submission_id);
    assert!(!pending.in_flight);
    assert!(!pending.outcome_uncertain);
    assert_eq!(state.composer.buffer, command);

    let second = begin_shell_submission(&session_id, &mut state, command.clone(), &None)
        .unwrap()
        .unwrap();
    assert_eq!(second.client_submission_id, submission_id);
    assert_eq!(second.terminal_ensure_submission_id, ensure_id);
    let uncertain = shell_submission_failure(
        &second,
        Some(terminal.clone()),
        None,
        "connection closed",
        true,
    );
    apply_shell_submission_completion(&mut state, &session_id, None, uncertain);
    handle_key(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    assert_eq!(state.composer.buffer, command);
    assert_eq!(
        state
            .pending_shell_submission
            .as_ref()
            .unwrap()
            .client_submission_id,
        submission_id
    );

    let retry = handle_key(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    let Some(UiControl::Submission(ComposerSubmission::Shell(retry_command))) = retry else {
        panic!("uncertain Shell command did not remain retryable");
    };
    let third = begin_shell_submission(&session_id, &mut state, retry_command, &None)
        .unwrap()
        .unwrap();
    assert_eq!(third.client_submission_id, submission_id);
    let accepted = ShellSubmissionCompletion {
        session_id: session_id.clone(),
        command: command.clone(),
        client_submission_id: submission_id,
        terminal: Some(terminal.clone()),
        attachment: None,
        outcome: Ok(ShellCommandAccepted {
            terminal_id: terminal.terminal_id.clone(),
            command_sequence: 1,
        }),
    };
    apply_shell_submission_completion(&mut state, &session_id, None, accepted);
    assert!(state.pending_shell_submission.is_none());
    assert_eq!(state.composer.mode, ComposerMode::Prompt);
    assert!(state.composer.buffer.is_empty());
    assert_eq!(state.human_commands.len(), 1);
    assert_eq!(state.human_commands[0].command, command);
    assert_eq!(
        state.human_commands[0].state,
        LocalHumanCommandState::Running
    );
    assert!(
        !serde_json::to_string(&state.snapshot)
            .unwrap()
            .contains("printf done")
    );

    let mut ready = terminal;
    ready.command_sequence = 1;
    ready.prompt_state = TerminalPromptState::Ready;
    update_local_human_commands(&mut state, &ready);
    assert_eq!(
        state.human_commands[0].state,
        LocalHumanCommandState::Completed
    );
}
#[test]
fn managed_shell_profile_is_exact_and_has_no_sh_fallback() {
    assert_eq!(
        managed_shell_profile_id(Path::new("/nix/store/hash/bin/bash")),
        Some("bash-managed")
    );
    assert_eq!(
        managed_shell_profile_id(Path::new("/usr/bin/zsh")),
        Some("zsh-managed")
    );
    assert_eq!(managed_shell_profile_id(Path::new("/bin/sh")), None);
}

#[test]
fn terminal_environment_uses_a_bounded_physical_terminal_name() {
    let overlay = terminal_environment_for(Some("tmux-256color"));
    assert_eq!(overlay.values["TERM"], "tmux-256color");
    assert!(overlay.inherited_names.is_empty());
    assert!(overlay.secret_refs.is_empty());

    for invalid in ["", "../../host", "xterm\nINJECTED=value"] {
        assert_eq!(
            terminal_environment_for(Some(invalid)).values["TERM"],
            "xterm-256color"
        );
    }
}
