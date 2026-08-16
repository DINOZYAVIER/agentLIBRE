use super::*;

pub(super) fn begin_shell_submission(
    session_id: &SessionId,
    state: &mut UiState,
    command: String,
    terminal_stream: &Option<TerminalStreamState>,
) -> Result<Option<ShellSubmissionTask>> {
    let command = canonical_human_command(&command)?;
    if let Some(pending) = state.pending_shell_submission.as_mut() {
        if pending.command != command {
            bail!("Shell command changed while its submission identity was retained");
        }
        if pending.in_flight {
            return Ok(None);
        }
        pending.in_flight = true;
    } else {
        state.pending_shell_submission = Some(PendingShellSubmission {
            command: command.clone(),
            client_submission_id: format!("terminal-ui-shell-{}", RequestId::generate()),
            terminal_ensure_submission_id: format!(
                "terminal-ui-terminal-{}",
                RequestId::generate()
            ),
            in_flight: true,
            outcome_uncertain: false,
        });
    }

    let pending = state
        .pending_shell_submission
        .as_ref()
        .expect("pending Shell submission was installed")
        .clone();
    let selected_terminal = selected_live_human_terminal(state);
    if selected_terminal.is_none() && state.shell_profile_id.is_none() {
        if let Some(pending) = state.pending_shell_submission.as_mut() {
            pending.in_flight = false;
        }
        bail!("configured shell is not an admitted managed Bash/Zsh profile");
    }
    let attach_after_sequence = selected_terminal.as_ref().map_or(0, |terminal| {
        terminal_stream
            .as_ref()
            .filter(|stream| stream.terminal.terminal_id == terminal.terminal_id)
            .map(|stream| stream.drained_cursor)
            .or_else(|| state.terminal_cursors.get(&terminal.execution_id).copied())
            .unwrap_or_default()
    });
    Ok(Some(ShellSubmissionTask {
        session_id: session_id.clone(),
        command,
        client_submission_id: pending.client_submission_id,
        terminal_ensure_submission_id: pending.terminal_ensure_submission_id,
        execution_context_revision: state.snapshot.header.execution_context_revision,
        shell_profile_id: state.shell_profile_id.clone(),
        terminal_size: current_terminal_size(),
        agl_env: current_terminal_environment(),
        selected_terminal,
        attach_after_sequence,
    }))
}

pub(super) fn shell_submission_failure(
    task: &ShellSubmissionTask,
    terminal: Option<TerminalSessionView>,
    attachment: Option<ShellSubmissionAttachment>,
    message: impl Into<String>,
    outcome_uncertain: bool,
) -> ShellSubmissionCompletion {
    ShellSubmissionCompletion {
        session_id: task.session_id.clone(),
        command: task.command.clone(),
        client_submission_id: task.client_submission_id.clone(),
        terminal,
        attachment,
        outcome: Err(ShellSubmissionFailure {
            message: message.into(),
            outcome_uncertain,
        }),
    }
}

pub(super) fn shell_submit_outcome_uncertain(error: &agl_terminal_client::ClientError) -> bool {
    !matches!(
        error,
        agl_terminal_client::ClientError::Protocol(_)
            | agl_terminal_client::ClientError::Remote(_)
            | agl_terminal_client::ClientError::UnexpectedResponse
    )
}

