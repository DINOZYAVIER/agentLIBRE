use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::{
    ComposerMode, InteractiveState, PickerKind, PickerPayload, header_text, palette_text,
    picker_help, transcript_model,
};

const MAX_VISIBLE_COMPOSER_LINES: usize = 6;
const TAB_WIDTH: usize = 4;

/// Complete deterministic layout model consumed by the Ratatui renderer.
/// Construction is pure: environment and runtime ownership are normalized
/// into `InteractiveState` before this boundary.
pub(super) struct RenderModel {
    pub(super) header: Rect,
    pub(super) transcript: Rect,
    pub(super) palette: Option<Rect>,
    pub(super) composer: Rect,
    pub(super) footer: Rect,
    pub(super) composer_content: ComposerRenderModel,
    pub(super) footer_text: String,
    pub(super) footer_style: Style,
    pub(super) header_text: Text<'static>,
    pub(super) transcript_text: Text<'static>,
    pub(super) transcript_scroll: u16,
    pub(super) palette_text: Option<Text<'static>>,
    pub(super) picker: Option<PickerRenderModel>,
}

pub(super) struct ComposerRenderModel {
    pub(super) title: String,
    pub(super) title_style: Style,
    pub(super) text: Text<'static>,
    pub(super) cursor: (u16, u16),
}

pub(super) struct PickerRenderModel {
    pub(super) area: Rect,
    pub(super) title: String,
    pub(super) text: Text<'static>,
    pub(super) cursor: (u16, u16),
}

pub(super) fn view(state: &InteractiveState, area: Rect) -> RenderModel {
    let palette_height = if state.composer.mode == ComposerMode::Command {
        state.matching_commands().len().min(6) as u16 + 2
    } else {
        0
    };
    let composer_lines = state
        .composer
        .buffer
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1)
        .min(MAX_VISIBLE_COMPOSER_LINES) as u16;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(palette_height),
            Constraint::Length(composer_lines.saturating_add(2)),
            Constraint::Length(1),
        ])
        .split(area);
    let continue_help = if state.latest_available_incomplete().is_some() {
        "  Ctrl+Y Continue incomplete"
    } else {
        ""
    };
    let (transcript_text, transcript_scroll) =
        transcript_model(state, layout[1].width, layout[1].height, state.no_color);
    RenderModel {
        header: layout[0],
        transcript: layout[1],
        palette: (palette_height > 0).then_some(layout[2]),
        composer: layout[3],
        footer: layout[4],
        composer_content: composer_model(state),
        footer_text: format!(
            "Enter submit  ! Shell  empty Shell Enter attaches Terminal  Shift+Enter newline{continue_help}  Ctrl+D disconnect"
        ),
        footer_style: if state.no_color {
            Style::default()
        } else {
            Style::default().fg(Color::DarkGray)
        },
        header_text: header_text(state),
        transcript_text,
        transcript_scroll,
        palette_text: (palette_height > 0).then(|| palette_text(state)),
        picker: state
            .picker
            .as_ref()
            .map(|picker| picker_model(picker, area, state.no_color)),
    }
}

