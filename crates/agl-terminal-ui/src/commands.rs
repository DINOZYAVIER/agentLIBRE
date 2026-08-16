use super::*;

pub(super) fn shell_submission_allows_edit(state: &mut UiState) -> bool {
    let Some(pending) = state.pending_shell_submission.as_ref() else {
        return true;
    };
    if pending.in_flight || pending.outcome_uncertain {
        state.notice(if pending.outcome_uncertain {
            "Shell command outcome is uncertain; retry with Enter keeps the same request identity"
        } else {
            "Shell command admission is pending; the exact command remains read-only"
        });
        return false;
    }
    state.pending_shell_submission = None;
    true
}

pub(super) fn handle_key(state: &mut UiState, key: KeyEvent) -> Option<UiControl> {
    if state.picker.is_some() {
        return handle_picker_key(state, key);
    }
    update(state, UiEvent::Key(key))
        .into_iter()
        .next()
        .map(|effect| match effect {
            UiEffect::Disconnect => UiControl::Disconnect,
            UiEffect::CancelRun(run_id) => UiControl::CancelRun(run_id),
            UiEffect::ContinueIncomplete(message_id) => UiControl::ContinueIncomplete(message_id),
            UiEffect::SubmitPrompt(prompt) => {
                UiControl::Submission(ComposerSubmission::Prompt(prompt))
            }
            UiEffect::SubmitHumanTerminalCommand(command) => {
                UiControl::Submission(ComposerSubmission::Shell(command))
            }
            UiEffect::AttachHumanTerminal => {
                UiControl::Submission(ComposerSubmission::SwitchTerminal)
            }
            UiEffect::InvokeCommand(command) => {
                UiControl::Submission(ComposerSubmission::Command(command))
            }
            UiEffect::SubmitPicker(picker) => {
                UiControl::Submission(ComposerSubmission::Picker(picker))
            }
            UiEffect::Notice(message) => UiControl::Notice(message),
        })
}

pub(super) fn handle_picker_key(state: &mut UiState, key: KeyEvent) -> Option<UiControl> {
    let confirmation = state
        .picker
        .as_ref()
        .and_then(|picker| picker.confirmation.clone());
    if let Some(confirmation) = confirmation {
        return match key.code {
            KeyCode::Enter => {
                state.picker = None;
                Some(UiControl::Submission(ComposerSubmission::Picker(
                    confirmation.submit,
                )))
            }
            KeyCode::Esc => {
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = None;
                }
                None
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = None;
                }
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(UiControl::Disconnect)
            }
            _ => None,
        };
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => return Some(UiControl::Disconnect),
            KeyCode::Char('c') => {
                state.picker = None;
                return None;
            }
            KeyCode::Char('a')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Skills)
                ) =>
            {
                if let Some(picker) = state.picker.as_mut() {
                    picker.select_all_skills();
                }
                return None;
            }
            KeyCode::Char('u')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Skills)
                ) =>
            {
                if let Some(picker) = state.picker.as_mut() {
                    picker.clear_skills();
                }
                return None;
            }
            KeyCode::Char('r')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                return selected_process_terminal(state).map_or_else(
                    || {
                        Some(UiControl::Notice(
                            "selected execution is not a terminal".to_owned(),
                        ))
                    },
                    |terminal| {
                        state.picker = None;
                        Some(UiControl::Submission(ComposerSubmission::Picker(
                            PickerSubmit::Attach {
                                terminal: Box::new(terminal),
                                writable: false,
                            },
                        )))
                    },
                );
            }
            KeyCode::Char('w')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                let Some(terminal) = selected_process_terminal(state) else {
                    return Some(UiControl::Notice(
                        "selected execution is not a terminal".to_owned(),
                    ));
                };
                let authority = if terminal.profile == ExecutionProfile::Host {
                    "HOST "
                } else {
                    ""
                };
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = Some(PickerConfirmation {
                        prompt: format!(
                            "Take the writable lease for {authority}terminal {}?",
                            terminal.terminal_id
                        ),
                        submit: PickerSubmit::Attach {
                            terminal: Box::new(terminal),
                            writable: true,
                        },
                    });
                }
                return None;
            }
            KeyCode::Char('k') | KeyCode::Char('K')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                let process = selected_process(state)?;
                if !process.state.is_live() {
                    return Some(UiControl::Notice(
                        "selected execution has already finished".to_owned(),
                    ));
                }
                let mode = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    agl_exec::KillMode::Immediate
                } else {
                    agl_exec::KillMode::Graceful
                };
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = Some(PickerConfirmation {
                        prompt: format!(
                            "Terminate execution {} with {mode:?} mode?",
                            process.execution_id
                        ),
                        submit: PickerSubmit::Kill {
                            execution_id: process.execution_id,
                            mode,
                        },
                    });
                }
                return None;
            }
            KeyCode::Char('p')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                let Some(terminal) = selected_process_terminal(state) else {
                    return Some(UiControl::Notice(
                        "selected execution is not a terminal".to_owned(),
                    ));
                };
                if !matches!(terminal.owner, TerminalOwnerView::Subagent { .. }) {
                    return Some(UiControl::Notice(
                        "only a subagent terminal can be promoted".to_owned(),
                    ));
                }
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = Some(PickerConfirmation {
                        prompt: format!(
                            "Promote subagent terminal {} to the durable session?",
                            terminal.terminal_id
                        ),
                        submit: PickerSubmit::Promote {
                            terminal_id: terminal.terminal_id,
                        },
                    });
                }
                return None;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => state.picker = None,
        KeyCode::Up => state.picker.as_mut()?.move_selection(-1),
        KeyCode::Down => state.picker.as_mut()?.move_selection(1),
        KeyCode::PageUp => state.picker.as_mut()?.move_selection(-8),
        KeyCode::PageDown => state.picker.as_mut()?.move_selection(8),
        KeyCode::Home => state.picker.as_mut()?.selected = 0,
        KeyCode::End => {
            let length = state.picker.as_ref()?.filtered_indices().len();
            state.picker.as_mut()?.selected = length.saturating_sub(1);
        }
        KeyCode::Backspace => state.picker.as_mut()?.pop_query(),
        KeyCode::Char(' ') if matches!(&state.picker.as_ref()?.kind, PickerKind::Skills) => {
            state.picker.as_mut()?.toggle_selected_skill()
        }
        KeyCode::Enter => return submit_current_picker(state),
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            state.picker.as_mut()?.push_query(character)
        }
        _ => {}
    }
    None
}