pub(super) async fn execute_shell_submission(
    client: AgentLibreClient,
    task: ShellSubmissionTask,
) -> ShellSubmissionCompletion {
    let (mut terminal, newly_created) = if let Some(terminal) = task.selected_terminal.clone() {
        (terminal, false)
    } else {
        let Some(shell_profile_id) = task.shell_profile_id.clone() else {
            return shell_submission_failure(
                &task,
                None,
                None,
                "configured shell is not an admitted managed Bash/Zsh profile",
                false,
            );
        };
        match client
            .ensure_human_terminal(HumanTerminalEnsureRequest {
                session_id: task.session_id.clone(),
                client_submission_id: task.terminal_ensure_submission_id.clone(),
                execution_context_revision: task.execution_context_revision,
                profile: ExecutionProfile::Workspace,
                shell_profile_id,
                terminal_size: task.terminal_size,
                agl_env: task.agl_env.clone(),
                host_startup: HostStartupPolicy::ManagedOnly,
            })
            .await
        {
            Ok(ensured) => (
                ensured.terminal,
                ensured.disposition == agl_protocol::TerminalEnsureDisposition::Created,
            ),
            Err(error) => {
                return shell_submission_failure(
                    &task,
                    None,
                    None,
                    format!("failed to ensure the Human workspace terminal: {error}"),
                    false,
                );
            }
        }
    };

    if terminal.prompt_state == TerminalPromptState::Starting && newly_created {
        let deadline = tokio::time::Instant::now() + SHELL_STARTUP_HANDSHAKE_TIMEOUT;
        while terminal.prompt_state == TerminalPromptState::Starting {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return shell_submission_failure(
                    &task,
                    Some(terminal),
                    None,
                    "new Human terminal did not reach a trusted prompt before the startup deadline",
                    false,
                );
            }
            tokio::time::sleep(SHELL_STARTUP_OBSERVE_INTERVAL.min(deadline - now)).await;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let snapshot = match tokio::time::timeout(
                remaining,
                client.session_presentation(SessionPresentationRequest {
                    session_id: task.session_id.clone(),
                    page_cursor: None,
                }),
            )
            .await
            {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => {
                    return shell_submission_failure(
                        &task,
                        Some(terminal),
                        None,
                        format!("failed to observe the new Human terminal startup: {error}"),
                        false,
                    );
                }
                Err(_) => {
                    return shell_submission_failure(
                        &task,
                        Some(terminal),
                        None,
                        "new Human terminal startup observation timed out",
                        false,
                    );
                }
            };
            let Some(latest) = snapshot
                .terminals
                .into_iter()
                .find(|candidate| candidate.terminal_id == terminal.terminal_id)
            else {
                return shell_submission_failure(
                    &task,
                    Some(terminal),
                    None,
                    "new Human terminal disappeared during startup",
                    false,
                );
            };
            terminal = latest;
        }
    }

    if terminal.prompt_state != TerminalPromptState::Ready {
        return shell_submission_failure(
            &task,
            Some(terminal),
            None,
            "Shell is busy or owns a foreground program; no bytes were sent. Attach Terminal to interact with it.",
            false,
        );
    }
    let Some(prompt_generation) = terminal.prompt_generation else {
        return shell_submission_failure(
            &task,
            Some(terminal),
            None,
            "Shell prompt readiness is stale; no bytes were sent",
            false,
        );
    };

    let after_sequence = task.attach_after_sequence;
    let attachment =
        match ExecutionAttachment::attach(&terminal.terminal_id, after_sequence, true).await {
            Ok(attachment) if attachment.started.writable => Some(ShellSubmissionAttachment {
                terminal: terminal.clone(),
                attachment,
                after_sequence,
            }),
            Ok(attachment) => {
                return shell_submission_failure(
                    &task,
                    Some(terminal.clone()),
                    Some(ShellSubmissionAttachment {
                        terminal,
                        attachment,
                        after_sequence,
                    }),
                    "Human terminal attachment is not writable; no bytes were sent",
                    false,
                );
            }
            Err(error) => {
                return shell_submission_failure(
                    &task,
                    Some(terminal),
                    None,
                    format!("failed to attach the Human terminal writer: {error}"),
                    false,
                );
            }
        };
    let attached = attachment
        .as_ref()
        .expect("successful writable attachment was installed");
    let local_terminal_id = agl_terminal::TerminalId::parse(terminal.terminal_id.as_str())
        .map_err(|error| anyhow::anyhow!(error.to_string()));
    let local_topology_id = agl_exec::CallerOwnerId::new(task.session_id.as_str())
        .map(agl_terminal::TerminalTopologyId::new)
        .map_err(|error| anyhow::anyhow!(error.to_string()));
    let outcome = match (local_terminal_id, local_topology_id) {
        (Ok(local_terminal_id), Ok(local_topology_id)) => terminal_access()
            .and_then(TerminalAccess::client)
            .map_err(|error| {
                agl_terminal_client::ClientError::Transport(
                    agl_terminal_client::TransportError::Unavailable(error.to_string()),
                )
            })
            .map(|terminal_client| (terminal_client, local_terminal_id, local_topology_id)),
        (Err(error), _) | (_, Err(error)) => Err(agl_terminal_client::ClientError::Transport(
            agl_terminal_client::TransportError::InvalidFrame(error.to_string()),
        )),
    };
    let outcome = match outcome {
        Ok((terminal_client, local_terminal_id, local_topology_id)) => {
            terminal_client
                .submit_command(
                    local_terminal_id,
                    local_topology_id,
                    attached.attachment.stream_id.clone(),
                    terminal.command_sequence,
                    prompt_generation,
                    task.command.clone(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        }
        Err(error) => Err(error),
    };
    match outcome {
        Ok(accepted) => ShellSubmissionCompletion {
            session_id: task.session_id,
            command: task.command,
            client_submission_id: task.client_submission_id,
            terminal: Some(terminal.clone()),
            attachment,
            outcome: Ok(ShellCommandAccepted {
                terminal_id: terminal.terminal_id,
                command_sequence: accepted.command_sequence,
            }),
        },
        Err(error) => {
            let outcome_uncertain = shell_submit_outcome_uncertain(&error);
            shell_submission_failure(
                &task,
                Some(terminal),
                attachment,
                format!("Shell command was not admitted: {error}"),
                outcome_uncertain,
            )
        }
    }
}

pub(super) fn spawn_shell_submission(
    client: AgentLibreClient,
    task: ShellSubmissionTask,
    sender: mpsc::Sender<UiAsyncEvent>,
) {
    tokio::spawn(async move {
        let completion = execute_shell_submission(client, task).await;
        let _ = sender
            .send(UiAsyncEvent::ShellSubmission(Box::new(completion)))
            .await;
    });
}

pub(super) async fn handle_submission(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    submission: ComposerSubmission,
    sender: &mpsc::Sender<UiAsyncEvent>,
) -> Result<SubmissionOutcome> {
    if let ComposerSubmission::Prompt(input) = &submission
        && let Err(error) = state.history.record_prompt(input)
    {
        state.notice(format!("prompt history write failed: {error:#}"));
    }
    match submission {
        ComposerSubmission::Prompt(content) => {
            spawn_prompt(client.clone(), session_id.clone(), content, sender.clone());
        }
        ComposerSubmission::Shell(_) => {
            unreachable!("Shell submissions use the nonblocking admission path")
        }
        ComposerSubmission::SwitchTerminal => {
            if let Some(terminal) = state.last_terminal.as_ref().and_then(|terminal_id| {
                state
                    .snapshot
                    .terminals
                    .iter()
                    .find(|terminal| {
                        &terminal.terminal_id == terminal_id && terminal.process_state.is_live()
                    })
                    .cloned()
            }) {
                let writable = matches!(terminal.owner, TerminalOwnerView::Human { .. });
                return Ok(SubmissionOutcome::EnterTerminal(Box::new(
                    TerminalViewRequest { terminal, writable },
                )));
            }
            let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let shell_profile_id = state.shell_profile_id.clone().ok_or_else(|| {
                anyhow::anyhow!("configured shell is not an admitted managed Bash/Zsh profile")
            })?;
            let ensured = client
                .ensure_human_terminal(HumanTerminalEnsureRequest {
                    session_id: session_id.clone(),
                    client_submission_id: format!(
                        "terminal-ui-terminal-{}",
                        agl_ids::RequestId::generate()
                    ),
                    execution_context_revision: state.snapshot.header.execution_context_revision,
                    profile: ExecutionProfile::Workspace,
                    shell_profile_id,
                    terminal_size: TerminalSize {
                        columns: columns.max(1),
                        rows: rows.max(1),
                    },
                    agl_env: current_terminal_environment(),
                    host_startup: HostStartupPolicy::ManagedOnly,
                })
                .await
                .context("failed to ensure the Human workspace terminal")?;
            state.last_terminal = Some(ensured.terminal.terminal_id.clone());
            return Ok(SubmissionOutcome::EnterTerminal(Box::new(
                TerminalViewRequest {
                    terminal: ensured.terminal,
                    writable: true,
                },
            )));
        }
        ComposerSubmission::Command(command) => {
            return match handle_command(client, session_id, state, &command).await? {
                CommandOutcome::Continue => Ok(SubmissionOutcome::Continue),
                CommandOutcome::Disconnect => Ok(SubmissionOutcome::Disconnect),
                CommandOutcome::EnterTerminal(request) => {
                    Ok(SubmissionOutcome::EnterTerminal(request))
                }
                CommandOutcome::SwitchSession { session_id } => {
                    Ok(SubmissionOutcome::SwitchSession { session_id })
                }
            };
        }
        ComposerSubmission::Picker(submit) => {
            return handle_picker_submit(client, session_id, state, submit).await;
        }
    }
    Ok(SubmissionOutcome::Continue)
}

pub(super) async fn handle_picker_submit(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    submit: PickerSubmit,
) -> Result<SubmissionOutcome> {
    let action = match submit {
        PickerSubmit::Resume(session_id) => ApplicationAction::SessionResume {
            selector: SessionSelector::Id { session_id },
        },
        PickerSubmit::Model(model_id) => ApplicationAction::ModelSelect { model_id },
        PickerSubmit::Mode(mode) => ApplicationAction::OperationModeSelect { mode },
        PickerSubmit::Skills(skill_ids) => ApplicationAction::SkillsSelect { skill_ids },
        PickerSubmit::EnsureHost { startup } => {
            return Ok(handle_host_terminal_submit(client, session_id, state, startup).await);
        }
        PickerSubmit::Attach { terminal, writable } => {
            state.last_terminal = Some(terminal.terminal_id.clone());
            return Ok(SubmissionOutcome::EnterTerminal(Box::new(
                TerminalViewRequest {
                    terminal: *terminal,
                    writable,
                },
            )));
        }
        PickerSubmit::Kill { execution_id, mode } => {
            let local_id = agl_exec::ExecutionId::parse(execution_id.as_str())
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            terminal_access()?
                .client()?
                .terminate_execution(local_id, mode, tokio_util::sync::CancellationToken::new())
                .await?;
            state.notice(format!(
                "execution {execution_id} termination requested ({mode:?})"
            ));
            return Ok(SubmissionOutcome::Continue);
        }
        PickerSubmit::Promote { terminal_id } => ApplicationAction::TerminalPromote { terminal_id },
    };
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!("cli-picker-{}", agl_ids::RequestId::generate()),
            action,
        })
        .await;
    match response {
        Ok(event) => match event.result {
            ApplicationToolResult::SessionOpened { session_id, .. } => {
                Ok(SubmissionOutcome::SwitchSession { session_id })
            }
            ApplicationToolResult::ModelChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("model selection changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::ModeChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("operation mode changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::SkillsChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("skill selection changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::TerminalPromoted { terminal } => {
                state.last_terminal = Some(terminal.terminal_id.clone());
                let _ = apply_presentation_event(
                    state,
                    agl_protocol::SessionPresentationEventPayload::TerminalChanged { terminal },
                );
                state.notice("subagent terminal promoted to the durable session");
                Ok(SubmissionOutcome::Continue)
            }
            result => bail!("daemon returned an invalid picker action result: {result:?}"),
        },
        Err(error) => {
            state.notice(format!("picker action failed: {error}"));
            Ok(SubmissionOutcome::Continue)
        }
    }
}

