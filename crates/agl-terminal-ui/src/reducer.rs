use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{ComposerMode, ComposerSubmission, InteractiveState, shell_submission_allows_edit};

/// Presentation-level inputs accepted by the deterministic Chat reducer.
///
/// This type intentionally contains no client, socket, descriptor, process or
/// attachment handle. Runtime owners translate their completions into these
/// bounded values before re-entering [`update`].
pub(super) enum UiEvent {
    Key(KeyEvent),
    Paste(String),
    RunAccepted {
        run_id: agl_ids::RunId,
        state: agl_protocol::ProtocolRunState,
    },
    Snapshot(Box<agl_protocol::SessionPresentationSnapshot>),
    ShellAccepted {
        command_sequence: u64,
    },
    ShellRejected {
        message: String,
        client_submission_id: String,
        outcome_uncertain: bool,
    },
    Notice(String),
}

#[derive(Debug)]
pub(super) enum UiEffect {
    Disconnect,
    CancelRun(agl_ids::RunId),
    ContinueIncomplete(agl_ids::MessageId),
    SubmitPrompt(String),
    SubmitHumanTerminalCommand(String),
    AttachHumanTerminal,
    InvokeCommand(String),
    SubmitPicker(super::PickerSubmit),
    Notice(String),
}

pub(super) fn update(state: &mut InteractiveState, event: UiEvent) -> Vec<UiEffect> {
    match event {
        UiEvent::Key(key) => update_key(state, key).into_iter().collect(),
        UiEvent::Paste(text) => {
            if shell_submission_allows_edit(state) {
                state.composer.insert_paste(&text);
            }
            Vec::new()
        }
        UiEvent::RunAccepted {
            run_id,
            state: agl_protocol::ProtocolRunState::Running,
        } => {
            state.active_run = Some(run_id);
            Vec::new()
        }
        UiEvent::RunAccepted { .. } => Vec::new(),
        UiEvent::Snapshot(snapshot) => {
            state.active_run = snapshot
                .active_run
                .as_ref()
                .map(|active| active.run_id.clone());
            state.snapshot = *snapshot;
            state.assistant_deltas.clear();
            Vec::new()
        }
        UiEvent::ShellAccepted { command_sequence } => {
            state.pending_shell_submission = None;
            state.composer.reset();
            state.notice(format!("Shell command {command_sequence} accepted"));
            Vec::new()
        }
        UiEvent::ShellRejected {
            message,
            client_submission_id,
            outcome_uncertain,
        } => {
            if let Some(pending) = state.pending_shell_submission.as_mut() {
                pending.in_flight = false;
                pending.outcome_uncertain = outcome_uncertain;
            }
            state.notice(if outcome_uncertain {
                format!(
                    "{message}; outcome is uncertain. Enter retries the exact command with request identity {client_submission_id}"
                )
            } else {
                format!(
                    "{message}; no automatic retry. The Shell buffer and request identity {client_submission_id} were retained"
                )
            });
            Vec::new()
        }
        UiEvent::Notice(message) => {
            state.notice(message);
            Vec::new()
        }
    }
}

