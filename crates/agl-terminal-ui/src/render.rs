use super::*;

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, state: &UiState) {
    let model = view(state, frame.area());
    frame.render_widget(Paragraph::new(model.header_text.clone()), model.header);
    frame.render_widget(
        Paragraph::new(model.transcript_text.clone())
            .wrap(Wrap { trim: false })
            .scroll((model.transcript_scroll, 0)),
        model.transcript,
    );
    if let (Some(area), Some(text)) = (model.palette, model.palette_text.as_ref()) {
        frame.render_widget(
            Paragraph::new(text.clone())
                .block(Block::default().borders(Borders::ALL).title(" Commands ")),
            area,
        );
    }
    draw_composer(frame, model.composer, &model.composer_content);
    frame.render_widget(
        Paragraph::new(model.footer_text).style(model.footer_style),
        model.footer,
    );
    if let Some(picker) = &model.picker {
        draw_picker(frame, picker);
    }
}

pub(super) fn draw_picker(frame: &mut ratatui::Frame<'_>, picker: &PickerRenderModel) {
    frame.render_widget(Clear, picker.area);
    frame.render_widget(
        Paragraph::new(picker.text.clone()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(picker.title.clone()),
        ),
        picker.area,
    );
    frame.set_cursor_position(picker.cursor);
}

pub(super) fn picker_help(kind: &PickerKind) -> &'static str {
    match kind {
        PickerKind::Skills => "Space toggle  Ctrl+A all  Ctrl+U none  Enter apply  Esc close",
        PickerKind::Processes => {
            "Enter attach/action (HOST confirms)  Ctrl+R read-only  Ctrl+W writer  Ctrl+K stop  Ctrl+Shift+K kill  Ctrl+P promote  Esc"
        }
        PickerKind::Resume | PickerKind::Model | PickerKind::Mode => {
            "Type to filter  Enter select  Esc close"
        }
    }
}

pub(super) fn header_text(state: &UiState) -> Text<'static> {
    let header = &state.snapshot.header;
    let model = header.model_id.as_deref().unwrap_or("local");
    let status = state
        .snapshot
        .activity
        .as_ref()
        .and_then(|graph| {
            graph.current_path.last().and_then(|id| {
                graph
                    .nodes
                    .iter()
                    .find(|node| &node.node_id == id)
                    .map(|node| activity_phase_label(node.phase).to_owned())
            })
        })
        .unwrap_or_else(|| {
            if state.active_run.is_some() {
                "working".to_owned()
            } else {
                "ready".to_owned()
            }
        });
    Text::from(vec![
        Line::from(vec![
            Span::styled("agentLIBRE", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                "  {status}  model:{model}  mode:{:?}",
                header.operation_mode
            )),
        ]),
        Line::from(format!(
            "{}  cwd:{}  session:{}",
            workspace_label(&header.workspace_root),
            display_path(&header.cwd),
            header.session_id
        )),
    ])
}

#[cfg(test)]
pub(super) fn draw_transcript(frame: &mut ratatui::Frame<'_>, area: Rect, state: &UiState) {
    draw_transcript_with_activity_mode(frame, area, state, state.no_color);
}