pub(super) fn selected_process(state: &UiState) -> Option<ProcessPickerItem> {
    match &state.picker.as_ref()?.selected_entry()?.payload {
        PickerPayload::Process(process) => Some(process.as_ref().clone()),
        _ => None,
    }
}

pub(super) fn selected_process_terminal(state: &UiState) -> Option<TerminalSessionView> {
    selected_process(state)?.terminal
}

pub(super) fn submit_current_picker(state: &mut UiState) -> Option<UiControl> {
    let submit = match picker_default_submit(state.picker.as_ref()?) {
        Ok(submit) => submit,
        Err(message) => return Some(UiControl::Notice(message.to_owned())),
    };
    match submit {
        PickerSubmit::EnsureHost { startup } => {
            let prompt = match startup {
                HostStartupPolicy::ManagedOnly => {
                    "Create or select a distinct HOST terminal with managed startup? This grants explicit Host process authority for that terminal lifetime."
                }
                HostStartupPolicy::SourceUserRc => {
                    "Create or select a distinct HOST terminal and source your normal shell rc? This grants explicit Host process authority and runs your user rc configuration."
                }
            };
            state.picker.as_mut()?.confirmation = Some(PickerConfirmation {
                prompt: prompt.to_owned(),
                submit: PickerSubmit::EnsureHost { startup },
            });
            None
        }
        submit => {
            state.picker = None;
            Some(UiControl::Submission(ComposerSubmission::Picker(submit)))
        }
    }
}