fn update_key(state: &mut InteractiveState, key: KeyEvent) -> Option<UiEffect> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if control {
        match key.code {
            KeyCode::Char('d') => return Some(UiEffect::Disconnect),
            KeyCode::Char('g') => {
                state.activity_expanded = !state.activity_expanded;
                return None;
            }
            KeyCode::Char('y') => {
                return state
                    .latest_available_incomplete()
                    .map(UiEffect::ContinueIncomplete)
                    .or_else(|| {
                        Some(UiEffect::Notice(
                            "no incomplete assistant output is available to continue".to_owned(),
                        ))
                    });
            }
            KeyCode::Char('c') => {
                if state.composer.buffer.is_empty() {
                    if let Some(run_id) = state.active_run.take() {
                        return Some(UiEffect::CancelRun(run_id));
                    }
                    state.notice("Ctrl+D disconnects this UI; /exit finishes the session");
                } else if shell_submission_allows_edit(state) {
                    state.composer.reset();
                }
                return None;
            }
            KeyCode::Char('a' | 'A') if !shift => {
                if shell_submission_allows_edit(state) {
                    state.composer.select_all();
                }
                return None;
            }
            KeyCode::Char('z' | 'Z') => {
                if shell_submission_allows_edit(state) {
                    if shift {
                        state.composer.redo();
                    } else {
                        state.composer.undo();
                    }
                }
                return None;
            }
            KeyCode::Left => {
                state.composer.move_word_left(shift);
                return None;
            }
            KeyCode::Right => {
                state.composer.move_word_right(shift);
                return None;
            }
            KeyCode::Backspace => {
                if shell_submission_allows_edit(state) {
                    state.composer.delete_word_left();
                }
                return None;
            }
            KeyCode::Delete => {
                if shell_submission_allows_edit(state) {
                    state.composer.delete_word_right();
                }
                return None;
            }
            _ => {}
        }
    }

    if key_edits_composer(key) && !shell_submission_allows_edit(state) {
        return None;
    }
    match key.code {
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            state.composer.insert_char(character)
        }
        KeyCode::Left => state.composer.move_left(shift),
        KeyCode::Right => state.composer.move_right(shift),
        KeyCode::Home => state.composer.move_home(shift),
        KeyCode::End => state.composer.move_end(shift),
        KeyCode::Backspace => state.composer.backspace(),
        KeyCode::Delete => state.composer.delete(),
        KeyCode::Esc => state.composer.reset(),
        KeyCode::Up if state.composer.mode == ComposerMode::Command => {
            state.composer.selected_command = state.composer.selected_command.saturating_sub(1)
        }
        KeyCode::Down if state.composer.mode == ComposerMode::Command => {
            state.composer.selected_command = state.composer.selected_command.saturating_add(1)
        }
        KeyCode::Up => {
            if !state.composer.move_up(shift) && !shift {
                let entries = state.history.entries(state.composer.mode).to_vec();
                state.composer.history_previous(&entries);
            }
        }
        KeyCode::Down => {
            if !state.composer.move_down(shift) && !shift {
                let entries = state.history.entries(state.composer.mode).to_vec();
                state.composer.history_next(&entries);
            }
        }
        KeyCode::Enter if shift => state.composer.insert_char('\n'),
        KeyCode::Enter => {
            if state.composer.mode == ComposerMode::Command
                && !state.matching_commands().is_empty()
                && !state.composer.buffer.contains(char::is_whitespace)
            {
                let selected = state
                    .composer
                    .selected_command
                    .min(state.matching_commands().len() - 1);
                let command = state.matching_commands()[selected].name.clone();
                state.composer.replace_buffer(command);
            }
            return state.composer.submit().map(|submission| match submission {
                ComposerSubmission::Prompt(prompt) => UiEffect::SubmitPrompt(prompt),
                ComposerSubmission::Shell(command) => UiEffect::SubmitHumanTerminalCommand(command),
                ComposerSubmission::SwitchTerminal => UiEffect::AttachHumanTerminal,
                ComposerSubmission::Command(command) => UiEffect::InvokeCommand(command),
                ComposerSubmission::Picker(picker) => UiEffect::SubmitPicker(picker),
            });
        }
        _ => {}
    }
    None
}

fn key_edits_composer(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete | KeyCode::Esc
    ) || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_returns_effect_without_runtime_io() {
        let session_id = agl_ids::SessionId::generate();
        let mut state = super::super::tests::test_ui_state(session_id, Vec::new());
        assert!(
            update(
                &mut state,
                UiEvent::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            )
            .is_empty()
        );
        update(&mut state, UiEvent::Paste("printf reducer".to_owned()));
        let effects = update(
            &mut state,
            UiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(matches!(
            effects.as_slice(),
            [UiEffect::SubmitHumanTerminalCommand(command)]
                if command == "printf reducer"
        ));
    }

    #[test]
    fn conventional_editor_bindings_select_undo_and_redo() {
        let session_id = agl_ids::SessionId::generate();
        let mut state = super::super::tests::test_ui_state(session_id, Vec::new());
        update(&mut state, UiEvent::Paste("one two".to_owned()));
        update(
            &mut state,
            UiEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
        );
        update(
            &mut state,
            UiEvent::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
        );
        assert!(state.composer.selection().is_some());
        update(
            &mut state,
            UiEvent::Key(KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::NONE)),
        );
        assert_eq!(state.composer.buffer, "one λ");
        update(
            &mut state,
            UiEvent::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
        );
        assert_eq!(state.composer.buffer, "one two");
        update(
            &mut state,
            UiEvent::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
        );
        assert_eq!(state.composer.buffer, "one λ");
    }
}