pub(super) fn transcript_model(
    state: &UiState,
    width: u16,
    height: u16,
    no_color: bool,
) -> (Text<'static>, u16) {
    let presentation_style = |style| {
        if no_color { Style::default() } else { style }
    };
    let mut lines = Vec::new();
    append_activity_lines(&mut lines, state, width, no_color);
    for item in &state.snapshot.items {
        match item {
            SessionPresentationItem::UserMessage { content, .. } => {
                lines.push(Line::styled(
                    "you",
                    presentation_style(Style::default().fg(Color::Cyan)),
                ));
                lines.extend(text_lines(content_text(content)));
            }
            SessionPresentationItem::AssistantMessage { content, .. } => {
                lines.push(Line::styled(
                    "agentLIBRE",
                    presentation_style(Style::default().fg(Color::Green)),
                ));
                lines.extend(text_lines(content_text(content)));
            }
            SessionPresentationItem::IncompleteAssistant { item } => {
                lines.push(Line::styled(
                    "agentLIBRE · incomplete · output limit",
                    presentation_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ));
                lines.extend(text_lines(content_text(&item.content)));
                let reason = match item.reason {
                    agl_protocol::IncompleteOutputReason::ModelLength => "model length limit",
                    agl_protocol::IncompleteOutputReason::ContentByteLimit => "content byte limit",
                };
                let action = match &item.continue_action {
                    agl_protocol::ContinueActionView::Available
                        if state.latest_available_incomplete().as_ref()
                            == Some(&item.message_id) =>
                    {
                        "Ctrl+Y Continue".to_owned()
                    }
                    agl_protocol::ContinueActionView::Available => {
                        "Continue available · Ctrl+Y targets the newest incomplete output"
                            .to_owned()
                    }
                    agl_protocol::ContinueActionView::Claimed {
                        continuation_run_id,
                    } => format!("Continue claimed · run {continuation_run_id}"),
                    agl_protocol::ContinueActionView::Unavailable { reason } => match reason {
                        agl_protocol::ContinueUnavailableReason::StaleContext => {
                            "Continue unavailable · context changed".to_owned()
                        }
                        agl_protocol::ContinueUnavailableReason::PolicyDenied => {
                            "Continue unavailable · policy denied".to_owned()
                        }
                        agl_protocol::ContinueUnavailableReason::SessionFinished => {
                            "Continue unavailable · session finished".to_owned()
                        }
                    },
                };
                lines.push(Line::styled(
                    format!("{reason} · {action}"),
                    presentation_style(Style::default().fg(Color::Yellow)),
                ));
            }
            SessionPresentationItem::ContextBoundary { .. } => lines.push(Line::styled(
                "──────── context cleared ────────",
                presentation_style(Style::default().fg(Color::DarkGray)),
            )),
            SessionPresentationItem::Notice { message, .. } => lines.push(Line::styled(
                message.clone(),
                presentation_style(Style::default().fg(Color::Yellow)),
            )),
            SessionPresentationItem::AgentAction { summary, state, .. } => {
                lines.push(Line::styled(
                    format!("agent action · {summary} · {state:?}"),
                    presentation_style(Style::default().fg(Color::Magenta)),
                ))
            }
        }
        lines.push(Line::raw(""));
    }
    for card in &state.human_commands {
        let (status, style) = match card.state {
            LocalHumanCommandState::Running => ("running", Style::default().fg(Color::Cyan)),
            LocalHumanCommandState::Completed => ("completed", Style::default().fg(Color::Green)),
            LocalHumanCommandState::OutcomeUnknown => {
                ("outcome unknown", Style::default().fg(Color::Yellow))
            }
        };
        lines.push(Line::styled(
            format!("! #{} · {status}", card.command_sequence),
            presentation_style(style.add_modifier(Modifier::BOLD)),
        ));
        lines.extend(text_lines(format!("$ {}", card.command)));
        lines.push(Line::styled(
            "private UI-local command · empty Shell Enter to Attach",
            presentation_style(Style::default().fg(Color::DarkGray)),
        ));
        lines.push(Line::raw(""));
    }
    for terminal in &state.snapshot.terminals {
        let authority = terminal_authority_label(terminal.profile);
        lines.push(Line::styled(
            format!(
                "! {} · {authority} · cwd:{} · {:?} · {:?}",
                terminal_owner_label(&terminal.owner),
                display_path(&terminal.cwd),
                terminal.prompt_state,
                terminal.process_state,
            ),
            presentation_style(if terminal.profile == ExecutionProfile::Host {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            }),
        ));
    }
    for delta in state.assistant_deltas.values().filter(|delta| delta.valid) {
        lines.push(Line::styled(
            "agentLIBRE · streaming",
            presentation_style(Style::default().fg(Color::Green)),
        ));
        lines.extend(text_lines(delta.text.clone()));
        lines.push(Line::raw(""));
    }
    for notice in &state.notices {
        lines.push(Line::styled(
            format!("· {notice}"),
            presentation_style(Style::default().fg(Color::Yellow)),
        ));
    }
    let text = Text::from(lines);
    let paragraph = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
    let scroll = paragraph
        .line_count(width)
        .saturating_sub(height as usize)
        .min(u16::MAX as usize) as u16;
    (text, scroll)
}