pub(super) fn picker_default_submit(
    picker: &PickerState,
) -> std::result::Result<PickerSubmit, &'static str> {
    Ok(match &picker.kind {
        PickerKind::Skills => {
            PickerSubmit::Skills(picker.selected_values.iter().cloned().collect::<Vec<_>>())
        }
        PickerKind::Processes => {
            let Some(entry) = picker.selected_entry() else {
                return Err("no matching execution is selected");
            };
            if let PickerPayload::EnsureHost(startup) = &entry.payload {
                return Ok(PickerSubmit::EnsureHost { startup: *startup });
            }
            let PickerPayload::Process(process) = &entry.payload else {
                return Err("process picker entry has an invalid action type");
            };
            let Some(terminal) = &process.terminal else {
                return Err(
                    "selected execution has no interactive terminal; use Ctrl+K to terminate it",
                );
            };
            PickerSubmit::Attach {
                writable: matches!(&terminal.owner, TerminalOwnerView::Human { .. }),
                terminal: Box::new(terminal.clone()),
            }
        }
        PickerKind::Resume | PickerKind::Model | PickerKind::Mode => {
            let Some(entry) = picker.selected_entry() else {
                return Err("no matching picker entry is selected");
            };
            match &entry.payload {
                PickerPayload::Resume(session_id) => PickerSubmit::Resume(session_id.clone()),
                PickerPayload::Model(model_id) => PickerSubmit::Model(model_id.clone()),
                PickerPayload::Mode(mode) => PickerSubmit::Mode(*mode),
                PickerPayload::Skill(_)
                | PickerPayload::EnsureHost(_)
                | PickerPayload::Process(_) => {
                    return Err("picker entry has an invalid action type");
                }
            }
        }
    })
}

pub(super) fn canonical_human_command(command: &str) -> Result<String> {
    let command = command.replace("\r\n", "\n");
    if command.contains('\r') {
        bail!("Shell command contains a lone carriage return");
    }
    Ok(command)
}

pub(super) fn selected_live_human_terminal(state: &UiState) -> Option<TerminalSessionView> {
    state
        .last_terminal
        .as_ref()
        .and_then(|terminal_id| {
            state.snapshot.terminals.iter().find(|terminal| {
                &terminal.terminal_id == terminal_id
                    && terminal.process_state.is_live()
                    && matches!(terminal.owner, TerminalOwnerView::Human { .. })
            })
        })
        .or_else(|| {
            state.snapshot.terminals.iter().find(|terminal| {
                terminal.process_state.is_live()
                    && matches!(terminal.owner, TerminalOwnerView::Human { .. })
            })
        })
        .cloned()
}

pub(super) async fn picker_suggestions(
    client: &AgentLibreClient,
    session_id: &SessionId,
    command_id: &str,
    argument_id: &str,
) -> Result<Vec<CommandSuggestion>> {
    let mut entries = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    for _ in 0..MAX_PICKER_PAGES {
        let page = client
            .command_suggestions(CommandSuggestionsRequest {
                session_id: Some(session_id.clone()),
                command_id: command_id.to_owned(),
                argument_id: argument_id.to_owned(),
                query: String::new(),
                cursor: cursor.clone(),
            })
            .await
            .with_context(|| format!("failed to load {argument_id} picker entries"))?;
        entries.extend(
            page.entries
                .into_iter()
                .take(MAX_PICKER_ENTRIES.saturating_sub(entries.len())),
        );
        if entries.len() >= MAX_PICKER_ENTRIES {
            break;
        }
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            bail!("daemon repeated a picker pagination cursor");
        }
        cursor = Some(next_cursor);
    }
    Ok(entries)
}

pub(super) async fn open_resume_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
) -> Result<()> {
    let suggestions = picker_suggestions(client, session_id, "session.resume", "selector").await?;
    let mut entries = Vec::with_capacity(suggestions.len());
    for suggestion in suggestions {
        let candidate = SessionId::parse(&suggestion.value)
            .context("daemon returned an invalid session picker ID")?;
        let detail = if &candidate == session_id {
            Some(match suggestion.detail {
                Some(detail) => format!("current · {detail}"),
                None => "current session".to_owned(),
            })
        } else {
            suggestion.detail
        };
        entries.push(PickerEntry {
            value: suggestion.value,
            label: suggestion.label,
            detail,
            payload: PickerPayload::Resume(candidate),
        });
    }
    if entries.is_empty() {
        state.notice("no resumable sessions are available");
    } else {
        state.picker = Some(PickerState::new(
            PickerKind::Resume,
            "Resume session",
            entries,
        ));
    }
    Ok(())
}