pub(super) trait HostTerminalEnsurer {
    async fn ensure_host_terminal(
        &self,
        request: HumanHostTerminalEnsureRequest,
    ) -> std::result::Result<HumanTerminalEnsuredEvent, ClientError>;
}

impl HostTerminalEnsurer for AgentLibreClient {
    async fn ensure_host_terminal(
        &self,
        request: HumanHostTerminalEnsureRequest,
    ) -> std::result::Result<HumanTerminalEnsuredEvent, ClientError> {
        self.ensure_human_host_terminal(request).await
    }
}

pub(super) async fn handle_host_terminal_submit(
    client: &impl HostTerminalEnsurer,
    session_id: &SessionId,
    state: &mut UiState,
    startup: HostStartupPolicy,
) -> SubmissionOutcome {
    let request = match host_terminal_request(session_id, state, startup, current_terminal_size()) {
        Ok(request) => request,
        Err(error) => {
            state.notice(format!("HOST terminal ensure failed: {error:#}"));
            return SubmissionOutcome::Continue;
        }
    };
    let ensured = match client.ensure_host_terminal(request).await {
        Ok(ensured) => ensured,
        Err(error) => {
            state.notice(format!("HOST terminal ensure failed: {error}"));
            return SubmissionOutcome::Continue;
        }
    };
    let terminal = ensured.terminal;
    if terminal.profile != ExecutionProfile::Host
        || !matches!(
            &terminal.owner,
            TerminalOwnerView::Human {
                session_id: owner_session_id
            } if owner_session_id == session_id
        )
    {
        state.notice("HOST terminal ensure failed: daemon returned a non-Human Host terminal");
        return SubmissionOutcome::Continue;
    }
    if state.snapshot.terminals.iter().any(|workspace| {
        workspace.profile == ExecutionProfile::Workspace
            && (workspace.terminal_id == terminal.terminal_id
                || workspace.execution_id == terminal.execution_id)
    }) {
        state.notice(
            "HOST terminal ensure failed: daemon attempted to reuse a Workspace terminal identity",
        );
        return SubmissionOutcome::Continue;
    }
    if !terminal.process_state.is_live() {
        state.notice("HOST terminal ensure failed: durable Host terminal is not live");
        return SubmissionOutcome::Continue;
    }

    state.last_terminal = Some(terminal.terminal_id.clone());
    let _ = apply_presentation_event(
        state,
        agl_protocol::SessionPresentationEventPayload::TerminalAdded {
            terminal: terminal.clone(),
        },
    );
    SubmissionOutcome::EnterTerminal(Box::new(TerminalViewRequest {
        terminal,
        writable: true,
    }))
}

