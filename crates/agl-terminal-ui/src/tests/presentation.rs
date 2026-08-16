use super::*;

#[test]
fn failed_run_notice_prefers_and_renders_the_detailed_message() {
    let message = "inference resource admission failed (accelerator_capacity_exceeded): \
            inference needs 23347593216 bytes with 0 already reserved, but only 23093305344 \
            bytes are available under 2659721216 bytes of device pressure";
    let finished = RunSubscriptionFinishedEvent {
        run_id: RunId::generate(),
        state: ProtocolRunState::Failed,
        last_sequence: 4,
        terminal_result: None,
        error_code: Some("accelerator_capacity_exceeded".to_owned()),
        error_message: Some(message.to_owned()),
    };

    assert_eq!(
        run_finished_notice(&finished),
        Some(format!("turn failed: {message}"))
    );

    let mut state = test_ui_state(SessionId::generate(), Vec::new());
    state.notice(run_finished_notice(&finished).unwrap());
    let backend = ratatui::backend::TestBackend::new(160, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_transcript(frame, frame.area(), &state))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("turn failed: inference resource admission failed"));
    assert!(rendered.contains("23093305344"));
}

#[test]
fn failed_run_notice_falls_back_to_code_then_state() {
    let mut finished = RunSubscriptionFinishedEvent {
        run_id: RunId::generate(),
        state: ProtocolRunState::Failed,
        last_sequence: 0,
        terminal_result: None,
        error_code: Some("worker_lost".to_owned()),
        error_message: Some("  ".to_owned()),
    };

    assert_eq!(
        run_finished_notice(&finished),
        Some("turn failed (worker_lost)".to_owned())
    );
    finished.error_code = None;
    assert_eq!(
        run_finished_notice(&finished),
        Some("turn failed".to_owned())
    );
}

#[test]
fn failed_run_notice_sanitizes_and_bounds_untrusted_protocol_text() {
    let hostile = format!(
        "osc=\u{1b}]52;c;secret\u{7}\ncolor=\u{1b}[31m bidi=\u{202e} {}",
        "x".repeat(MAX_RUN_FINISHED_NOTICE_BYTES * 2)
    );
    let mut finished = RunSubscriptionFinishedEvent {
        run_id: RunId::generate(),
        state: ProtocolRunState::Failed,
        last_sequence: 4,
        terminal_result: None,
        error_code: Some(hostile.clone()),
        error_message: Some(hostile),
    };

    for use_message in [true, false] {
        if !use_message {
            finished.error_message = Some("  ".to_owned());
        }
        let notice = run_finished_notice(&finished).unwrap();
        assert!(notice.len() <= MAX_RUN_FINISHED_NOTICE_BYTES);
        assert!(notice.contains("\\u{1B}]52;c;secret\\u{7}\\u{A}"));
        assert!(notice.contains("\\u{202E}"));
        assert!(!notice.chars().any(|character| {
            character.is_control() || is_unicode_format_control(character as u32)
        }));
        assert!(notice.contains('…'));
    }
}

#[test]
fn succeeded_run_has_no_failure_notice() {
    let finished = RunSubscriptionFinishedEvent {
        run_id: RunId::generate(),
        state: ProtocolRunState::Succeeded,
        last_sequence: 1,
        terminal_result: None,
        error_code: Some("ignored".to_owned()),
        error_message: Some("ignored".to_owned()),
    };

    assert_eq!(run_finished_notice(&finished), None);
}

#[test]
fn assistant_deltas_are_ordered_bounded_and_retired_on_gaps() {
    let run_id = RunId::generate();
    let message_id = MessageId::generate();
    let mut deltas = BTreeMap::new();
    assert_eq!(
        append_assistant_delta(&mut deltas, run_id.clone(), message_id.clone(), 1, "hel",),
        AssistantDeltaApply::Applied
    );
    assert_eq!(
        append_assistant_delta(
            &mut deltas,
            run_id.clone(),
            message_id.clone(),
            1,
            "duplicate",
        ),
        AssistantDeltaApply::Duplicate
    );
    assert_eq!(deltas[&message_id].text, "hel");
    assert_eq!(
        append_assistant_delta(&mut deltas, run_id, message_id.clone(), 3, "lo"),
        AssistantDeltaApply::SequenceGap
    );
    assert!(!deltas[&message_id].valid);
    assert!(deltas[&message_id].text.is_empty());
}