pub(super) async fn open_model_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
) -> Result<()> {
    let suggestions = picker_suggestions(client, session_id, "model.select", "model_id").await?;
    let current = state.snapshot.header.model_id.as_deref();
    let entries = suggestions
        .into_iter()
        .map(|suggestion| PickerEntry {
            detail: if current == Some(suggestion.value.as_str()) {
                Some(match suggestion.detail {
                    Some(detail) => format!("current · {detail}"),
                    None => "current model".to_owned(),
                })
            } else {
                suggestion.detail
            },
            payload: PickerPayload::Model(suggestion.value.clone()),
            value: suggestion.value,
            label: suggestion.label,
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        state.notice("no installed compatible models are available");
    } else {
        let mut picker = PickerState::new(PickerKind::Model, "Select model", entries);
        if let Some(current) = current {
            picker.select_value(current);
        }
        state.picker = Some(picker);
    }
    Ok(())
}

pub(super) fn open_mode_picker(state: &mut UiState) {
    let current = state.snapshot.header.operation_mode;
    let entries = operation_mode_picker_entries(current);
    let mut picker = PickerState::new(PickerKind::Mode, "Select operation mode", entries);
    if let Some(current) = picker
        .entries
        .iter()
        .find(|entry| entry.detail.as_deref() == Some("current mode"))
        .map(|entry| entry.value.clone())
    {
        picker.select_value(&current);
    }
    state.picker = Some(picker);
}

pub(super) fn operation_mode_picker_entries(current: ProtocolToolMode) -> Vec<PickerEntry> {
    [
        ("read-only", ProtocolToolMode::ReadOnly),
        ("write", ProtocolToolMode::Write),
        ("execute", ProtocolToolMode::Execute),
        ("approve", ProtocolToolMode::Approve),
        ("admin", ProtocolToolMode::Admin),
    ]
    .into_iter()
    .map(|(value, mode)| PickerEntry {
        value: value.to_owned(),
        label: value.to_owned(),
        detail: (mode == current).then(|| "current mode".to_owned()),
        payload: PickerPayload::Mode(mode),
    })
    .collect()
}

pub(super) async fn open_skills_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
) -> Result<()> {
    let suggestions = picker_suggestions(client, session_id, "skills.select", "skill_id").await?;
    let mut seen = BTreeSet::new();
    let mut entries = suggestions
        .into_iter()
        .map(|suggestion| {
            seen.insert(suggestion.value.clone());
            PickerEntry {
                payload: PickerPayload::Skill(suggestion.value.clone()),
                value: suggestion.value,
                label: suggestion.label,
                detail: suggestion.detail,
            }
        })
        .collect::<Vec<_>>();
    for selected in &state.snapshot.header.selected_skills {
        if seen.insert(selected.clone()) {
            entries.push(PickerEntry {
                value: selected.clone(),
                label: selected.clone(),
                detail: Some("currently selected; not in the admitted suggestion set".to_owned()),
                payload: PickerPayload::Skill(selected.clone()),
            });
        }
    }
    let mut picker = PickerState::new(PickerKind::Skills, "Select skills", entries);
    picker.selected_values = state
        .snapshot
        .header
        .selected_skills
        .iter()
        .cloned()
        .collect();
    if let Some(selected) = state.snapshot.header.selected_skills.first() {
        picker.select_value(selected);
    }
    state.picker = Some(picker);
    Ok(())
}

pub(super) async fn open_process_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    include_finished: bool,
) -> Result<()> {
    let terminals = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!(
                "terminal-ui-terminals-{}",
                agl_ids::RequestId::generate()
            ),
            action: ApplicationAction::TerminalList { include_finished },
        })
        .await
        .context("failed to load terminal picker entries")?;
    let ApplicationToolResult::Terminals { terminals } = terminals.result else {
        bail!("daemon returned an invalid terminal-list result");
    };
    let executions = state
        .snapshot
        .executions
        .iter()
        .filter(|execution| include_finished || execution.state.is_live())
        .cloned()
        .collect::<Vec<_>>();

    let terminals_by_execution = terminals
        .into_iter()
        .map(|terminal| (terminal.execution_id.clone(), terminal))
        .collect::<BTreeMap<_, _>>();
    let mut processes = executions
        .into_iter()
        .map(|execution| {
            let terminal = terminals_by_execution.get(&execution.execution_id).cloned();
            process_picker_item(execution, terminal)
        })
        .collect::<BTreeMap<_, _>>();
    for (execution_id, terminal) in terminals_by_execution {
        processes
            .entry(execution_id.clone())
            .or_insert_with(|| ProcessPickerItem {
                execution_id: terminal.execution_id.clone(),
                state: terminal.process_state,
                profile: terminal.profile,
                cwd: display_path(&terminal.cwd),
                terminal: Some(terminal),
            });
    }
    let mut entries = host_terminal_picker_entries();
    let remaining_entries = MAX_PICKER_ENTRIES.saturating_sub(entries.len());
    entries.extend(
        processes
            .into_values()
            .take(remaining_entries)
            .map(process_picker_entry),
    );
    let mut picker = PickerState::new(
        PickerKind::Processes,
        if include_finished {
            "Processes · live and finished"
        } else {
            "Processes · live"
        },
        entries,
    );
    if let Some(execution_id) = state.last_terminal.as_ref().and_then(|terminal_id| {
        picker.entries.iter().find_map(|entry| {
            let PickerPayload::Process(process) = &entry.payload else {
                return None;
            };
            process
                .terminal
                .as_ref()
                .filter(|terminal| &terminal.terminal_id == terminal_id)
                .map(|_| entry.value.clone())
        })
    }) {
        picker.select_value(&execution_id);
    }
    state.picker = Some(picker);
    Ok(())
}