pub(super) fn host_terminal_request(
    session_id: &SessionId,
    state: &UiState,
    startup: HostStartupPolicy,
    terminal_size: TerminalSize,
) -> Result<HumanHostTerminalEnsureRequest> {
    let shell_profile_id = state.shell_profile_id.clone().ok_or_else(|| {
        anyhow::anyhow!("configured shell is not an admitted managed Bash/Zsh profile")
    })?;
    Ok(HumanHostTerminalEnsureRequest {
        terminal: HumanTerminalEnsureRequest {
            session_id: session_id.clone(),
            client_submission_id: format!("cli-host-terminal-{}", agl_ids::RequestId::generate()),
            execution_context_revision: state.snapshot.header.execution_context_revision,
            profile: ExecutionProfile::Host,
            shell_profile_id,
            terminal_size,
            agl_env: current_terminal_environment(),
            host_startup: startup,
        },
        confirm_host_authority: true,
    })
}

pub(super) fn current_terminal_size() -> TerminalSize {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    TerminalSize {
        columns: columns.max(1),
        rows: rows.max(1),
    }
}

pub(super) fn current_terminal_environment() -> StructuredEnvironmentOverlay {
    let terminal_name = std::env::var("TERM").ok();
    terminal_environment_for(terminal_name.as_deref())
}