#[cfg(test)]
pub(super) fn draw_transcript_with_activity_mode(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &UiState,
    no_color: bool,
) {
    let (text, scroll) = transcript_model(state, area.width, area.height, no_color);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

pub(super) fn append_activity_lines(
    lines: &mut Vec<Line<'static>>,
    state: &UiState,
    width: u16,
    no_color: bool,
) {
    let Some(graph) = &state.snapshot.activity else {
        return;
    };
    let ascii = width < 50 || no_color;
    let arrow = if ascii { " -> " } else { " → " };
    let current = graph
        .current_path
        .iter()
        .filter_map(|id| graph.nodes.iter().find(|node| &node.node_id == id))
        .map(activity_node_label)
        .collect::<Vec<_>>();
    if !current.is_empty() {
        lines.push(Line::styled(
            format!("activity · {}", current.join(arrow)),
            if no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD)
            },
        ));
    }
    if state.activity_expanded {
        for node in &graph.nodes {
            let prefix = activity_tree_prefix(graph, node, ascii, 8);
            let marker = match node.state {
                agl_protocol::ActivityNodeState::Pending => "○",
                agl_protocol::ActivityNodeState::Waiting => "○",
                agl_protocol::ActivityNodeState::Running => "▶",
                agl_protocol::ActivityNodeState::Succeeded => "✓",
                agl_protocol::ActivityNodeState::Failed => "!",
                agl_protocol::ActivityNodeState::Cancelled => "×",
                agl_protocol::ActivityNodeState::Incomplete => "…",
                agl_protocol::ActivityNodeState::Truncated => "…",
            };
            let marker = if ascii {
                match node.state {
                    agl_protocol::ActivityNodeState::Pending => "o",
                    agl_protocol::ActivityNodeState::Waiting => "o",
                    agl_protocol::ActivityNodeState::Running => ">",
                    agl_protocol::ActivityNodeState::Succeeded => "+",
                    agl_protocol::ActivityNodeState::Failed => "!",
                    agl_protocol::ActivityNodeState::Cancelled => "x",
                    agl_protocol::ActivityNodeState::Incomplete => "...",
                    agl_protocol::ActivityNodeState::Truncated => "...",
                }
            } else {
                marker
            };
            lines.push(Line::styled(
                format!("{prefix}{marker} {}", activity_node_label(node)),
                if no_color {
                    Style::default()
                } else if matches!(
                    node.state,
                    agl_protocol::ActivityNodeState::Failed
                        | agl_protocol::ActivityNodeState::Incomplete
                        | agl_protocol::ActivityNodeState::Truncated
                ) {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ));
        }
        lines.push(Line::styled(
            "Ctrl+G collapse activity",
            if no_color {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    } else if !graph.nodes.is_empty() {
        for node in graph.nodes.iter().filter(|node| {
            matches!(
                node.state,
                agl_protocol::ActivityNodeState::Waiting
                    | agl_protocol::ActivityNodeState::Failed
                    | agl_protocol::ActivityNodeState::Incomplete
                    | agl_protocol::ActivityNodeState::Truncated
            ) && !graph.current_path.iter().any(|id| id == &node.node_id)
        }) {
            let marker = if ascii { "!" } else { "↳" };
            lines.push(Line::styled(
                format!("{marker} {}", activity_node_label(node)),
                if no_color {
                    Style::default()
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ));
        }
        lines.push(Line::styled(
            if graph.truncated {
                "Ctrl+G expand activity graph · retained history truncated"
            } else {
                "Ctrl+G expand activity graph"
            },
            if no_color {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    lines.push(Line::raw(""));
}

pub(super) fn activity_node_label(node: &agl_protocol::ActivityNodeView) -> String {
    let mut label = format!("{} · {}", activity_phase_label(node.phase), node.summary);
    use agl_protocol::{ActivityDetailView as Detail, ToolActivityDetail as Tool};
    match &node.detail {
        Detail::None => {}
        Detail::Tool(Tool::FilesystemList {
            path,
            entries,
            completeness,
        }) => label.push_str(&format!(
            " · {} · {entries} entries · {}",
            display_path(path),
            format!("{completeness:?}").to_ascii_lowercase()
        )),
        Detail::Tool(Tool::FilesystemRead { path, bytes }) => {
            label.push_str(&format!(" · {} · {bytes} bytes", display_path(path)));
        }
        Detail::Tool(Tool::RepositorySearch {
            scope,
            matches,
            complete,
        }) => label.push_str(&format!(
            " · {} · {matches} matches · {}",
            display_path(scope),
            if *complete { "complete" } else { "partial" }
        )),
        Detail::Tool(Tool::ProcessExecution {
            profile,
            exit_status,
        }) => label.push_str(&format!(" · {profile:?} · exit {exit_status:?}")),
        Detail::Tool(Tool::PolicyCheck { tool_id, outcome }) => label.push_str(&format!(
            " · {tool_id} · {}",
            format!("{outcome:?}").to_ascii_lowercase()
        )),
        Detail::Inference(detail) => {
            label.push_str(&format!(
                " · {}",
                format!("{:?}", detail.stage).to_ascii_lowercase()
            ));
            if let Some(completed) = detail.completed {
                let unit = match detail.unit {
                    Some(agl_protocol::InferenceProgressUnit::Tokens) => "tokens",
                    Some(agl_protocol::InferenceProgressUnit::Chunks) => "chunks",
                    None => "units",
                };
                label.push_str(&format!(
                    " · {completed}/{} {unit}",
                    detail.total.unwrap_or(completed),
                ));
            }
            if detail.cache != agl_protocol::ActivityCacheDisposition::NotApplicable {
                label.push_str(&format!(
                    " · {}",
                    format!("{:?}", detail.cache).to_ascii_lowercase()
                ));
            }
        }
        Detail::Aggregate(detail) => label.push_str(&format!(
            " · {} collapsed · {} failed · {} incomplete",
            detail.collapsed_nodes, detail.failed, detail.incomplete
        )),
        Detail::UnknownTool { tool_id } => {
            label.push_str(&format!(" · {tool_id}"));
        }
    }
    if node.elapsed_ms > 0 {
        label.push_str(&format!(" · {:.1}s", node.elapsed_ms as f64 / 1000.0));
    }
    label
}

pub(super) fn activity_tree_prefix(
    graph: &agl_protocol::ActivityGraphView,
    node: &agl_protocol::ActivityNodeView,
    ascii: bool,
    maximum_depth: usize,
) -> String {
    let mut ancestors = Vec::new();
    let mut parent = node.parent_node_id.as_deref();
    while let Some(parent_id) = parent {
        let Some(parent_node) = graph
            .nodes
            .iter()
            .find(|candidate| candidate.node_id == parent_id)
        else {
            break;
        };
        ancestors.push(parent_node);
        parent = parent_node.parent_node_id.as_deref();
    }
    ancestors.reverse();

    let omitted = ancestors.len().saturating_sub(maximum_depth);
    let mut prefix = if omitted > 0 {
        if ascii { ".. " } else { "… " }.to_owned()
    } else {
        String::new()
    };
    for ancestor in ancestors.into_iter().skip(omitted) {
        let connector = if activity_node_is_last_sibling(graph, ancestor) {
            "  "
        } else if ascii {
            "| "
        } else {
            "│ "
        };
        prefix.push_str(connector);
    }
    prefix.push_str(if activity_node_is_last_sibling(graph, node) {
        if ascii { "`- " } else { "└─ " }
    } else if ascii {
        "+- "
    } else {
        "├─ "
    });
    prefix
}

pub(super) fn activity_node_is_last_sibling(
    graph: &agl_protocol::ActivityGraphView,
    node: &agl_protocol::ActivityNodeView,
) -> bool {
    graph
        .nodes
        .iter()
        .rev()
        .find(|candidate| candidate.parent_node_id == node.parent_node_id)
        .is_some_and(|candidate| candidate.node_id == node.node_id)
}

pub(super) fn activity_phase_label(phase: agl_protocol::ActivityPhase) -> &'static str {
    match phase {
        agl_protocol::ActivityPhase::Queued => "queued",
        agl_protocol::ActivityPhase::Policy => "policy",
        agl_protocol::ActivityPhase::Model => "model",
        agl_protocol::ActivityPhase::Tool => "tool",
        agl_protocol::ActivityPhase::ChildRun => "child run",
        agl_protocol::ActivityPhase::InferenceQueue => "inference queue",
        agl_protocol::ActivityPhase::InferenceAdmission => "inference admission",
        agl_protocol::ActivityPhase::ModelLoad => "model load",
        agl_protocol::ActivityPhase::Context => "context",
        agl_protocol::ActivityPhase::Prefill => "prefill",
        agl_protocol::ActivityPhase::Generation => "generation",
        agl_protocol::ActivityPhase::OutputParsing => "output parsing",
        agl_protocol::ActivityPhase::Terminal => "terminal",
        agl_protocol::ActivityPhase::Retention => "retention",
    }
}

pub(super) fn palette_text(state: &UiState) -> Text<'static> {
    let commands = state.matching_commands();
    let selected = state
        .composer
        .selected_command
        .min(commands.len().saturating_sub(1));
    let lines = commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let availability = match &command.availability {
                CommandAvailability::Enabled => "",
                CommandAvailability::Disabled { message, .. } => message,
                CommandAvailability::Hidden => "hidden",
            };
            let style = if index == selected {
                if state.no_color {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                }
            } else if matches!(command.availability, CommandAvailability::Disabled { .. }) {
                if state.no_color {
                    Style::default()
                } else {
                    Style::default().fg(Color::DarkGray)
                }
            } else {
                Style::default()
            };
            Line::styled(
                format!(
                    "/{:<12} {:<38} {}",
                    command.name, command.summary, availability
                ),
                style,
            )
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

pub(super) fn draw_composer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    model: &ComposerRenderModel,
) {
    let paragraph = Paragraph::new(model.text.clone()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(model.title.clone(), model.title_style)),
    );
    frame.render_widget(paragraph, area);
    let cursor_x = area.x.saturating_add(1).saturating_add(model.cursor.0);
    let cursor_y = area.y.saturating_add(1).saturating_add(model.cursor.1);
    frame.set_cursor_position((
        cursor_x.min(area.right().saturating_sub(1)),
        cursor_y.min(area.bottom().saturating_sub(1)),
    ));
}

pub(super) fn text_lines(text: String) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| Line::raw(line.to_owned()))
        .collect()
}

pub(super) fn content_text(content: &agl_content::Content) -> String {
    content
        .clone()
        .text_only()
        .unwrap_or_else(|| "[multimodal content]".to_owned())
}

pub(super) fn display_path(path: &agl_protocol::SanitizedDisplayPath) -> String {
    if path.truncated {
        format!("{}…", path.text)
    } else {
        path.text.clone()
    }
}

pub(super) fn workspace_label(workspace: &agl_protocol::SanitizedDisplayPath) -> String {
    let label = workspace
        .text
        .rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(&workspace.text);
    if workspace.truncated {
        format!("{label}…")
    } else {
        label.to_owned()
    }
}

pub(super) fn managed_shell_profile_id(program: &Path) -> Option<&'static str> {
    match program.file_name()?.to_str()? {
        "bash" => Some("bash-managed"),
        "zsh" => Some("zsh-managed"),
        _ => None,
    }
}

pub(super) fn terminal_owner_label(owner: &TerminalOwnerView) -> String {
    match owner {
        TerminalOwnerView::Human { .. } => "Human".to_owned(),
        TerminalOwnerView::MainAgent { .. } => "main agent".to_owned(),
        TerminalOwnerView::Subagent { owner_run_id, .. } => format!("subagent {owner_run_id}"),
        TerminalOwnerView::SessionPromoted {
            previous_owner_run_id,
            ..
        } => format!("promoted {previous_owner_run_id}"),
    }
}

pub(super) fn terminal_authority_label(profile: ExecutionProfile) -> &'static str {
    match profile {
        ExecutionProfile::Workspace => "workspace",
        ExecutionProfile::Host => "HOST",
    }
}
