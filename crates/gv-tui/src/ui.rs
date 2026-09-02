//! Ratatui rendering for the four-pane shell (M10.02, #457 — phase 2a).
//!
//! Drawing is a pure projection of [`App`]. Every event-loop turn draws;
//! Ratatui's terminal diff suppresses unchanged writes, keeping invalidation
//! logic out of the state model. The palette is intentionally limited to
//! ANSI names: cyan marks the focused border, red marks an error status, and
//! selection uses the terminal's own reversed modifier.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Pane, Tone};
use crate::layout;

/// Draw one complete frame from the current application state.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let Some(panes) = layout::split(area) else {
        frame.render_widget(
            Paragraph::new(format!(
                "gv-tui needs at least {}x{}; this terminal is {}x{}",
                layout::MIN_WIDTH,
                layout::MIN_HEIGHT,
                area.width,
                area.height
            ))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    };

    let rows: Vec<ListItem<'_>> = app
        .catalog
        .iter()
        .map(|repository| ListItem::new(App::catalog_row(repository)))
        .collect();
    let repositories = List::new(rows)
        .block(pane_block(Pane::Repositories, app.focus))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let selected = (app.rows(Pane::Repositories) > 0).then(|| app.cursor(Pane::Repositories));
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(repositories, panes.of(Pane::Repositories), &mut state);

    draw_placeholder(
        frame,
        panes.of(Pane::Branches),
        Pane::Branches,
        app.focus,
        "#457 draws refs here",
    );
    draw_placeholder(
        frame,
        panes.of(Pane::Commits),
        Pane::Commits,
        app.focus,
        "#457 draws the graph here",
    );
    draw_placeholder(
        frame,
        panes.of(Pane::Main),
        Pane::Main,
        app.focus,
        "#458 detail and diff · #459 working tree",
    );

    let status_style = match app.status.tone {
        Tone::Info => Style::default(),
        Tone::Error => Style::default().fg(Color::Red),
    };
    frame.render_widget(
        Paragraph::new(app.status.text.as_str()).style(status_style),
        panes.status,
    );
}

fn pane_block(pane: Pane, focus: Pane) -> Block<'static> {
    let title_style = if pane == focus {
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Reset)
    };
    let block = Block::bordered().title(Line::styled(
        format!(" {} {} ", pane.number(), pane.title()),
        title_style,
    ));
    if pane == focus {
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block
    }
}