pub(super) fn host_terminal_picker_entries() -> Vec<PickerEntry> {
    vec![
        PickerEntry {
            value: "action:host-terminal:managed".to_owned(),
            label: "Open HOST terminal".to_owned(),
            detail: Some(
                "managed startup · recommended · explicit Host authority confirmation".to_owned(),
            ),
            payload: PickerPayload::EnsureHost(HostStartupPolicy::ManagedOnly),
        },
        PickerEntry {
            value: "action:host-terminal:user-rc".to_owned(),
            label: "Open HOST terminal + user rc".to_owned(),
            detail: Some(
                "source normal shell rc · separate Host authority confirmation".to_owned(),
            ),
            payload: PickerPayload::EnsureHost(HostStartupPolicy::SourceUserRc),
        },
    ]
}

pub(super) fn process_picker_item(
    execution: ExecutionView,
    terminal: Option<TerminalSessionView>,
) -> (ExecutionId, ProcessPickerItem) {
    let execution_id = execution.execution_id;
    (
        execution_id.clone(),
        ProcessPickerItem {
            execution_id,
            state: execution.state,
            profile: execution.profile,
            cwd: display_path(&execution.cwd),
            terminal,
        },
    )
}

pub(super) fn process_picker_entry(process: ProcessPickerItem) -> PickerEntry {
    let (label, detail) = if let Some(terminal) = &process.terminal {
        let authority = terminal_authority_label(terminal.profile);
        (
            format!("terminal {}", terminal.terminal_id),
            format!(
                "{} · {authority} · {:?} · writer:{:?} · cwd:{}",
                terminal_owner_label(&terminal.owner),
                terminal.process_state,
                terminal.writer,
                display_path(&terminal.cwd),
            ),
        )
    } else {
        (
            format!("process {}", process.execution_id),
            format!(
                "{:?} · {:?} · cwd:{}",
                process.profile, process.state, process.cwd
            ),
        )
    };
    PickerEntry {
        value: process.execution_id.to_string(),
        label,
        detail: Some(detail),
        payload: PickerPayload::Process(Box::new(process)),
    }
}

