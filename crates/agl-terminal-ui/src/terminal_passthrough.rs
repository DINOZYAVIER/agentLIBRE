use super::*;

#[cfg(target_os = "linux")]
pub(super) async fn run_terminal_passthrough(
    client: &AgentLibreClient,
    request: TerminalViewRequest,
    state: &mut UiState,
    presentation: &mut PresentationSubscription,
    async_events: &mut mpsc::Receiver<UiAsyncEvent>,
    terminal_io: TerminalPhysicalIo<'_>,
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<TerminalPassthroughOutcome> {
    let result = run_terminal_passthrough_inner(
        client,
        request,
        state,
        presentation,
        async_events,
        terminal_io,
        terminal_stream,
    )
    .await;
    let restore_result = restore_chat_terminal_modes(terminal_stream);
    match (result, restore_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

#[cfg(target_os = "linux")]
pub(super) async fn run_terminal_passthrough_inner(
    client: &AgentLibreClient,
    request: TerminalViewRequest,
    state: &mut UiState,
    presentation: &mut PresentationSubscription,
    async_events: &mut mpsc::Receiver<UiAsyncEvent>,
    terminal_io: TerminalPhysicalIo<'_>,
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<TerminalPassthroughOutcome> {
    let (terminal, interrupt_signal, terminate_signal, suspend_signal, resize_signal) = terminal_io;
    let TerminalViewRequest {
        terminal: terminal_view,
        writable,
    } = request;
    let terminal_id = terminal_view.terminal_id.clone();
    let execution_id = terminal_view.execution_id.clone();
    prepare_terminal_stream(
        client,
        terminal_view.clone(),
        writable,
        state,
        terminal_stream,
    )
    .await?;
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    terminal
        .clear()
        .context("failed to clear Chat before Terminal view")?;
    let mut stdout = io::stdout();
    execute!(stdout, Show).context("failed to restore the Terminal cursor")?;
    let stream = terminal_stream
        .as_mut()
        .context("Terminal stream was not installed")?;
    refresh_terminal_panic_restore(stream);
    let _restore_chat_view = ChatViewRestore {
        physical_alternate_screen: Arc::clone(&stream.physical_alternate_screen),
        restore_bytes: Arc::clone(&stream.panic_restore_bytes),
    };
    writeln!(
        stdout,
        "\r! agentLIBRE Terminal · {} · {} · {} · ! Enter at prompt · Esc then ! in foreground → Chat\r",
        terminal_owner_label(&terminal_view.owner),
        terminal_authority_label(terminal_view.profile),
        if stream.writable {
            "writable"
        } else {
            "read-only"
        },
    )?;
    sync_physical_terminal_modes(&mut stdout, stream)?;
    stream.filter.set_visible(true);
    stream
        .attachment
        .resize(columns.max(1), rows.max(1))
        .await
        .context("failed to send the Terminal view size")?;
    stdout.flush()?;

    let raw_input = RawTtyInput::open().context("failed to open /dev/tty for Terminal input")?;
    let mut input_buffer = [0_u8; 4096];
    let mut input_gate = RawTerminalInputGate::default();
    let initial_actions = update_terminal_input_gate(&mut input_gate, terminal_view.prompt_state);
    if forward_terminal_actions(&stream.attachment, stream.writable, initial_actions).await? {
        stream.filter.set_visible(false);
        return Ok(TerminalPassthroughOutcome::Chat);
    }
    let blocked_before = stream.filter.blocked_total();
    let malformed_before = stream.filter.malformed_total();
    let clock = Instant::now();
    let mut stream_ended = false;
    let mut disconnect = false;

    'terminal_view: loop {
        tokio::select! {
            read = raw_input.read(&mut input_buffer) => {
                let count = read.context("failed to read Terminal input")?;
                if count == 0 {
                    break 'terminal_view;
                }
                let actions = input_gate.handle_bytes(&input_buffer[..count], clock.elapsed());
                if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                    break 'terminal_view;
                }
            }
            event = stream.attachment.next() => {
                match event.context("Terminal attachment failed")? {
                    Some(ExecutionAttachmentEvent::Output(event)) => {
                        state.terminal_cursors.insert(execution_id.clone(), event.chunk.sequence);
                        stream.visible_cursor = event.chunk.sequence;
                        stream.drained_cursor = event.chunk.sequence;
                        let bytes = event.chunk.bytes.decode(65_536)
                            .context("daemon sent an invalid Terminal output chunk")?;
                        let stale_replay = stream
                            .replay_through_cursor
                            .is_some_and(|cursor| event.chunk.sequence <= cursor);
                        let report = if stale_replay {
                            stream.filter.filter_stale_replay(&bytes)
                        } else {
                            stream.filter.filter(&bytes)
                        };
                        refresh_terminal_panic_restore(stream);
                        let replay_completed = stream
                            .replay_through_cursor
                            .is_some_and(|cursor| event.chunk.sequence >= cursor);
                        if replay_completed {
                            stream.replay_through_cursor = None;
                        }
                        if !report.bytes.is_empty() {
                            stdout.write_all(&report.bytes)?;
                            stdout.flush()?;
                        }
                        if replay_completed {
                            sync_physical_terminal_modes(&mut stdout, stream)?;
                        } else {
                            stream
                                .physical_alternate_screen
                                .store(stream.filter.alternate_screen(), Ordering::Release);
                        }
                    }
                    Some(ExecutionAttachmentEvent::Finished(event)) => {
                        state.terminal_cursors.insert(execution_id.clone(), event.last_delivered_sequence);
                        state.notice(format!("Terminal process ended: {:?}", event.state));
                        stream_ended = true;
                        break 'terminal_view;
                    }
                    None => {
                        state.notice("Terminal attachment ended");
                        stream_ended = true;
                        break 'terminal_view;
                    }
                }
            }
            event = presentation.next() => {
                match event {
                    Ok(Some(PresentationSubscriptionEvent::SnapshotReplaced { snapshot, .. })) => {
                        install_presentation_snapshot(state, *snapshot);
                        reload_command_catalog(client, state).await?;
                        let prompt_state = terminal_prompt_from_snapshot(&state.snapshot, &terminal_id);
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                    Ok(Some(PresentationSubscriptionEvent::Event(event))) => {
                        let prompt_state = terminal_prompt_from_event(&event.event, &terminal_id);
                        let outcome = apply_presentation_event(state, event.event.clone());
                        if outcome.resync_required {
                            state.notice("presentation delta gap; installing a fresh snapshot");
                            resubscribe_presentation(client, state, presentation).await?;
                        } else if outcome.command_catalog_changed {
                            reload_command_catalog(client, state).await?;
                        }
                        if let Some(prompt_state) = prompt_state {
                            let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                            if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                                break 'terminal_view;
                            }
                        }
                    }
                    Ok(Some(PresentationSubscriptionEvent::Finished(event))) => {
                        if event.reason == agl_protocol::PresentationSubscriptionFinishReason::SessionFinished {
                            disconnect = true;
                            break 'terminal_view;
                        }
                        state.notice(format!(
                            "presentation ended ({:?}); loading a fresh snapshot",
                            event.reason
                        ));
                        resubscribe_presentation(client, state, presentation).await?;
                        let prompt_state = state.snapshot.terminals.iter()
                            .find(|terminal| terminal.terminal_id == terminal_id)
                            .map(|terminal| terminal.prompt_state)
                            .unwrap_or(TerminalPromptState::Unavailable);
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                    Ok(None) => bail!("session presentation stream ended without a terminal event"),
                    Err(error) => {
                        state.notice(format!("presentation needs resync: {error}"));
                        resubscribe_presentation(client, state, presentation).await?;
                        let prompt_state = state.snapshot.terminals.iter()
                            .find(|terminal| terminal.terminal_id == terminal_id)
                            .map(|terminal| terminal.prompt_state)
                            .unwrap_or(TerminalPromptState::Unavailable);
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                }
            }
            event = async_events.recv() => {
                if let Some(event) = event {
                    let session_id = state.snapshot.header.session_id.clone();
                    let before = state.snapshot.terminals.iter()
                        .find(|terminal| terminal.terminal_id == terminal_id)
                        .map(|terminal| terminal.prompt_state);
                    apply_async_event(state, &session_id, event, None);
                    let after = state.snapshot.terminals.iter()
                        .find(|terminal| terminal.terminal_id == terminal_id)
                        .map(|terminal| terminal.prompt_state);
                    if after != before && let Some(prompt_state) = after {
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                }
            }
            resize = resize_signal.recv() => {
                if resize.is_none() {
                    bail!("Terminal resize signal stream ended");
                }
                let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                stream.attachment.resize(columns.max(1), rows.max(1)).await
                    .context("failed to resize the Terminal view")?;
            }
            signal = interrupt_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGINT signal stream ended");
                }
                disconnect = true;
                break 'terminal_view;
            }
            signal = terminate_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTERM signal stream ended");
                }
                disconnect = true;
                break 'terminal_view;
            }
            signal = suspend_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTSTP signal stream ended");
                }
                restore_chat_terminal_modes_for_stream(&mut stdout, stream)?;
                restore_physical_terminal();
                if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
                    bail!("failed to suspend the interactive process");
                }
                enable_raw_mode().context("failed to restore raw mode after SIGCONT")?;
                execute!(stdout, EnableBracketedPaste, Show)
                    .context("failed to restore Terminal mode after SIGCONT")?;
                resubscribe_presentation(client, state, presentation).await?;
                let prompt_state = state.snapshot.terminals.iter()
                    .find(|terminal| terminal.terminal_id == terminal_id)
                    .map(|terminal| terminal.prompt_state)
                    .unwrap_or(TerminalPromptState::Unavailable);
                let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                    break 'terminal_view;
                }
                sync_physical_terminal_modes(&mut stdout, stream)?;
                let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                stream.attachment.resize(columns.max(1), rows.max(1)).await
                    .context("failed to redraw the Terminal view after SIGCONT")?;
            }
        }
    }

    stream.filter.set_visible(false);
    let blocked = stream.filter.blocked_total().saturating_sub(blocked_before);
    let malformed = stream
        .filter
        .malformed_total()
        .saturating_sub(malformed_before);
    if blocked > 0 || malformed > 0 {
        state.notice(format!(
            "Terminal filtered {} high-risk and {} malformed control sequence(s)",
            blocked, malformed,
        ));
    }
    if stream_ended {
        finish_terminal_stream(terminal_stream, state);
    }
    Ok(if disconnect {
        TerminalPassthroughOutcome::Disconnect
    } else {
        TerminalPassthroughOutcome::Chat
    })
}

