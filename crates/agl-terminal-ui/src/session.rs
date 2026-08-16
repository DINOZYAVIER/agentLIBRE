use super::*;

pub(super) async fn resolve_session(
    client: &AgentLibreClient,
    options: &InteractiveOptions,
) -> Result<SessionId> {
    let action = match options.resume.as_deref() {
        None => ApplicationAction::SessionNew {
            launch: SessionLaunchOptions {
                workspace_root: options
                    .workspace_root
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                function_ref: options.function_ref.clone(),
                model_id: options.model_id.clone(),
                operation_mode: options.operation_mode.map(protocol_tool_mode),
                skill_ids: options.skills.clone(),
            },
        },
        Some("latest") => ApplicationAction::SessionResume {
            selector: SessionSelector::Latest,
        },
        Some(value) => ApplicationAction::SessionResume {
            selector: SessionSelector::Id {
                session_id: SessionId::parse(value).context("invalid --resume session ID")?,
            },
        },
    };
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: None,
            client_submission_id: format!("terminal-ui-launch-{}", agl_ids::RequestId::generate()),
            action,
        })
        .await
        .context("failed to open interactive session")?;
    match response.result {
        ApplicationToolResult::SessionOpened { session_id, .. } => Ok(session_id),
        result => bail!("daemon returned an invalid launch result: {result:?}"),
    }
}

pub(super) fn protocol_tool_mode(mode: ToolAccessMode) -> ProtocolToolMode {
    match mode {
        ToolAccessMode::Write => ProtocolToolMode::Write,
        ToolAccessMode::Execute => ProtocolToolMode::Execute,
        ToolAccessMode::Approve => ProtocolToolMode::Approve,
        ToolAccessMode::Admin => ProtocolToolMode::Admin,
        ToolAccessMode::ReadOnly => ProtocolToolMode::ReadOnly,
    }
}

pub(super) fn missing_daemon_message(socket_path: &Path) -> String {
    format!(
        "agentLIBRE daemon is unavailable at {}; install/start the user socket or run `agl serve --socket {}`",
        socket_path.display(),
        socket_path.display()
    )
}

pub(super) fn interactive_connect_error(socket_path: &Path, error: ClientError) -> anyhow::Error {
    let context = match &error {
        ClientError::DaemonUnavailable(_) | ClientError::Io(_) => {
            missing_daemon_message(socket_path)
        }
        _ => format!(
            "daemon at {} is running an incompatible protocol; restart it with the current `agl serve` binary",
            socket_path.display()
        ),
    };
    anyhow::Error::new(error).context(context)
}

pub(super) enum UiControl {
    Disconnect,
    CancelRun(agl_ids::RunId),
    ContinueIncomplete(MessageId),
    Submission(ComposerSubmission),
    Notice(String),
}

pub(super) enum SubmissionOutcome {
    Continue,
    Disconnect,
    EnterTerminal(Box<TerminalViewRequest>),
    SwitchSession { session_id: SessionId },
}

pub(super) enum CommandOutcome {
    Continue,
    Disconnect,
    EnterTerminal(Box<TerminalViewRequest>),
    SwitchSession { session_id: SessionId },
}

pub(super) enum TerminalPassthroughOutcome {
    Chat,
    Disconnect,
}

pub(super) struct TerminalViewRequest {
    pub(super) terminal: TerminalSessionView,
    pub(super) writable: bool,
}

pub(super) struct TerminalStreamState {
    pub(super) terminal: TerminalSessionView,
    pub(super) attachment: ExecutionAttachment,
    pub(super) filter: TerminalOutputFilter,
    pub(super) visible_cursor: u64,
    pub(super) drained_cursor: u64,
    pub(super) hidden_normal_output: bool,
    pub(super) replay_through_cursor: Option<u64>,
    pub(super) physical_alternate_screen: Arc<AtomicBool>,
    pub(super) panic_restore_bytes: Arc<Mutex<Vec<u8>>>,
    pub(super) writable: bool,
}

pub(super) struct PreparedSessionSwitch {
    pub(super) session_id: SessionId,
    pub(super) presentation: PresentationSubscription,
    pub(super) snapshot: SessionPresentationSnapshot,
    pub(super) catalog: Vec<CommandDescriptor>,
    pub(super) history: InputHistory,
    pub(super) warnings: Vec<String>,
}

pub(super) async fn prepare_session_switch(
    client: &AgentLibreClient,
    session_id: SessionId,
    state_dir: &Path,
    input_history: bool,
) -> Result<PreparedSessionSwitch> {
    let presentation = client
        .subscribe_presentation(SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .context("failed to load the selected session presentation")?;
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
        .context("failed to load the selected session command catalog")?
        .descriptors;
    let snapshot = presentation.snapshot.clone();
    let (history, warnings) = InputHistory::load(
        state_dir,
        &snapshot.header.workspace_history_scope,
        input_history,
    );
    Ok(PreparedSessionSwitch {
        session_id,
        presentation,
        snapshot,
        catalog,
        history,
        warnings,
    })
}

pub(super) fn install_session_switch(
    state: &mut UiState,
    snapshot: SessionPresentationSnapshot,
    catalog: Vec<CommandDescriptor>,
    history: InputHistory,
    warnings: Vec<String>,
) {
    state.snapshot = snapshot;
    state.catalog = catalog;
    state.history = history;
    state.last_terminal = state
        .snapshot
        .terminals
        .first()
        .map(|terminal| terminal.terminal_id.clone());
    state.terminal_cursors.clear();
    state.seen_terminals = state
        .snapshot
        .terminals
        .iter()
        .map(|terminal| terminal.terminal_id.clone())
        .collect();
    state.assistant_deltas.clear();
    state.continuation_submission_ids.clear();
    state.picker = None;
    state.active_run = state
        .snapshot
        .active_run
        .as_ref()
        .map(|active| active.run_id.clone());
    state.exit_armed = false;
    state.workspace_change_armed = None;
    state.pending_shell_submission = None;
    state.human_commands.clear();
    for warning in warnings {
        state.notice(warning);
    }
}

pub(super) fn install_presentation_snapshot(
    state: &mut UiState,
    snapshot: SessionPresentationSnapshot,
) {
    state.seen_terminals.extend(
        snapshot
            .terminals
            .iter()
            .map(|terminal| terminal.terminal_id.clone()),
    );
    state.active_run = snapshot
        .active_run
        .as_ref()
        .map(|active| active.run_id.clone());
    state.snapshot = snapshot;
    state.assistant_deltas.clear();
    state.continuation_submission_ids.retain(|message_id, _| {
        state.snapshot.items.iter().any(|item| {
            matches!(
                item,
                SessionPresentationItem::IncompleteAssistant { item }
                    if &item.message_id == message_id
            )
        })
    });
}

pub(super) type TerminalPhysicalIo<'a> = (
    &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
);
