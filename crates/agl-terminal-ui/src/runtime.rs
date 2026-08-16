use super::*;

pub(super) static TERMINAL_ACCESS: OnceLock<TerminalAccess> = OnceLock::new();

#[derive(Clone)]
pub(super) struct TerminalAccess {
    socket_path: PathBuf,
    runtime_root: PathBuf,
    installed_generation: TerminalGenerationIdentity,
    authority: agl_exec::AuthorityFingerprint,
}

impl TerminalAccess {
    pub(super) fn from_runtime(runtime: &UiRuntimeConfig) -> Result<Self> {
        let executable = std::env::current_exe().context("failed to resolve terminal UI binary")?;
        let generation_root = executable
            .parent()
            .context("terminal UI binary has no generation directory")?;
        let generation = VerifiedTerminalGeneration::load_installed(generation_root)
            .context("failed to verify terminal UI generation")?;
        if generation.file_path(TerminalGenerationFileRole::Ui) != executable {
            bail!("terminal UI must execute its sealed generation binary");
        }
        Ok(Self {
            socket_path: runtime.terminal_runtime_dir.join("terminal.sock"),
            runtime_root: runtime.terminal_runtime_dir.clone(),
            installed_generation: generation.identity().clone(),
            authority: agl_exec::AuthorityFingerprint::new(LOCAL_OPERATOR_AUTHORITY_FINGERPRINT)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
        })
    }

    pub(super) fn client(&self) -> Result<TerminalClient<UnixTerminalTransport>> {
        let transport = UnixTerminalTransport::new(self.socket_path.clone())?;
        TerminalClient::for_generation_with_runtime_projection(
            transport,
            self.installed_generation.clone(),
            Some(self.authority.clone()),
            self.runtime_root.join("service-identity.json"),
        )
        .map_err(Into::into)
    }

    pub(super) async fn bootstrap(&self) -> Result<()> {
        self.client()?
            .bootstrap_identity(tokio_util::sync::CancellationToken::new())
            .await
            .context("terminal generation handshake failed")?;
        Ok(())
    }
}

pub(super) fn terminal_access() -> Result<&'static TerminalAccess> {
    TERMINAL_ACCESS
        .get()
        .context("terminal access was not initialized")
}