fn draw_placeholder(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    pane: Pane,
    focus: Pane,
    message: &'static str,
) {
    frame.render_widget(Paragraph::new(message).block(pane_block(pane, focus)), area);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use git_vista_protocol::RepositoryDescriptor;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::Terminal;

    use super::*;
    use crate::app::{Action, App, Data, Pane};
    use crate::layout;

    const THREE: &str = r#"[
      {"repository":"r1","worktree":"w1","name":"alpha","kind":"main_worktree","read_only":false},
      {"repository":"r2","worktree":"w2","name":"beta","kind":"bare","read_only":true},
      {"repository":"r3","worktree":"w3","name":"gamma","kind":"linked_worktree","read_only":false}
    ]"#;

    fn loaded() -> App {
        let mut app = App::new();
        assert_eq!(app.start(), [crate::app::Fetch::Catalog]);
        let catalog: Vec<RepositoryDescriptor> =
            serde_json::from_str(THREE).expect("the wire literal is valid");
        app.receive(Data::Catalog(Ok(catalog)));
        app
    }

    fn rendered(width: u16, height: u16, app: &App) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("drawing to TestBackend succeeds");
        terminal
    }

    fn text(buffer: &Buffer) -> String {
        let mut out = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                out.push_str(buffer.cell((x, y)).expect("cell inside buffer").symbol());
            }
            out.push('\n');
        }
        out
    }

    fn line(buffer: &Buffer, y: u16) -> String {
        let mut out = String::new();
        for x in buffer.area.x..buffer.area.right() {
            out.push_str(buffer.cell((x, y)).expect("cell inside buffer").symbol());
        }
        out
    }

    fn inside(rect: Rect, x: u16, y: u16) -> bool {
        x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
    }

    #[test]
    fn pane_titles_carry_their_digit_and_every_pane_is_drawn() {
        let app = loaded();
        let terminal = rendered(80, 24, &app);
        let screen = text(terminal.backend().buffer());
        for pane in Pane::ALL {
            let title = format!("{} {}", pane.number(), pane.title());
            assert!(screen.contains(&title), "missing `{title}`:\n{screen}");
        }
    }

    #[test]
    fn the_focused_pane_is_the_only_one_drawn_with_the_focus_colour() {
        for focus in Pane::ALL {
            let mut app = loaded();
            app.apply(Action::Focus(focus));
            let terminal = rendered(80, 24, &app);
            let buffer = terminal.backend().buffer();
            let focused = layout::split(buffer.area).unwrap().of(focus);
            let mut cyan = 0;
            for y in buffer.area.y..buffer.area.bottom() {
                for x in buffer.area.x..buffer.area.right() {
                    let cell = buffer.cell((x, y)).unwrap();
                    if cell.fg == Color::Cyan {
                        cyan += 1;
                        assert!(
                            inside(focused, x, y),
                            "{focus:?}: cyan escaped the focused pane at ({x},{y})"
                        );
                    }
                }
            }
            assert!(cyan > 0, "{focus:?}: the focused pane had no cyan cells");
        }
    }

    fn reversed_rows(buffer: &Buffer) -> BTreeSet<u16> {
        let mut rows = BTreeSet::new();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                if buffer
                    .cell((x, y))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
                {
                    rows.insert(y);
                }
            }
        }
        rows
    }

    #[test]
    fn the_catalog_rows_appear_in_the_repositories_pane_with_the_cursor_row_reversed() {
        let mut app = loaded();
        let first = rendered(80, 24, &app);
        let first_buffer = first.backend().buffer();
        let first_rows = reversed_rows(first_buffer);
        assert_eq!(first_rows.len(), 1, "exactly one row is selected");
        let first_y = *first_rows.first().unwrap();
        assert!(line(first_buffer, first_y).contains("alpha"));
        assert!(text(first_buffer).contains("beta (bare, read-only)"));

        app.apply(Action::CursorDown);
        let second = rendered(80, 24, &app);
        let second_buffer = second.backend().buffer();
        let second_rows = reversed_rows(second_buffer);
        assert_eq!(second_rows.len(), 1, "exactly one row remains selected");
        let second_y = *second_rows.first().unwrap();
        assert_eq!(second_y, first_y + 1, "the cursor moved down one row");
        assert!(line(second_buffer, second_y).contains("beta"));
    }

    #[test]
    fn the_placeholder_panes_say_which_slice_draws_them_rather_than_sitting_empty() {
        let app = loaded();
        let terminal = rendered(80, 24, &app);
        let screen = text(terminal.backend().buffer());
        for issue in ["#457", "#458", "#459"] {
            assert!(screen.contains(issue), "missing {issue}:\n{screen}");
        }
    }

    #[test]
    fn the_status_line_shows_the_text_and_turns_red_only_on_error() {
        let mut app = loaded();
        let info = rendered(80, 24, &app);
        let info_buffer = info.backend().buffer();
        assert!(text(info_buffer).contains("3 repositories"));
        assert!(
            info_buffer
                .content()
                .iter()
                .all(|cell| cell.fg != Color::Red),
            "an informational status must not be red"
        );

        app.receive(Data::Catalog(Err(String::from(
            "GET /api/catalog answered 503: catalog rebuilding",
        ))));
        let error = rendered(80, 24, &app);
        let error_buffer = error.backend().buffer();
        assert!(text(error_buffer).contains("503"));
        let mut red = 0;
        for y in error_buffer.area.y..error_buffer.area.bottom() {
            for x in error_buffer.area.x..error_buffer.area.right() {
                if error_buffer.cell((x, y)).unwrap().fg == Color::Red {
                    red += 1;
                    assert_eq!(y, error_buffer.area.bottom() - 1, "red left status row");
                }
            }
        }
        assert!(red > 0, "the error status had no red cells");
    }

    #[test]
    fn a_too_small_terminal_gets_the_minimum_message_not_a_frame() {
        let app = loaded();
        let terminal = rendered(30, 8, &app);
        let screen = text(terminal.backend().buffer());
        assert!(screen.contains("40x10"), "{screen}");
        assert!(screen.contains("30x8"), "{screen}");
        assert!(!screen.contains("Repositories"), "{screen}");
        assert!(!screen.contains("alpha"), "{screen}");
    }

    fn is_ansi_or_reset(colour: Color) -> bool {
        matches!(
            colour,
            Color::Reset
                | Color::Black
                | Color::Red
                | Color::Green
                | Color::Yellow
                | Color::Blue
                | Color::Magenta
                | Color::Cyan
                | Color::Gray
                | Color::DarkGray
                | Color::LightRed
                | Color::LightGreen
                | Color::LightYellow
                | Color::LightBlue
                | Color::LightMagenta
                | Color::LightCyan
                | Color::White
        )
    }

    #[test]
    fn colours_stay_within_the_sixteen_ansi_names() {
        for (width, height) in [(80, 24), (40, 10), (30, 8)] {
            let mut app = loaded();
            app.receive(Data::Catalog(Err(String::from("503: unavailable"))));
            let terminal = rendered(width, height, &app);
            for cell in terminal.backend().buffer().content() {
                assert!(is_ansi_or_reset(cell.fg), "non-ANSI fg: {:?}", cell.fg);
                assert!(is_ansi_or_reset(cell.bg), "non-ANSI bg: {:?}", cell.bg);
            }
        }
    }
}