pub(super) async fn handle_command(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    command: &str,
) -> Result<CommandOutcome> {
    let words = lex_command(command)?;
    let mut parts = words.into_iter();
    let invoked_name = parts.next().unwrap_or_default();
    let descriptor = state
        .catalog
        .iter()
        .find(|descriptor| {
            descriptor.name == invoked_name
                || descriptor
                    .aliases
                    .iter()
                    .any(|alias| alias == &invoked_name)
        })
        .map(|descriptor| (descriptor.name.clone(), descriptor.availability.clone()));
    let name = match descriptor {
        Some((_, CommandAvailability::Disabled { message, .. })) => {
            state.notice(format!("/{invoked_name} is unavailable: {message}"));
            return Ok(CommandOutcome::Continue);
        }
        Some((_, CommandAvailability::Hidden)) => {
            state.notice(format!("unknown command /{invoked_name}"));
            return Ok(CommandOutcome::Continue);
        }
        Some((name, CommandAvailability::Enabled)) => name,
        None => invoked_name,
    };
    let mut workspace_candidate = None;
    match name.as_str() {
        "disconnect" => return Ok(CommandOutcome::Disconnect),
        "help" => {
            state.notice("Use ↑/↓ in Command mode; Enter invokes the selected command");
            return Ok(CommandOutcome::Continue);
        }
        "exit"
            if !state.exit_armed
                && (state.snapshot.header.active_run_count > 0
                    || state.snapshot.header.queued_prompt_count > 0
                    || state.snapshot.header.active_execution_count > 0) =>
        {
            state.exit_armed = true;
            state
                .notice("Active work exists. Run /exit again to cancel it and finish the session.");
            return Ok(CommandOutcome::Continue);
        }
        _ => {}
    }
    let action = match name.as_str() {
        "status" => ApplicationAction::SessionStatus,
        "workspace" => match parts.next() {
            Some(path) => {
                let path = std::iter::once(path)
                    .chain(parts)
                    .collect::<Vec<_>>()
                    .join(" ");
                workspace_candidate = Some(path.clone());
                ApplicationAction::WorkspaceSet {
                    confirm_terminate_terminals: state.workspace_change_armed.as_deref()
                        == Some(path.as_str()),
                    path,
                }
            }
            None => ApplicationAction::WorkspaceGet,
        },
        "processes" => {
            let include_finished = match parts.next().as_deref() {
                None => false,
                Some("--all") => true,
                Some(_) => {
                    state.notice("usage: /processes [--all]");
                    return Ok(CommandOutcome::Continue);
                }
            };
            if parts.next().is_some() {
                state.notice("usage: /processes [--all]");
                return Ok(CommandOutcome::Continue);
            }
            if let Err(error) =
                open_process_picker(client, session_id, state, include_finished).await
            {
                state.notice(format!("process picker failed: {error:#}"));
            }
            return Ok(CommandOutcome::Continue);
        }
        "kill" => {
            let id = parts.next().context("/kill requires EXECUTION_ID")?;
            let execution_id = ExecutionId::parse(&id).context("invalid execution ID")?;
            let mode = if matches!(parts.next().as_deref(), Some("--immediate")) {
                agl_exec::KillMode::Immediate
            } else {
                agl_exec::KillMode::Graceful
            };
            let local_id = agl_exec::ExecutionId::parse(execution_id.as_str())
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            terminal_access()?
                .client()?
                .terminate_execution(local_id, mode, tokio_util::sync::CancellationToken::new())
                .await?;
            state.notice(format!(
                "execution {execution_id} termination requested ({mode:?})"
            ));
            return Ok(CommandOutcome::Continue);
        }
        "reload" => ApplicationAction::RuntimeContextReload,
        "clear" => ApplicationAction::SessionClear,
        "exit" => ApplicationAction::SessionExit {
            confirm_active: state.exit_armed,
        },
        "attach" => {
            let id = parts.next().context("/attach requires EXECUTION_ID")?;
            let execution_id = ExecutionId::parse(&id).context("invalid execution ID")?;
            let Some(candidate) = state
                .snapshot
                .terminals
                .iter()
                .find(|terminal| terminal.execution_id == execution_id)
                .cloned()
            else {
                state.notice("That execution is not an interactive terminal");
                return Ok(CommandOutcome::Continue);
            };
            let read_only = !matches!(candidate.owner, TerminalOwnerView::Human { .. })
                || matches!(parts.next().as_deref(), Some("--read-only"));
            state.last_terminal = Some(candidate.terminal_id.clone());
            return Ok(CommandOutcome::EnterTerminal(Box::new(
                TerminalViewRequest {
                    terminal: candidate,
                    writable: !read_only,
                },
            )));
        }
        "new" => ApplicationAction::SessionNew {
            launch: SessionLaunchOptions {
                // A presentation-only display path must never round-trip into
                // authority. The daemon inherits the source session's
                // canonical workspace for this session-scoped action.
                workspace_root: None,
                function_ref: None,
                model_id: state.snapshot.header.model_id.clone(),
                operation_mode: Some(state.snapshot.header.operation_mode),
                skill_ids: state.snapshot.header.selected_skills.clone(),
            },
        },
        "resume" => {
            let Some(selector) = parts.next() else {
                if let Err(error) = open_resume_picker(client, session_id, state).await {
                    state.notice(format!("session picker failed: {error:#}"));
                }
                return Ok(CommandOutcome::Continue);
            };
            if parts.next().is_some() {
                state.notice("usage: /resume [latest|SESSION_ID]");
                return Ok(CommandOutcome::Continue);
            }
            let selector = match selector.as_str() {
                "latest" => SessionSelector::Latest,
                value => SessionSelector::Id {
                    session_id: SessionId::parse(value).context("invalid session ID")?,
                },
            };
            ApplicationAction::SessionResume { selector }
        }
        "model" => {
            let Some(model_id) = parts.next() else {
                if let Err(error) = open_model_picker(client, session_id, state).await {
                    state.notice(format!("model picker failed: {error:#}"));
                }
                return Ok(CommandOutcome::Continue);
            };
            if parts.next().is_some() {
                state.notice("usage: /model [MODEL_ID]");
                return Ok(CommandOutcome::Continue);
            }
            ApplicationAction::ModelSelect { model_id }
        }
        "mode" => {
            let Some(mode) = parts.next() else {
                open_mode_picker(state);
                return Ok(CommandOutcome::Continue);
            };
            if parts.next().is_some() {
                state.notice("usage: /mode [read-only|write|execute|approve|admin]");
                return Ok(CommandOutcome::Continue);
            }
            ApplicationAction::OperationModeSelect {
                mode: parse_protocol_tool_mode(&mode)?,
            }
        }
        "skills" => {
            let skill_ids = parts.collect::<Vec<_>>();
            if skill_ids.is_empty() {
                if let Err(error) = open_skills_picker(client, session_id, state).await {
                    state.notice(format!("skills picker failed: {error:#}"));
                }
                return Ok(CommandOutcome::Continue);
            }
            ApplicationAction::SkillsSelect { skill_ids }
        }
        _ => {
            state.notice(format!("unknown command /{name}"));
            return Ok(CommandOutcome::Continue);
        }
    };
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!("cli-action-{}", agl_ids::RequestId::generate()),
            action,
        })
        .await;
    match response {
        Ok(event) => match event.result {
            ApplicationToolResult::SessionOpened { session_id, .. } => {
                return Ok(CommandOutcome::SwitchSession { session_id });
            }
            ApplicationToolResult::SessionExited { .. } => {
                return Ok(CommandOutcome::Disconnect);
            }
            ApplicationToolResult::Status { header } => {
                let notice = match name.as_str() {
                    "workspace" => Some(display_path(&header.workspace_root)),
                    _ => None,
                };
                state.snapshot.header = header;
                if let Some(notice) = notice {
                    state.notice(notice);
                }
            }
            ApplicationToolResult::WorkspaceChanged { header } => {
                state.snapshot.header = header;
                state.workspace_change_armed = None;
            }
            ApplicationToolResult::ModelChanged { header }
            | ApplicationToolResult::ModeChanged { header }
            | ApplicationToolResult::SkillsChanged { header } => {
                state.snapshot.header = header;
            }
            ApplicationToolResult::Cleared { .. } => {
                state.snapshot.items.clear();
                state.assistant_deltas.clear();
                state.notice("conversation context cleared");
            }
            _ => state.notice(format!("/{name} completed")),
        },
        Err(ClientError::Protocol {
            code: agl_protocol::ProtocolErrorCode::ConfirmationRequired,
            ..
        }) if name == "workspace" => {
            state.workspace_change_armed = workspace_candidate;
            state.notice(
                "Workspace change will terminate terminals tied to the current root. Run the same /workspace command again to confirm.",
            );
        }
        Err(error) => state.notice(format!("/{name} failed: {error}")),
    }
    Ok(CommandOutcome::Continue)
}

pub(super) fn lex_command(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in input.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if character == '\\' {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                word.push(character);
            }
            started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            character if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(character);
                started = true;
            }
        }
    }
    if escaped {
        bail!("command ends with an incomplete escape");
    }
    if quote.is_some() {
        bail!("command contains an unterminated quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

pub(super) fn parse_protocol_tool_mode(value: &str) -> Result<ProtocolToolMode> {
    match value {
        "read-only" => Ok(ProtocolToolMode::ReadOnly),
        "write" => Ok(ProtocolToolMode::Write),
        "execute" => Ok(ProtocolToolMode::Execute),
        "approve" => Ok(ProtocolToolMode::Approve),
        "admin" => Ok(ProtocolToolMode::Admin),
        _ => bail!(
            "invalid operation mode `{value}`; expected read-only, write, execute, approve, or admin"
        ),
    }
}