#[test]
fn presentation_delta_gaps_require_fresh_snapshot_without_losing_pending_shell() {
    let session_id = SessionId::generate();
    let mut state = test_ui_state(session_id, Vec::new());
    let pending = PendingShellSubmission {
        command: "printf safe".to_owned(),
        client_submission_id: "stable-shell-id".to_owned(),
        terminal_ensure_submission_id: "stable-terminal-id".to_owned(),
        in_flight: true,
        outcome_uncertain: false,
    };
    state.pending_shell_submission = Some(pending.clone());
    let run_id = RunId::generate();
    let turn_id = agl_ids::TurnId::generate();
    let message_id = MessageId::generate();
    let first = apply_presentation_event(
        &mut state,
        agl_protocol::SessionPresentationEventPayload::AssistantTextDelta {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            provisional_message_id: message_id.clone(),
            sequence: 1,
            text: "one".to_owned(),
        },
    );
    assert!(!first.resync_required);
    let gap = apply_presentation_event(
        &mut state,
        agl_protocol::SessionPresentationEventPayload::AssistantTextDelta {
            run_id,
            turn_id,
            provisional_message_id: message_id,
            sequence: 3,
            text: "three".to_owned(),
        },
    );
    assert!(gap.resync_required);

    let activity_gap = apply_presentation_event(
        &mut state,
        agl_protocol::SessionPresentationEventPayload::ActivityGraphDelta {
            batch: agl_protocol::ActivityGraphDeltaBatch {
                graph_revision: 3,
                upserts: Vec::new(),
                removals: Vec::new(),
                current_path: None,
                truncated: false,
            },
        },
    );
    assert!(activity_gap.resync_required);

    let mut fresh = state.snapshot.clone();
    fresh.cursor.revision = fresh.cursor.revision.saturating_add(1);
    install_presentation_snapshot(&mut state, fresh);
    assert_eq!(state.pending_shell_submission, Some(pending));
}

#[test]
fn incomplete_output_is_visible_without_color_and_targets_the_newest_available_item() {
    let session_id = SessionId::generate();
    let mut state = test_ui_state(session_id, Vec::new());
    let older_message_id = MessageId::generate();
    let newest_message_id = MessageId::generate();
    let continuation_run_id = RunId::generate();
    let incomplete_item =
        |message_id: MessageId,
         content: &str,
         continue_action: agl_protocol::ContinueActionView| {
            SessionPresentationItem::IncompleteAssistant {
                item: agl_protocol::IncompleteAssistantItemView {
                    message_id,
                    content: agl_content::Content::text(content).unwrap(),
                    source_run_id: RunId::generate(),
                    source_turn_id: agl_ids::TurnId::generate(),
                    source_attempt_id: agl_ids::AttemptId::generate(),
                    reason: agl_protocol::IncompleteOutputReason::ContentByteLimit,
                    continuation_index: 0,
                    continue_action,
                },
            }
        };
    state.snapshot.items = vec![
        incomplete_item(
            older_message_id,
            "older partial",
            agl_protocol::ContinueActionView::Claimed {
                continuation_run_id,
            },
        ),
        incomplete_item(
            newest_message_id.clone(),
            "newest partial survives",
            agl_protocol::ContinueActionView::Available,
        ),
    ];

    assert_eq!(state.latest_available_incomplete(), Some(newest_message_id));
    let backend = ratatui::backend::TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_transcript(frame, area, &state);
        })
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("agentLIBRE · incomplete · output limit"));
    assert!(rendered.contains("newest partial survives"));
    assert!(rendered.contains("content byte limit · Ctrl+Y Continue"));
}

#[allow(clippy::too_many_arguments)]
fn test_activity_node(
    run_id: &RunId,
    node_id: String,
    parent_node_id: Option<String>,
    order_index: u64,
    kind: agl_protocol::ActivityNodeKind,
    phase: agl_protocol::ActivityPhase,
    state: agl_protocol::ActivityNodeState,
    summary: &str,
    detail: agl_protocol::ActivityDetailView,
) -> agl_protocol::ActivityNodeView {
    let terminal = state.is_terminal();
    agl_protocol::ActivityNodeView {
        node_id,
        parent_node_id,
        order_index,
        run_id: run_id.clone(),
        turn_id: None,
        attempt_id: None,
        step_id: None,
        kind,
        phase,
        state,
        retry: 0,
        started_at_unix_ms: 1,
        updated_at_unix_ms: 5,
        finished_at_unix_ms: terminal.then_some(5),
        elapsed_ms: if terminal { 4 } else { 0 },
        summary: summary.to_owned(),
        detail,
    }
}

