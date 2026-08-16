use super::*;

#[test]
fn unicode_editing_moves_and_deletes_whole_graphemes() {
    let mut composer = Composer::default();
    composer.insert_text("a👩‍💻б");
    composer.move_left(false);
    composer.backspace();
    assert_eq!(composer.buffer, "aб");
    assert_eq!(composer.cursor, 1);
}

#[test]
fn composer_render_goldens_cover_wide_narrow_no_color_and_selection() {
    let render = |width: u16, no_color: bool| {
        let mut state = test_ui_state(SessionId::generate(), Vec::new());
        state.no_color = no_color;
        state.composer.insert_paste("alpha\t界\n👩‍💻 beta");
        state.composer.move_word_left(false);
        state.composer.move_word_right(true);
        let backend = ratatui::backend::TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let has_color = buffer
            .content
            .iter()
            .any(|cell| cell.fg != Color::Reset || cell.bg != Color::Reset);
        let has_selection = buffer
            .content
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::REVERSED) || cell.bg == Color::Cyan);
        (rows, has_color, has_selection)
    };

    let (wide, wide_color, wide_selection) = render(80, false);
    assert!(wide_color);
    assert!(wide_selection);
    assert!(wide.iter().any(|row| row.contains("Prompt >")));
    assert!(wide.iter().any(|row| row.contains("alpha   界")));
    assert!(wide.iter().any(|row| row.contains("👩‍💻")));
    assert!(wide.iter().any(|row| row.contains("beta")));

    let (narrow, _, narrow_selection) = render(28, false);
    assert!(narrow_selection);
    assert_eq!(narrow.iter().map(String::len).min(), Some(28));
    assert!(narrow.iter().any(|row| row.contains("Prompt >")));

    let (no_color, has_color, no_color_selection) = render(80, true);
    assert!(!has_color);
    assert!(no_color_selection);
    assert!(no_color.iter().any(|row| row.contains("alpha   界")));
}