pub(super) async fn run_interactive_async(
    options: InteractiveOptions,
    runtime: &UiRuntimeConfig,
) -> Result<()> {
    let socket_path = options
        .socket_path
        .clone()
        .unwrap_or_else(|| runtime.agent_state_dir.join("daemon/agl.sock"));
    let client = AgentLibreClient::connect(&socket_path)
        .await
        .map_err(|error| interactive_connect_error(&socket_path, error))?;
    let terminal_access = TerminalAccess::from_runtime(runtime)?;
    terminal_access.bootstrap().await?;
    let _ = TERMINAL_ACCESS.set(terminal_access);
    let mut session_id = resolve_session(&client, &options).await?;
    let mut presentation = client
        .subscribe_presentation(SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .context("failed to subscribe to the session presentation")?;
    let catalog = client
        .command_catalog(CommandCatalogRequest {
            session_id: Some(session_id.clone()),
            client_effects: vec![
                ClientEffectKind::Help,
                ClientEffectKind::Disconnect,
                ClientEffectKind::InputHistory,
            ],
        })
        .await
        .context("failed to load the command catalog")?
        .descriptors;
    let (history, history_warnings) = InputHistory::load(
        &runtime.ui_state_dir,
        &presentation.snapshot.header.workspace_history_scope,
        options.input_history,
    );
    let mut notices =
        vec!["Type ! for Shell commands, / for product commands, Ctrl+D to disconnect".to_owned()];
    notices.extend(history_warnings);
    let seen_terminals = presentation
        .snapshot
        .terminals
        .iter()
        .map(|terminal| terminal.terminal_id.clone())
        .collect();
    let mut state = UiState {
        snapshot: presentation.snapshot.clone(),
        catalog,
        composer: Composer::default(),
        last_terminal: presentation
            .snapshot
            .terminals
            .first()
            .map(|terminal| terminal.terminal_id.clone()),
        terminal_cursors: BTreeMap::new(),
        seen_terminals,
        assistant_deltas: BTreeMap::new(),
        continuation_submission_ids: BTreeMap::new(),
        picker: None,
        notices,
        active_run: None,
        exit_armed: false,
        workspace_change_armed: None,
        shell_profile_id: managed_shell_profile_id(&runtime.shell_program).map(str::to_owned),
        history,
        activity_expanded: false,
        pending_shell_submission: None,
        human_commands: Vec::new(),
        no_color: std::env::var_os("NO_COLOR").is_some(),
    };
    let (async_sender, mut async_events) = mpsc::channel(UI_EVENT_CAPACITY);
    let mut pending_terminal: Option<Box<TerminalViewRequest>> = None;
    let mut terminal_stream = None;
    let mut interrupt_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("failed to install SIGINT handling")?;
    let mut terminate_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("failed to install SIGTERM handling")?;
    let mut suspend_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))
            .context("failed to install SIGTSTP handling")?;
    let mut resize_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .context("failed to install SIGWINCH handling")?;
    let mut terminal_mode = TuiTerminalMode::enter()?;
    let mut render_tick = tokio::time::interval(CHAT_FRAME_INTERVAL);
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let height = crossterm::terminal::size()
        .map(|(_, rows)| rows.max(8))
        .unwrap_or(24);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("failed to initialize terminal UI")?;
    let mut input = Some(ChatInput::new().context("failed to start Chat input reader")?);

    let result = loop {
        if let Some(terminal_request) = pending_terminal.take() {
            drop(input.take());
            match run_terminal_passthrough(
                &client,
                *terminal_request,
                &mut state,
                &mut presentation,
                &mut async_events,
                (
                    &mut terminal,
                    &mut interrupt_signal,
                    &mut terminate_signal,
                    &mut suspend_signal,
                    &mut resize_signal,
                ),
                &mut terminal_stream,
            )
            .await
            {
                Ok(TerminalPassthroughOutcome::Chat) => {}
                Ok(TerminalPassthroughOutcome::Disconnect) => break Ok(()),
                Err(error) => state.notice(format!("Terminal view ended: {error:#}")),
            }
            if let Some(stream) = terminal_stream.as_mut() {
                stream.filter.set_visible(false);
            }
            // Reconcile the inline viewport before restarting Crossterm's
            // asynchronous reader. On Unix, inline resize asks the terminal
            // for its cursor position through the same global reader.
            terminal
                .autoresize()
                .context("failed to resize restored Chat view")?;
            terminal.clear().context("failed to restore Chat view")?;
            terminal
                .draw(|frame| draw(frame, &state))
                .context("failed to redraw restored Chat view")?;
            input = Some(ChatInput::new().context("failed to restart Chat input reader")?);
        }
        tokio::select! {
            _ = render_tick.tick() => {
                terminal
                    .draw(|frame| draw(frame, &state))
                    .context("failed to render terminal UI")?;
            }
            event = input.as_mut().expect("Chat input stream is installed").next() => {
                let Some(event) = event else { break Ok(()); };
                match event.context("failed to read terminal input")? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(control) = handle_key(&mut state, key) {
                            match control {
                                UiControl::Disconnect => break Ok(()),
                                UiControl::CancelRun(run_id) => {
                                    match client.cancel_run(run_id).await {
                                        Ok(_) => state.notice("active turn cancellation requested"),
                                        Err(error) => state.notice(format!("cancel failed: {error}")),
                                    }
                                }
                                UiControl::ContinueIncomplete(message_id) => {
                                    if let Err(error) = continue_incomplete_output(
                                        &client,
                                        &session_id,
                                        &mut state,
                                        message_id,
                                    )
                                    .await
                                    {
                                        state.notice(format!(
                                            "Continue failed; retry keeps the same request identity: {error:#}"
                                        ));
                                    }
                                }
                                UiControl::Notice(message) => state.notice(message),
                                UiControl::Submission(submission) => {
                                    if let ComposerSubmission::Shell(command) = &submission {
                                        match begin_shell_submission(
                                            &session_id,
                                            &mut state,
                                            command.clone(),
                                            &terminal_stream,
                                        ) {
                                            Ok(Some(task)) => {
                                                spawn_shell_submission(
                                                    client.clone(),
                                                    task,
                                                    async_sender.clone(),
                                                );
                                            }
                                            Ok(None) => state.notice(
                                                "Shell command admission is already pending",
                                            ),
                                            Err(error) => state.notice(format!(
                                                "Shell submission was not started: {error:#}"
                                            )),
                                        }
                                        continue;
                                    }
                                    match handle_submission(
                                        &client,
                                        &session_id,
                                        &mut state,
                                        submission,
                                        &async_sender,
                                    ).await {
                                        Err(error) => state.notice(format!(
                                            "submission failed; session remains active: {error:#}"
                                        )),
                                        Ok(SubmissionOutcome::Continue) => {}
                                        Ok(SubmissionOutcome::Disconnect) => break Ok(()),
                                        Ok(SubmissionOutcome::EnterTerminal(request)) => {
                                            pending_terminal = Some(request);
                                        }
                                        Ok(SubmissionOutcome::SwitchSession { session_id: next_session_id }) => {
                                            match prepare_session_switch(
                                                &client,
                                                next_session_id,
                                                &runtime.ui_state_dir,
                                                options.input_history,
                                            )
                                            .await
                                            {
                                                Ok(next) => {
                                                    let PreparedSessionSwitch {
                                                        session_id: next_session_id,
                                                        presentation: next_presentation,
                                                        snapshot,
                                                        catalog,
                                                        history,
                                                        warnings,
                                                    } = next;
                                                    if let Some(stream) = terminal_stream.as_mut() {
                                                        stream.attachment.detach().await.ok();
                                                    }
                                                    terminal_stream = None;
                                                    session_id = next_session_id;
                                                    presentation = next_presentation;
                                                    install_session_switch(
                                                        &mut state,
                                                        snapshot,
                                                        catalog,
                                                        history,
                                                        warnings,
                                                    );
                                                    state.notice(format!("switched to session {session_id}"));
                                                }
                                                Err(error) => state.notice(format!(
                                                    "session switch failed; source session remains active: {error:#}"
                                                )),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(picker) = state.picker.as_mut() {
                            for character in text.chars() {
                                picker.push_query(character);
                            }
                        } else {
                            let _ = update(&mut state, UiEvent::Paste(text));
                        }
                    }
                    Event::Resize(_, _) => {
                        drop(input.take());
                        terminal.autoresize()?;
                        terminal
                            .draw(|frame| draw(frame, &state))
                            .context("failed to render resized Chat view")?;
                        input = Some(
                            ChatInput::new().context("failed to restart Chat input reader")?
                        );
                    }
                    _ => {}
                }
            }
            event = presentation.next() => {
                match event {
                    Ok(Some(PresentationSubscriptionEvent::SnapshotReplaced { snapshot, .. })) => {
                        install_presentation_snapshot(&mut state, *snapshot);
                        reload_command_catalog(&client, &mut state).await?;
                    }
                    Ok(Some(PresentationSubscriptionEvent::Event(event))) => {
                        let outcome = apply_presentation_event(&mut state, event.event.clone());
                        if outcome.resync_required {
                            state.notice("presentation delta gap; installing a fresh snapshot");
                            resubscribe_presentation(&client, &mut state, &mut presentation)
                                .await?;
                        } else if outcome.command_catalog_changed {
                            reload_command_catalog(&client, &mut state).await?;
                        }
                    }
                    Ok(Some(PresentationSubscriptionEvent::Finished(event))) => {
                        if event.reason == agl_protocol::PresentationSubscriptionFinishReason::SessionFinished {
                            break Ok(());
                        }
                        state.notice(format!(
                            "presentation ended ({:?}); loading a fresh snapshot",
                            event.reason
                        ));
                        resubscribe_presentation(&client, &mut state, &mut presentation).await?;
                    }
                    Ok(None) => bail!("session presentation stream ended without a terminal event"),
                    Err(error) => {
                        state.notice(format!("presentation needs resync: {error}"));
                        resubscribe_presentation(&client, &mut state, &mut presentation).await?;
                    }
                }
            }
            event = async_events.recv() => {
                if let Some(event) = event {
                    apply_async_event(
                        &mut state,
                        &session_id,
                        event,
                        Some(&mut terminal_stream),
                    );
                }
            }
            event = next_hidden_terminal_event(&mut terminal_stream) => {
                match event {
                    Ok(Some(ExecutionAttachmentEvent::Output(event))) => {
                        if let Some(stream) = terminal_stream.as_mut() {
                            let bytes = event.chunk.bytes.decode(65_536)
                                .context("daemon sent an invalid hidden Terminal output chunk")?;
                            stream.filter.set_visible(false);
                            let was_alternate = stream.filter.alternate_screen();
                            let report = stream.filter.filter(&bytes);
                            stream.drained_cursor = event.chunk.sequence;
                            state.terminal_cursors.insert(
                                stream.terminal.execution_id.clone(),
                                event.chunk.sequence,
                            );
                            if (!was_alternate || !stream.filter.alternate_screen())
                                && !report.bytes.is_empty()
                            {
                                stream.hidden_normal_output = true;
                            }
                        }
                    }
                    Ok(Some(ExecutionAttachmentEvent::Finished(event))) => {
                        state.notice(format!("Terminal process ended: {:?}", event.state));
                        finish_terminal_stream(&mut terminal_stream, &mut state);
                    }
                    Ok(None) => finish_terminal_stream(&mut terminal_stream, &mut state),
                    Err(error) => {
                        state.notice(format!("Terminal background stream ended: {error}"));
                        finish_terminal_stream(&mut terminal_stream, &mut state);
                    }
                }
            }
            signal = interrupt_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGINT signal stream ended");
                }
                break Ok(());
            }
            signal = terminate_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTERM signal stream ended");
                }
                break Ok(());
            }
            signal = suspend_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTSTP signal stream ended");
                }
                // ChatInput joins its polling thread on drop, so synchronous
                // inline-viewport cursor queries cannot race it after SIGCONT.
                drop(input.take());
                terminal_mode.suspend();
                if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
                    bail!("failed to suspend the interactive process");
                }
                terminal_mode.resume()?;
                resubscribe_presentation(&client, &mut state, &mut presentation).await?;
                terminal.autoresize().context("failed to resize Chat after SIGCONT")?;
                terminal.clear().context("failed to redraw Chat after SIGCONT")?;
                terminal
                    .draw(|frame| draw(frame, &state))
                    .context("failed to render Chat after SIGCONT")?;
                input = Some(ChatInput::new().context("failed to restart Chat input reader")?);
            }
            signal = resize_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGWINCH signal stream ended");
                }
                drop(input.take());
                terminal.autoresize()?;
                terminal
                    .draw(|frame| draw(frame, &state))
                    .context("failed to render resized Chat view")?;
                input = Some(ChatInput::new().context("failed to restart Chat input reader")?);
            }
        }
    };
    drop(input.take());
    drop(terminal);
    drop(terminal_mode);
    result
}
