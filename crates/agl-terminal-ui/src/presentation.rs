use super::*;

pub(super) struct ChatViewRestore {
    pub(super) physical_alternate_screen: Arc<AtomicBool>,
    pub(super) restore_bytes: Arc<Mutex<Vec<u8>>>,
}

impl Drop for ChatViewRestore {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let restore_bytes = self
            .restore_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = stdout.write_all(&restore_bytes);
        if self.physical_alternate_screen.swap(false, Ordering::AcqRel) {
            let _ = stdout.write_all(b"\x1b[?47l\x1b[?1047l\x1b[?1049l");
        }
        if std::thread::panicking() {
            let _ = execute!(stdout, DisableBracketedPaste, Show);
            let _ = stdout.flush();
            let _ = disable_raw_mode();
        } else {
            let _ = execute!(stdout, EnableBracketedPaste, Show);
            let _ = stdout.flush();
        }
    }
}

pub(super) fn spawn_prompt(
    client: AgentLibreClient,
    session_id: SessionId,
    content: String,
    sender: mpsc::Sender<UiAsyncEvent>,
) {
    tokio::spawn(async move {
        let accepted = match client
            .submit_prompt(RunSubmitRequest {
                session_id: session_id.clone(),
                content: match agl_content::Content::text(content) {
                    Ok(content) => content,
                    Err(error) => {
                        let _ = sender.send(UiAsyncEvent::Notice(error.to_string())).await;
                        return;
                    }
                },
                client_submission_id: format!("cli-prompt-{}", agl_ids::RequestId::generate()),
                budget: RunBudgetRequest::default(),
            })
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("prompt rejected: {error}")))
                    .await;
                return;
            }
        };
        let _ = sender
            .send(UiAsyncEvent::RunAccepted {
                session_id: session_id.clone(),
                run_id: accepted.run_id.clone(),
                state: accepted.state,
            })
            .await;
        let mut run = match client
            .subscribe_run(RunSubscribeRequest {
                run_id: accepted.run_id,
                after_sequence: 0,
            })
            .await
        {
            Ok(run) => run,
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("run stream failed: {error}")))
                    .await;
                return;
            }
        };
        while let Ok(Some(event)) = run.next().await {
            if let RunSubscriptionEvent::Finished(finished) = event {
                if let Some(notice) = run_finished_notice(&finished) {
                    let _ = sender.send(UiAsyncEvent::Notice(notice)).await;
                }
                break;
            }
        }
        match client
            .session_presentation(SessionPresentationRequest {
                session_id: session_id.clone(),
                page_cursor: None,
            })
            .await
        {
            Ok(snapshot) => {
                let _ = sender
                    .send(UiAsyncEvent::Snapshot {
                        session_id,
                        snapshot: Box::new(snapshot),
                    })
                    .await;
            }
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("refresh failed: {error}")))
                    .await;
            }
        }
    });
}

pub(super) fn run_finished_notice(finished: &RunSubscriptionFinishedEvent) -> Option<String> {
    let prefix = match finished.state {
        ProtocolRunState::Succeeded => return None,
        ProtocolRunState::Incomplete => "turn incomplete".to_owned(),
        ProtocolRunState::Failed => "turn failed".to_owned(),
        ProtocolRunState::Cancelled => "turn cancelled".to_owned(),
        state => format!("turn finished: {state:?}"),
    };
    let message_budget = MAX_RUN_FINISHED_NOTICE_BYTES.saturating_sub(prefix.len() + 2);
    if let Some(message) = finished
        .error_message
        .as_deref()
        .and_then(|message| sanitize_notice_detail(message, message_budget))
    {
        return Some(format!("{prefix}: {message}"));
    }
    let code_budget = MAX_RUN_FINISHED_NOTICE_BYTES.saturating_sub(prefix.len() + 3);
    if let Some(code) = finished
        .error_code
        .as_deref()
        .and_then(|code| sanitize_notice_detail(code, code_budget))
    {
        return Some(format!("{prefix} ({code})"));
    }
    Some(prefix)
}