pub(super) fn terminal_environment_for(
    terminal_name: Option<&str>,
) -> StructuredEnvironmentOverlay {
    const DEFAULT_TERMINAL_NAME: &str = "xterm-256color";
    const MAX_TERMINAL_NAME_BYTES: usize = 128;

    let terminal_name = terminal_name
        .filter(|name| {
            !name.is_empty()
                && name.len() <= MAX_TERMINAL_NAME_BYTES
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'.')
                })
        })
        .unwrap_or(DEFAULT_TERMINAL_NAME);
    StructuredEnvironmentOverlay {
        values: BTreeMap::from([("TERM".to_owned(), terminal_name.to_owned())]),
        ..StructuredEnvironmentOverlay::default()
    }
}

#[cfg(target_os = "linux")]
pub(super) fn install_shell_submission_attachment(
    state: &mut UiState,
    terminal_stream: &mut Option<TerminalStreamState>,
    returned: ShellSubmissionAttachment,
) {
    finish_terminal_stream(terminal_stream, state);
    state
        .seen_terminals
        .insert(returned.terminal.terminal_id.clone());
    state.terminal_cursors.insert(
        returned.terminal.execution_id.clone(),
        returned.after_sequence,
    );
    let writable = returned.attachment.started.writable;
    *terminal_stream = Some(TerminalStreamState {
        terminal: returned.terminal,
        attachment: returned.attachment,
        filter: TerminalOutputFilter::new(false),
        visible_cursor: returned.after_sequence,
        drained_cursor: returned.after_sequence,
        hidden_normal_output: false,
        replay_through_cursor: None,
        physical_alternate_screen: Arc::new(AtomicBool::new(false)),
        panic_restore_bytes: Arc::new(Mutex::new(Vec::new())),
        writable,
    });
}