#[test]
fn activity_delta_is_atomic_revisioned_idempotent_and_parent_ordered() {
    let run_id = RunId::generate();
    let root_id = format!("run:{run_id}");
    let step_id = "step:safe".to_owned();
    let root = test_activity_node(
        &run_id,
        root_id.clone(),
        None,
        1,
        agl_protocol::ActivityNodeKind::Run,
        agl_protocol::ActivityPhase::Model,
        agl_protocol::ActivityNodeState::Running,
        "run",
        agl_protocol::ActivityDetailView::None,
    );
    let step = test_activity_node(
        &run_id,
        step_id.clone(),
        Some(root_id.clone()),
        2,
        agl_protocol::ActivityNodeKind::Step,
        agl_protocol::ActivityPhase::Tool,
        agl_protocol::ActivityNodeState::Waiting,
        "core.workspace:fs.list",
        agl_protocol::ActivityDetailView::UnknownTool {
            tool_id: "core.workspace:fs.list".to_owned(),
        },
    );
    let first = agl_protocol::ActivityGraphDeltaBatch {
        graph_revision: 1,
        upserts: vec![root, step],
        removals: Vec::new(),
        current_path: Some(vec![root_id.clone(), step_id.clone()]),
        truncated: false,
    };
    let graph = apply_activity_graph_delta(None, &first).unwrap();
    assert_eq!(graph.graph_revision, 1);
    assert_eq!(graph.roots, std::slice::from_ref(&root_id));
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        [root_id.as_str(), step_id.as_str()]
    );

    let duplicate = apply_activity_graph_delta(Some(&graph), &first).unwrap();
    assert_eq!(duplicate, graph);
    let mut conflicting = first.clone();
    conflicting.upserts[1].summary = "different".to_owned();
    assert!(apply_activity_graph_delta(Some(&graph), &conflicting).is_err());
    let mut gap = first;
    gap.graph_revision = 3;
    assert!(apply_activity_graph_delta(Some(&graph), &gap).is_err());
}

#[test]
fn activity_render_has_compact_and_expanded_unicode_ascii_fallbacks() {
    let session_id = SessionId::generate();
    let mut state = test_ui_state(session_id, Vec::new());
    let run_id = RunId::generate();
    let root_id = format!("run:{run_id}");
    let failed_id = "step:failed".to_owned();
    let root = test_activity_node(
        &run_id,
        root_id.clone(),
        None,
        1,
        agl_protocol::ActivityNodeKind::Run,
        agl_protocol::ActivityPhase::Model,
        agl_protocol::ActivityNodeState::Running,
        "turn",
        agl_protocol::ActivityDetailView::None,
    );
    let failed = test_activity_node(
        &run_id,
        failed_id.clone(),
        Some(root_id.clone()),
        2,
        agl_protocol::ActivityNodeKind::Step,
        agl_protocol::ActivityPhase::Tool,
        agl_protocol::ActivityNodeState::Failed,
        "repository search",
        agl_protocol::ActivityDetailView::Tool(
            agl_protocol::ToolActivityDetail::RepositorySearch {
                scope: test_display_path("crates/agl-app"),
                matches: 7,
                complete: false,
            },
        ),
    );
    state.snapshot.activity = Some(agl_protocol::ActivityGraphView {
        graph_revision: 9,
        roots: vec![root_id.clone()],
        nodes: vec![root, failed],
        current_path: vec![root_id],
        truncated: true,
    });
    state
        .notices
        .push("NO_COLOR must cover the complete Chat surface".to_owned());

    let render = |state: &UiState, width: u16, no_color: bool| {
        let backend = ratatui::backend::TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_transcript_with_activity_mode(frame, area, state, no_color);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let has_color = buffer.content.iter().any(|cell| cell.fg != Color::Reset);
        (rendered, has_color)
    };

    let (compact, _) = render(&state, 120, false);
    assert!(compact.contains("activity · model · turn"));
    assert!(compact.contains("repository search"));
    assert!(compact.contains("retained history truncated"));
    assert!(!compact.contains("├─"));

    state.activity_expanded = true;
    let (unicode, _) = render(&state, 120, false);
    assert!(unicode.contains("├─") || unicode.contains("└─"));
    assert!(unicode.contains("crates/agl-app · 7 matches · partial"));
    let (narrow, _) = render(&state, 42, false);
    let (no_color, has_no_color_style) = render(&state, 120, true);
    assert!(!has_no_color_style);
    for fallback in [&narrow, &no_color] {
        assert!(fallback.contains("+- ") || fallback.contains("`- "));
        for forbidden in [" → ", "├─", "└─", "\u{1b}_G", "\u{1b}Pq"] {
            assert!(!fallback.contains(forbidden));
        }
    }
    for sentinel in ["raw prompt", "super-secret-token", "/home/private"] {
        assert!(!unicode.contains(sentinel));
    }
}