pub(super) fn sanitize_notice_detail(value: &str, maximum_bytes: usize) -> Option<String> {
    const ELLIPSIS: &str = "…";

    let value = value.trim();
    if value.is_empty() || maximum_bytes == 0 {
        return None;
    }
    let mut output = String::new();
    let mut truncated = false;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        let fragment = if character.is_control() || is_unicode_format_control(character as u32) {
            format!("\\u{{{:X}}}", character as u32)
        } else {
            character.to_string()
        };
        let truncation_reserve = if characters.peek().is_some() {
            ELLIPSIS.len()
        } else {
            0
        };
        if output
            .len()
            .saturating_add(fragment.len())
            .saturating_add(truncation_reserve)
            > maximum_bytes
        {
            truncated = true;
            break;
        }
        output.push_str(&fragment);
    }
    if truncated && output.len().saturating_add(ELLIPSIS.len()) <= maximum_bytes {
        output.push_str(ELLIPSIS);
    }
    (!output.is_empty()).then_some(output)
}

pub(super) fn is_unicode_format_control(code: u32) -> bool {
    matches!(
        code,
        0x00ad
            | 0x061c
            | 0x06dd
            | 0x070f
            | 0x180e
            | 0xfeff
            | 0x110bd
            | 0x110cd
            | 0xe0001
            | 0x0600..=0x0605
            | 0x0890..=0x0891
            | 0x08e2
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x206f
            | 0xfff9..=0xfffb
            | 0x13430..=0x1343f
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
    )
}

pub(super) async fn continue_incomplete_output(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    message_id: MessageId,
) -> Result<()> {
    let available = state.snapshot.items.iter().any(|item| {
        matches!(
            item,
            SessionPresentationItem::IncompleteAssistant { item }
                if item.message_id == message_id
                    && matches!(
                        item.continue_action,
                        agl_protocol::ContinueActionView::Available
                    )
        )
    });
    if !available {
        state.notice("incomplete output is no longer available to continue");
        return Ok(());
    }
    let client_submission_id = state
        .continuation_submission_ids
        .entry(message_id.clone())
        .or_insert_with(|| format!("cli-incomplete-continue-{}", agl_ids::RequestId::generate()))
        .clone();
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id,
            action: ApplicationAction::IncompleteTurnContinue {
                message_id: message_id.clone(),
                expected_execution_context_revision: state
                    .snapshot
                    .header
                    .execution_context_revision,
            },
        })
        .await
        .context("daemon rejected incomplete-output continuation")?;
    let ApplicationToolResult::IncompleteTurnContinued { admission } = response.result else {
        bail!("daemon returned an invalid incomplete-output continuation result")
    };
    if admission.session_id != *session_id {
        bail!("daemon returned a continuation for a different session");
    }
    for item in &mut state.snapshot.items {
        if let SessionPresentationItem::IncompleteAssistant { item } = item
            && item.message_id == message_id
        {
            item.continue_action = agl_protocol::ContinueActionView::Claimed {
                continuation_run_id: admission.run_id.clone(),
            };
        }
    }
    if admission.state == agl_protocol::PromptAdmissionState::Running {
        state.active_run = Some(admission.run_id.clone());
    }
    state.notice(format!(
        "Continue admitted as {} ({:?}, position {})",
        admission.run_id, admission.state, admission.ordinal
    ));
    Ok(())
}