pub(super) fn apply_shell_submission_completion(
    state: &mut UiState,
    session_id: &SessionId,
    terminal_stream: Option<&mut Option<TerminalStreamState>>,
    mut completion: ShellSubmissionCompletion,
) {
    let matches_pending = &completion.session_id == session_id
        && state
            .pending_shell_submission
            .as_ref()
            .is_some_and(|pending| {
                pending.command == completion.command
                    && pending.client_submission_id == completion.client_submission_id
            });
    if !matches_pending {
        return;
    }
    if let Some(terminal) = completion.terminal.as_ref() {
        state.last_terminal = Some(terminal.terminal_id.clone());
    }
    if let (Some(terminal_stream), Some(attachment)) =
        (terminal_stream, completion.attachment.take())
    {
        install_shell_submission_attachment(state, terminal_stream, attachment);
    }

    match completion.outcome {
        Ok(accepted) => {
            let accepted_matches = completion
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.terminal_id == accepted.terminal_id);
            if !accepted_matches {
                let _ = update(
                    state,
                    UiEvent::ShellRejected {
                        message: "Shell acceptance named a different terminal; exact command and request identity retained".to_owned(),
                        client_submission_id: completion.client_submission_id,
                        outcome_uncertain: true,
                    },
                );
                return;
            }
            state.last_terminal = Some(accepted.terminal_id.clone());
            state.human_commands.push(LocalHumanCommandCard {
                terminal_id: accepted.terminal_id,
                command_sequence: accepted.command_sequence,
                command: completion.command.clone(),
                state: LocalHumanCommandState::Running,
            });
            if state.human_commands.len() > MAX_LOCAL_HUMAN_COMMAND_CARDS {
                state.human_commands.remove(0);
            }
            let _ = update(
                state,
                UiEvent::ShellAccepted {
                    command_sequence: accepted.command_sequence,
                },
            );
        }
        Err(failure) => {
            let _ = update(
                state,
                UiEvent::ShellRejected {
                    message: failure.message,
                    client_submission_id: completion.client_submission_id,
                    outcome_uncertain: failure.outcome_uncertain,
                },
            );
        }
    }
}