#[test]
fn activity_tree_connectors_follow_siblings_instead_of_global_node_order() {
    let first_run = RunId::generate();
    let second_run = RunId::generate();
    let first_root_id = format!("run:{first_run}");
    let first_child_id = "step:first-child".to_owned();
    let grandchild_id = "step:grandchild".to_owned();
    let second_child_id = "step:second-child".to_owned();
    let second_root_id = format!("run:{second_run}");
    let nodes = vec![
        test_activity_node(
            &first_run,
            first_root_id.clone(),
            None,
            1,
            agl_protocol::ActivityNodeKind::Run,
            agl_protocol::ActivityPhase::Model,
            agl_protocol::ActivityNodeState::Running,
            "first root",
            agl_protocol::ActivityDetailView::None,
        ),
        test_activity_node(
            &first_run,
            first_child_id.clone(),
            Some(first_root_id.clone()),
            2,
            agl_protocol::ActivityNodeKind::Step,
            agl_protocol::ActivityPhase::Tool,
            agl_protocol::ActivityNodeState::Running,
            "first child",
            agl_protocol::ActivityDetailView::None,
        ),
        test_activity_node(
            &first_run,
            grandchild_id.clone(),
            Some(first_child_id.clone()),
            3,
            agl_protocol::ActivityNodeKind::Step,
            agl_protocol::ActivityPhase::Tool,
            agl_protocol::ActivityNodeState::Running,
            "grandchild",
            agl_protocol::ActivityDetailView::None,
        ),
        test_activity_node(
            &first_run,
            second_child_id.clone(),
            Some(first_root_id.clone()),
            4,
            agl_protocol::ActivityNodeKind::Step,
            agl_protocol::ActivityPhase::Tool,
            agl_protocol::ActivityNodeState::Waiting,
            "second child",
            agl_protocol::ActivityDetailView::None,
        ),
        test_activity_node(
            &second_run,
            second_root_id.clone(),
            None,
            5,
            agl_protocol::ActivityNodeKind::Run,
            agl_protocol::ActivityPhase::Queued,
            agl_protocol::ActivityNodeState::Pending,
            "second root",
            agl_protocol::ActivityDetailView::None,
        ),
    ];
    let graph = agl_protocol::ActivityGraphView {
        graph_revision: 1,
        roots: vec![first_root_id, second_root_id],
        nodes,
        current_path: Vec::new(),
        truncated: false,
    };

    assert_eq!(
        activity_tree_prefix(&graph, &graph.nodes[0], false, 8),
        "├─ "
    );
    assert_eq!(
        activity_tree_prefix(&graph, &graph.nodes[1], false, 8),
        "│ ├─ "
    );
    assert_eq!(
        activity_tree_prefix(&graph, &graph.nodes[2], false, 8),
        "│ │ └─ "
    );
    assert_eq!(
        activity_tree_prefix(&graph, &graph.nodes[3], false, 8),
        "│ └─ "
    );
    assert_eq!(
        activity_tree_prefix(&graph, &graph.nodes[4], false, 8),
        "└─ "
    );
    assert_eq!(
        activity_tree_prefix(&graph, &graph.nodes[2], true, 8),
        "| | `- "
    );
}