pub(super) fn apply_async_event(
    state: &mut UiState,
    session_id: &SessionId,
    event: UiAsyncEvent,
    terminal_stream: Option<&mut Option<TerminalStreamState>>,
) {
    match event {
        UiAsyncEvent::ShellSubmission(completion) => {
            apply_shell_submission_completion(state, session_id, terminal_stream, *completion)
        }
        UiAsyncEvent::RunAccepted {
            session_id: event_session_id,
            ..
        }
        | UiAsyncEvent::Snapshot {
            session_id: event_session_id,
            ..
        } if &event_session_id != session_id => {}
        UiAsyncEvent::RunAccepted {
            run_id,
            state: run_state,
            ..
        } => {
            let _ = update(
                state,
                UiEvent::RunAccepted {
                    run_id,
                    state: run_state,
                },
            );
        }
        UiAsyncEvent::Snapshot { snapshot, .. } => {
            let _ = update(state, UiEvent::Snapshot(snapshot));
        }
        UiAsyncEvent::Notice(message) => {
            let _ = update(state, UiEvent::Notice(message));
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PresentationApplyOutcome {
    pub(super) command_catalog_changed: bool,
    pub(super) resync_required: bool,
}

pub(super) fn apply_presentation_event(
    state: &mut UiState,
    event: agl_protocol::SessionPresentationEventPayload,
) -> PresentationApplyOutcome {
    let command_catalog_changed = matches!(
        &event,
        agl_protocol::SessionPresentationEventPayload::CommandAvailabilityChanged
    );
    let mut resync_required = false;
    match event {
        agl_protocol::SessionPresentationEventPayload::HeaderChanged { header } => {
            state.snapshot.header = header
        }
        agl_protocol::SessionPresentationEventPayload::ItemUpsert { item } => {
            if let SessionPresentationItem::AssistantMessage { message_id, .. } = &item {
                state.assistant_deltas.remove(message_id);
            }
            let key = presentation_item_key(&item);
            if let Some(existing) = state
                .snapshot
                .items
                .iter_mut()
                .find(|existing| presentation_item_key(existing) == key)
            {
                *existing = item;
            } else {
                state.snapshot.items.push(item);
            }
        }
        agl_protocol::SessionPresentationEventPayload::ItemRemoved { item_key } => {
            state
                .snapshot
                .items
                .retain(|item| presentation_item_key(item) != item_key);
            state
                .assistant_deltas
                .retain(|message_id, _| message_id.to_string() != item_key);
        }
        agl_protocol::SessionPresentationEventPayload::AssistantTextDelta {
            run_id,
            provisional_message_id,
            sequence,
            text,
            ..
        } => {
            match append_assistant_delta(
                &mut state.assistant_deltas,
                run_id,
                provisional_message_id,
                sequence,
                &text,
            ) {
                AssistantDeltaApply::SequenceGap => {
                    resync_required = true;
                    state.notice("assistant presentation delta gap; fresh snapshot required");
                }
                AssistantDeltaApply::BoundExceeded => state.notice(
                    "assistant presentation delta exceeded its private display bound; waiting for the durable final message",
                ),
                AssistantDeltaApply::Applied | AssistantDeltaApply::Duplicate => {}
            }
        }
        agl_protocol::SessionPresentationEventPayload::PromptQueued { prompt } => {
            if let Some(existing) = state
                .snapshot
                .queued_prompts
                .iter_mut()
                .find(|existing| existing.run_id == prompt.run_id)
            {
                *existing = prompt;
            } else {
                state.snapshot.queued_prompts.push(prompt);
            }
            sync_ui_prompt_counts(&mut state.snapshot);
        }
        agl_protocol::SessionPresentationEventPayload::PromptActivated { run_id } => {
            state
                .snapshot
                .queued_prompts
                .retain(|prompt| prompt.run_id != run_id);
            if state
                .snapshot
                .active_run
                .as_ref()
                .is_none_or(|active| active.run_id != run_id)
            {
                state.snapshot.active_run = Some(ActiveRunView {
                    run_id: run_id.clone(),
                    turn_id: None,
                    state: "running".to_owned(),
                });
            }
            state.active_run = Some(run_id);
            sync_ui_prompt_counts(&mut state.snapshot);
        }
        agl_protocol::SessionPresentationEventPayload::PromptFinished { run_id, .. } => {
            state
                .assistant_deltas
                .retain(|_, delta| delta.run_id != run_id);
            if state.active_run.as_ref() == Some(&run_id) {
                state.active_run = None;
            }
            if state
                .snapshot
                .active_run
                .as_ref()
                .is_some_and(|active| active.run_id == run_id)
            {
                state.snapshot.active_run = None;
            }
            state
                .snapshot
                .queued_prompts
                .retain(|prompt| prompt.run_id != run_id);
            sync_ui_prompt_counts(&mut state.snapshot);
        }
        agl_protocol::SessionPresentationEventPayload::TerminalAdded { terminal }
        | agl_protocol::SessionPresentationEventPayload::TerminalChanged { terminal } => {
            update_local_human_commands(state, &terminal);
            if let Some(existing) = state
                .snapshot
                .terminals
                .iter_mut()
                .find(|existing| existing.terminal_id == terminal.terminal_id)
            {
                *existing = terminal;
            } else {
                state.snapshot.terminals.push(terminal);
            }
        }
        agl_protocol::SessionPresentationEventPayload::TerminalRemoved { terminal_id } => {
            for card in state
                .human_commands
                .iter_mut()
                .filter(|card| card.terminal_id == terminal_id)
            {
                if card.state == LocalHumanCommandState::Running {
                    card.state = LocalHumanCommandState::OutcomeUnknown;
                }
            }
            state
                .snapshot
                .terminals
                .retain(|terminal| terminal.terminal_id != terminal_id);
        }
        agl_protocol::SessionPresentationEventPayload::ExecutionStateChanged { execution } => {
            if let Some(existing) = state
                .snapshot
                .executions
                .iter_mut()
                .find(|existing| existing.execution_id == execution.execution_id)
            {
                *existing = execution;
            } else {
                state.snapshot.executions.push(execution);
            }
        }
        agl_protocol::SessionPresentationEventPayload::ActivityGraphDelta { batch } => {
            match apply_activity_graph_delta(state.snapshot.activity.as_ref(), &batch) {
                Ok(graph) => state.snapshot.activity = Some(graph),
                Err(error) => {
                    state.snapshot.activity = None;
                    resync_required = true;
                    state.notice(format!("activity graph needs a fresh snapshot: {error}"));
                }
            }
        }
        agl_protocol::SessionPresentationEventPayload::Notice { message, .. } => {
            state.notice(message)
        }
        _ => {}
    }
    PresentationApplyOutcome {
        command_catalog_changed,
        resync_required,
    }
}

pub(super) fn update_local_human_commands(state: &mut UiState, terminal: &TerminalSessionView) {
    for card in state
        .human_commands
        .iter_mut()
        .filter(|card| card.terminal_id == terminal.terminal_id)
    {
        if card.state != LocalHumanCommandState::Running {
            continue;
        }
        if terminal.command_sequence >= card.command_sequence
            && terminal.prompt_state == TerminalPromptState::Ready
        {
            card.state = LocalHumanCommandState::Completed;
        } else if terminal.process_state.is_terminal() {
            card.state = LocalHumanCommandState::OutcomeUnknown;
        }
    }
}

pub(super) fn apply_activity_graph_delta(
    current: Option<&agl_protocol::ActivityGraphView>,
    batch: &agl_protocol::ActivityGraphDeltaBatch,
) -> std::result::Result<agl_protocol::ActivityGraphView, String> {
    let current_revision = current.map_or(0, |graph| graph.graph_revision);
    if batch.graph_revision == current_revision {
        let duplicate = current.is_some_and(|graph| {
            batch.upserts.iter().all(|node| graph.nodes.contains(node))
                && batch.removals.iter().all(|removal| {
                    graph
                        .nodes
                        .iter()
                        .all(|node| node.node_id != removal.subtree_root_id)
                })
                && batch
                    .current_path
                    .as_ref()
                    .is_none_or(|path| path == &graph.current_path)
                && (!batch.truncated || graph.truncated)
        });
        return duplicate
            .then(|| {
                current
                    .expect("duplicate requires an installed graph")
                    .clone()
            })
            .ok_or_else(|| "same revision carried a different batch".to_owned());
    }
    if batch.graph_revision != current_revision.saturating_add(1) {
        return Err(format!(
            "expected revision {}, received {}",
            current_revision.saturating_add(1),
            batch.graph_revision
        ));
    }
    let mut graph = current.cloned().unwrap_or(agl_protocol::ActivityGraphView {
        graph_revision: 0,
        roots: Vec::new(),
        nodes: Vec::new(),
        current_path: Vec::new(),
        truncated: false,
    });
    for node in &batch.upserts {
        if let Some(existing) = graph
            .nodes
            .iter_mut()
            .find(|existing| existing.node_id == node.node_id)
        {
            *existing = node.clone();
        } else {
            graph.nodes.push(node.clone());
        }
    }
    if let Some(path) = &batch.current_path {
        graph.current_path = path.clone();
    }
    for removal in &batch.removals {
        let mut removed = BTreeSet::from([removal.subtree_root_id.clone()]);
        if graph
            .nodes
            .iter()
            .all(|node| node.node_id != removal.subtree_root_id)
        {
            return Err(format!(
                "removal references unknown node {}",
                removal.subtree_root_id
            ));
        }
        loop {
            let before = removed.len();
            for node in &graph.nodes {
                if node
                    .parent_node_id
                    .as_ref()
                    .is_some_and(|parent| removed.contains(parent))
                {
                    removed.insert(node.node_id.clone());
                }
            }
            if removed.len() == before {
                break;
            }
        }
        if graph.current_path.iter().any(|id| removed.contains(id)) {
            return Err("removal intersects the current activity path".to_owned());
        }
        graph.nodes.retain(|node| !removed.contains(&node.node_id));
    }
    let mut by_id = graph
        .nodes
        .drain(..)
        .map(|node| (node.node_id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut children = BTreeMap::<Option<String>, Vec<String>>::new();
    for node in by_id.values() {
        children
            .entry(node.parent_node_id.clone())
            .or_default()
            .push(node.node_id.clone());
    }
    for ids in children.values_mut() {
        ids.sort_by(|left, right| {
            let left = by_id.get(left).expect("activity child exists");
            let right = by_id.get(right).expect("activity child exists");
            (left.order_index, left.node_id.as_str())
                .cmp(&(right.order_index, right.node_id.as_str()))
        });
    }
    fn visit(
        parent: Option<String>,
        children: &BTreeMap<Option<String>, Vec<String>>,
        by_id: &mut BTreeMap<String, agl_protocol::ActivityNodeView>,
        output: &mut Vec<agl_protocol::ActivityNodeView>,
    ) {
        for id in children.get(&parent).into_iter().flatten() {
            let Some(node) = by_id.remove(id) else {
                continue;
            };
            output.push(node);
            visit(Some(id.clone()), children, by_id, output);
        }
    }
    visit(None, &children, &mut by_id, &mut graph.nodes);
    if !by_id.is_empty() {
        return Err("graph contains a cycle or disconnected nodes".to_owned());
    }
    graph.roots = graph
        .nodes
        .iter()
        .filter(|node| node.parent_node_id.is_none())
        .map(|node| node.node_id.clone())
        .collect();
    graph.graph_revision = batch.graph_revision;
    graph.truncated |= batch.truncated;
    graph.validate().map_err(|error| error.to_string())?;
    Ok(graph)
}

pub(super) fn sync_ui_prompt_counts(snapshot: &mut SessionPresentationSnapshot) {
    snapshot.header.active_run_count = u32::from(snapshot.active_run.is_some());
    snapshot.header.queued_prompt_count =
        u32::try_from(snapshot.queued_prompts.len()).unwrap_or(u32::MAX);
    snapshot.command_context.active_or_queued_turns = snapshot
        .header
        .active_run_count
        .saturating_add(snapshot.header.queued_prompt_count);
}

pub(super) fn append_assistant_delta(
    deltas: &mut BTreeMap<MessageId, AssistantDeltaState>,
    run_id: RunId,
    message_id: MessageId,
    sequence: u64,
    text: &str,
) -> AssistantDeltaApply {
    if !deltas.contains_key(&message_id) && deltas.len() >= MAX_LIVE_ASSISTANT_DELTAS {
        return AssistantDeltaApply::BoundExceeded;
    }
    let delta = deltas
        .entry(message_id)
        .or_insert_with(|| AssistantDeltaState {
            run_id: run_id.clone(),
            next_sequence: 1,
            text: String::new(),
            valid: true,
        });
    if !delta.valid {
        return AssistantDeltaApply::Duplicate;
    }
    if delta.run_id != run_id || sequence > delta.next_sequence {
        delta.valid = false;
        delta.text.clear();
        return AssistantDeltaApply::SequenceGap;
    }
    if sequence < delta.next_sequence {
        return AssistantDeltaApply::Duplicate;
    }
    if delta.text.len().saturating_add(text.len()) > MAX_LIVE_ASSISTANT_DELTA_BYTES {
        delta.valid = false;
        delta.text.clear();
        return AssistantDeltaApply::BoundExceeded;
    }
    delta.text.push_str(text);
    delta.next_sequence = delta.next_sequence.saturating_add(1);
    AssistantDeltaApply::Applied
}

pub(super) fn presentation_item_key(item: &SessionPresentationItem) -> String {
    match item {
        SessionPresentationItem::UserMessage { message_id, .. }
        | SessionPresentationItem::AssistantMessage { message_id, .. } => message_id.to_string(),
        SessionPresentationItem::IncompleteAssistant { item } => item.message_id.to_string(),
        SessionPresentationItem::AgentAction {
            run_id, step_id, ..
        } => format!("{run_id}:{step_id}"),
        SessionPresentationItem::ContextBoundary { event_id, .. }
        | SessionPresentationItem::Notice { event_id, .. } => event_id.to_string(),
    }
}