fn picker_model(
    picker: &super::PickerState,
    frame_area: Rect,
    no_color: bool,
) -> PickerRenderModel {
    let color = |style| if no_color { Style::default() } else { style };
    let width = frame_area.width.saturating_sub(2).clamp(1, 110);
    let height = frame_area.height.saturating_sub(2).clamp(1, 24);
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let title = if matches!(&picker.kind, PickerKind::Skills) {
        format!(
            " {} · {} selected ",
            picker.title,
            picker.selected_values.len()
        )
    } else {
        format!(" {} ", picker.title)
    };
    let query_prefix = "filter: ";
    let mut lines = vec![Line::from(vec![
        Span::styled(query_prefix, color(Style::default().fg(Color::DarkGray))),
        Span::raw(picker.query.clone()),
    ])];
    let inner_height = area.height.saturating_sub(2) as usize;
    if let Some(confirmation) = &picker.confirmation {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            confirmation.prompt.clone(),
            color(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ));
        lines.push(Line::styled(
            "Enter confirm  Esc cancel",
            color(Style::default().fg(Color::DarkGray)),
        ));
    } else {
        let filtered = picker.filtered_indices();
        let visible_entries = inner_height.saturating_sub(2).max(1);
        let selected = picker.selected.min(filtered.len().saturating_sub(1));
        let first = selected
            .saturating_add(1)
            .saturating_sub(visible_entries)
            .min(filtered.len().saturating_sub(visible_entries));
        if filtered.is_empty() {
            lines.push(Line::styled(
                "no matching entries",
                color(Style::default().fg(Color::DarkGray)),
            ));
        } else {
            for (rank, entry_index) in filtered
                .iter()
                .enumerate()
                .skip(first)
                .take(visible_entries)
            {
                let entry = &picker.entries[*entry_index];
                let selection = match &entry.payload {
                    PickerPayload::Skill(skill_id) => {
                        if picker.selected_values.contains(skill_id) {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    }
                    _ => "",
                };
                let detail = entry
                    .detail
                    .as_deref()
                    .map(|detail| format!(" · {detail}"))
                    .unwrap_or_default();
                let style = if rank == selected {
                    if no_color {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    }
                } else {
                    Style::default()
                };
                lines.push(Line::styled(
                    format!("{selection}{}{detail}", entry.label),
                    style,
                ));
            }
        }
        lines.push(Line::styled(
            picker_help(&picker.kind),
            color(Style::default().fg(Color::DarkGray)),
        ));
    }
    let query_width = picker.query.width().min(u16::MAX as usize) as u16;
    let cursor_x = area
        .x
        .saturating_add(1)
        .saturating_add(query_prefix.len() as u16)
        .saturating_add(query_width)
        .min(area.right().saturating_sub(1).max(area.x));
    PickerRenderModel {
        area,
        title,
        text: Text::from(lines),
        cursor: (cursor_x, area.y.saturating_add(1)),
    }
}

fn composer_model(state: &InteractiveState) -> ComposerRenderModel {
    let no_color = state.no_color;
    let mode_style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        match state.composer.mode {
            ComposerMode::Prompt => Style::default().fg(Color::Cyan),
            ComposerMode::Shell => Style::default().fg(Color::Magenta),
            ComposerMode::Command => Style::default().fg(Color::Blue),
        }
    };
    let selected_style = if no_color {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    };
    let selection = state.composer.selection();
    let mut lines = vec![Line::default()];
    let mut row = 0_u16;
    let mut column = 0_usize;
    let mut cursor = (0_u16, 0_u16);
    for (index, grapheme) in state.composer.buffer.grapheme_indices(true) {
        if index == state.composer.cursor {
            cursor = (column.min(u16::MAX as usize) as u16, row);
        }
        if grapheme == "\n" {
            lines.push(Line::default());
            row = row.saturating_add(1);
            column = 0;
            continue;
        }
        let visible = if grapheme == "\t" {
            " ".repeat(TAB_WIDTH - column % TAB_WIDTH)
        } else {
            grapheme.to_owned()
        };
        let style = if selection
            .as_ref()
            .is_some_and(|range| range.start <= index && index < range.end)
        {
            selected_style
        } else {
            Style::default()
        };
        column = column.saturating_add(visible.width());
        lines
            .last_mut()
            .expect("composer always has one line")
            .spans
            .push(Span::styled(visible, style));
    }
    if state.composer.cursor == state.composer.buffer.len() {
        cursor = (column.min(u16::MAX as usize) as u16, row);
    }
    ComposerRenderModel {
        title: format!(" {} ", state.composer.label()),
        title_style: mode_style,
        text: Text::from(lines),
        cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_expands_tabs_and_tracks_cjk_multiline_cursor() {
        let mut state =
            super::super::tests::test_ui_state(agl_ids::SessionId::generate(), Vec::new());
        state.composer.insert_paste("a\t界\n👩‍💻");
        let model = view(&state, Rect::new(0, 0, 80, 20));
        assert_eq!(model.composer_content.cursor, (2, 1));
        assert_eq!(model.composer_content.text.lines.len(), 2);
        assert_eq!(model.composer.height, 4);
    }
}