pub(super) async fn prepare_terminal_stream(
    _client: &AgentLibreClient,
    terminal_view: TerminalSessionView,
    writable: bool,
    state: &mut UiState,
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<()> {
    let execution_id = terminal_view.execution_id.clone();
    if let Some(mut existing) = terminal_stream.take() {
        if existing.terminal.terminal_id == terminal_view.terminal_id {
            existing.replay_through_cursor = None;
            let replay_after =
                if existing.writable != writable || existing.filter.alternate_screen() {
                    Some(existing.drained_cursor)
                } else if existing.hidden_normal_output {
                    Some(existing.visible_cursor)
                } else {
                    None
                };
            if let Some(after_sequence) = replay_after {
                let replay_through_cursor =
                    (after_sequence == existing.visible_cursor).then_some(existing.drained_cursor);
                existing.attachment.detach().await.ok();
                existing.attachment = ExecutionAttachment::attach(
                    &terminal_view.terminal_id,
                    after_sequence,
                    writable,
                )
                .await
                .context("failed to resume the Human terminal")?;
                existing.visible_cursor = after_sequence;
                existing.drained_cursor = after_sequence;
                existing.hidden_normal_output = false;
                existing.replay_through_cursor = replay_through_cursor;
                if !existing.filter.alternate_screen() {
                    existing.filter = TerminalOutputFilter::new(true);
                }
            }
            existing.terminal = terminal_view;
            existing.writable = existing.attachment.started.writable;
            existing.filter.set_visible(true);
            *terminal_stream = Some(existing);
            return Ok(());
        }
        existing.attachment.detach().await.ok();
        let _ = existing.filter.finish();
    }

    let first_attach = state
        .seen_terminals
        .insert(terminal_view.terminal_id.clone());
    let after_sequence = if first_attach {
        0
    } else {
        state
            .terminal_cursors
            .get(&execution_id)
            .copied()
            .unwrap_or_default()
    };
    let attachment =
        ExecutionAttachment::attach(&terminal_view.terminal_id, after_sequence, writable)
            .await
            .context("failed to attach the Human terminal")?;
    state.terminal_cursors.insert(execution_id, after_sequence);
    let writable = attachment.started.writable;
    *terminal_stream = Some(TerminalStreamState {
        terminal: terminal_view,
        attachment,
        filter: TerminalOutputFilter::new(true),
        visible_cursor: after_sequence,
        drained_cursor: after_sequence,
        hidden_normal_output: false,
        replay_through_cursor: None,
        physical_alternate_screen: Arc::new(AtomicBool::new(false)),
        panic_restore_bytes: Arc::new(Mutex::new(Vec::new())),
        writable,
    });
    Ok(())
}

pub(super) fn sync_physical_terminal_modes(
    stdout: &mut io::Stdout,
    stream: &mut TerminalStreamState,
) -> Result<()> {
    refresh_terminal_panic_restore(stream);
    stdout.write_all(&stream.filter.terminal_mode_restore_bytes())?;
    stream
        .physical_alternate_screen
        .store(stream.filter.alternate_screen(), Ordering::Release);
    stdout.flush()?;
    Ok(())
}

pub(super) fn restore_chat_terminal_modes(
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<()> {
    let Some(stream) = terminal_stream.as_mut() else {
        return Ok(());
    };
    restore_chat_terminal_modes_for_stream(&mut io::stdout(), stream)
}

pub(super) fn restore_chat_terminal_modes_for_stream(
    stdout: &mut io::Stdout,
    stream: &mut TerminalStreamState,
) -> Result<()> {
    stdout.write_all(&stream.filter.chat_mode_restore_bytes())?;
    stdout.flush()?;
    stream
        .physical_alternate_screen
        .store(false, Ordering::Release);
    Ok(())
}

pub(super) fn refresh_terminal_panic_restore(stream: &TerminalStreamState) {
    let bytes = stream.filter.chat_mode_restore_bytes();
    *stream
        .panic_restore_bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = bytes;
}

pub(super) async fn next_hidden_terminal_event(
    terminal_stream: &mut Option<TerminalStreamState>,
) -> std::result::Result<Option<ExecutionAttachmentEvent>, agl_terminal_client::ClientError> {
    match terminal_stream.as_mut() {
        Some(stream) => stream.attachment.next().await,
        None => std::future::pending().await,
    }
}

pub(super) fn finish_terminal_stream(
    terminal_stream: &mut Option<TerminalStreamState>,
    state: &mut UiState,
) {
    let Some(mut stream) = terminal_stream.take() else {
        return;
    };
    let report = stream.filter.finish();
    state
        .terminal_cursors
        .insert(stream.terminal.execution_id, stream.drained_cursor);
    if report.malformed_sequences > 0 {
        state.notice("Terminal stream ended inside a malformed control sequence");
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) async fn run_terminal_passthrough(
    _client: &AgentLibreClient,
    _request: TerminalViewRequest,
    _state: &mut UiState,
    _presentation: &mut PresentationSubscription,
    _async_events: &mut mpsc::Receiver<UiAsyncEvent>,
    _terminal_io: TerminalPhysicalIo<'_>,
    _terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<TerminalPassthroughOutcome> {
    bail!("Terminal view is currently supported only on Linux")
}

pub(super) async fn forward_terminal_actions(
    attachment: &ExecutionAttachment,
    writable: bool,
    actions: Vec<TerminalInputAction>,
) -> Result<bool> {
    for action in actions {
        match action {
            TerminalInputAction::Forward(bytes) if writable && !bytes.is_empty() => {
                attachment
                    .input(agl_exec::ProcessBytes::from_bytes(&bytes), false)
                    .await
                    .context("failed to forward Terminal input")?;
            }
            TerminalInputAction::Forward(_) => {}
            TerminalInputAction::SwitchToChat => return Ok(true),
        }
    }
    Ok(false)
}

pub(super) fn update_terminal_input_gate(
    input_gate: &mut RawTerminalInputGate,
    prompt_state: TerminalPromptState,
) -> Vec<TerminalInputAction> {
    match prompt_state {
        TerminalPromptState::Ready => {
            input_gate.prompt_ready();
            Vec::new()
        }
        TerminalPromptState::Degraded | TerminalPromptState::Unavailable => {
            input_gate.integration_degraded()
        }
        TerminalPromptState::Starting
        | TerminalPromptState::CommandRunning
        | TerminalPromptState::ForegroundProcess => input_gate.prompt_busy(),
    }
}

pub(super) fn terminal_prompt_from_event(
    event: &agl_protocol::SessionPresentationEventPayload,
    terminal_id: &TerminalId,
) -> Option<TerminalPromptState> {
    match event {
        agl_protocol::SessionPresentationEventPayload::TerminalAdded { terminal }
        | agl_protocol::SessionPresentationEventPayload::TerminalChanged { terminal }
            if &terminal.terminal_id == terminal_id =>
        {
            Some(terminal.prompt_state)
        }
        agl_protocol::SessionPresentationEventPayload::TerminalRemoved {
            terminal_id: removed,
        } if removed == terminal_id => Some(TerminalPromptState::Unavailable),
        agl_protocol::SessionPresentationEventPayload::TerminalCommandStarted {
            terminal_id: changed,
            ..
        } if changed == terminal_id => Some(TerminalPromptState::CommandRunning),
        _ => None,
    }
}

pub(super) fn terminal_prompt_from_snapshot(
    snapshot: &SessionPresentationSnapshot,
    terminal_id: &TerminalId,
) -> TerminalPromptState {
    snapshot
        .terminals
        .iter()
        .find(|terminal| &terminal.terminal_id == terminal_id)
        .map(|terminal| terminal.prompt_state)
        .unwrap_or(TerminalPromptState::Unavailable)
}

pub(super) async fn resubscribe_presentation(
    client: &AgentLibreClient,
    state: &mut UiState,
    presentation: &mut PresentationSubscription,
) -> Result<()> {
    let replacement = client
        .subscribe_presentation(SessionPresentationSubscribeRequest {
            session_id: state.snapshot.header.session_id.clone(),
        })
        .await
        .context("failed to resubscribe to the session presentation")?;
    install_presentation_snapshot(state, replacement.snapshot.clone());
    *presentation = replacement;
    reload_command_catalog(client, state).await?;
    Ok(())
}

pub(super) async fn reload_command_catalog(
    client: &AgentLibreClient,
    state: &mut UiState,
) -> Result<()> {
    state.catalog = client
        .command_catalog(CommandCatalogRequest {
            session_id: Some(state.snapshot.header.session_id.clone()),
            client_effects: vec![
                ClientEffectKind::Help,
                ClientEffectKind::Disconnect,
                ClientEffectKind::InputHistory,
            ],
        })
        .await
        .context("failed to refresh the command catalog")?
        .descriptors;
    Ok(())
}
