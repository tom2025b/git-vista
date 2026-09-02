//! Ratatui rendering for the four-pane shell (M10.02, #457 — phase 2a).
//!
//! Drawing is a pure projection of [`App`]. Every event-loop turn draws;
//! Ratatui's terminal diff suppresses unchanged writes, keeping invalidation
//! logic out of the state model. The palette is intentionally limited to
//! ANSI names: cyan marks the focused border, red marks an error status, and
//! selection uses the terminal's own reversed modifier.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Pane, Tone};
use crate::layout;
use crate::panes::detail::RowTone;
use crate::panes::graph::{self, ColorDepth, Emphasis, Foreground, GraphLine, LayoutData};
use crate::panes::plan_review::{PlanReviewPane, RowTone as PlanRowTone};

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

    // #457's graph: refs are badged inline on their commit row (see
    // graph.rs's ref-badge handling), not listed separately here. No issue
    // yet owns a standalone branches list, so this stays an honest
    // placeholder rather than a claim #457 will fill it.
    draw_placeholder(
        frame,
        panes.of(Pane::Branches),
        Pane::Branches,
        app.focus,
        "a branches list is not yet built",
    );
    draw_commits(frame, panes.of(Pane::Commits), app, detect_color_depth());
    draw_main(frame, panes.of(Pane::Main), app);
    if let Some(review) = &app.plan_review {
        draw_plan_review(frame, area, review);
    }

    let status_style = match app.status.tone {
        Tone::Info => Style::default(),
        Tone::Error => Style::default().fg(Color::Red),
    };
    frame.render_widget(
        Paragraph::new(app.status.text.as_str()).style(status_style),
        panes.status,
    );
}

/// Draw plan review as a modal over the shell: no pane underneath can look
/// actionable while the reducer is deliberately ignoring its navigation.
fn draw_plan_review(frame: &mut Frame, area: Rect, review: &PlanReviewPane) {
    let margin_x = if area.width >= 70 { 4 } else { 1 };
    let margin_y = if area.height >= 18 { 2 } else { 1 };
    let modal = Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(margin_x * 2),
        // Leave the shell's final status row visible.
        height: area.height.saturating_sub(margin_y * 2 + 1),
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Yellow))
        .title(Line::styled(
            " Plan review — nothing runs until approval ",
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(format!(" {} ", review.help())));
    let rows = review.rows();
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .skip(review.offset())
        .map(|row| Line::styled(row.text, plan_tone_style(row.tone)))
        .collect();

    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        modal,
    );
}

fn plan_tone_style(tone: PlanRowTone) -> Style {
    match tone {
        PlanRowTone::Plain => Style::default(),
        PlanRowTone::Heading => Style::default().add_modifier(Modifier::BOLD),
        PlanRowTone::Muted => Style::default().fg(Color::DarkGray),
        PlanRowTone::Risk => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        PlanRowTone::Advisory => Style::default().fg(Color::Yellow),
        PlanRowTone::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
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

/// The commit graph (#457): core's lanes and edges, windowed to the pane's
/// visible rows and scrolled to keep the cursor's commit on screen.
fn draw_commits(frame: &mut Frame, area: Rect, app: &App, colors: ColorDepth) {
    let inner = pane_block(Pane::Commits, app.focus).inner(area);
    if app.commits.is_empty() {
        let message = if app.active_repo.is_some() {
            "No commits."
        } else {
            "Select a repository and press Enter."
        };
        frame.render_widget(
            Paragraph::new(message).block(pane_block(Pane::Commits, app.focus)),
            area,
        );
        return;
    }

    let layout = LayoutData {
        rows: &app.commits,
        edges: &app.edges,
        stubs: &app.stubs,
        lane_count: app.lane_count,
    };
    let pane = graph::GraphPane::new(layout, colors);
    let total_lines = pane.row_count();
    let visible_height = inner.height as usize;
    // A commit sits on an even physical row (odd rows are connectors); keep
    // that row inside the window rather than tracking separate scroll state.
    let selected_physical = app.cursor(Pane::Commits).saturating_mul(2);
    let max_offset = total_lines.saturating_sub(visible_height);
    let line_offset = if visible_height == 0 || selected_physical < visible_height {
        0
    } else {
        (selected_physical + 1 - visible_height).min(max_offset)
    };

    let lines: Vec<Line<'static>> = pane
        .window(line_offset, visible_height)
        .iter()
        .enumerate()
        .map(|(i, line)| graph_line(line, line_offset + i == selected_physical))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(pane_block(Pane::Commits, app.focus)),
        area,
    );
}

/// One [`GraphLine`] as a styled Ratatui [`Line`]; `selected` reverses the
/// whole row, the same convention this module's other selections use.
fn graph_line(line: &GraphLine, selected: bool) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|span| {
            let mut style = Style::default();
            if let Foreground::Indexed(index) = span.foreground {
                style = style.fg(Color::Indexed(index));
            }
            if span.emphasis == Emphasis::Bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Span::styled(span.text.clone(), style)
        })
        .collect();
    Line::from(spans)
}

/// The commit detail and diff (#458): [`DetailPane`](crate::panes::detail::DetailPane)
/// already windows its own rows, so this hands it the pane's cursor as a
/// vertical offset and lets Ratatui's own scroll carry the horizontal one.
fn draw_main(frame: &mut Frame, area: Rect, app: &App) {
    let inner = pane_block(Pane::Main, app.focus).inner(area);
    let rows = app
        .detail
        .window(app.cursor(Pane::Main), inner.height as usize);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|row| Line::styled(row.text.clone(), tone_style(row.tone)))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((0, app.detail.horizontal() as u16))
            .block(pane_block(Pane::Main, app.focus)),
        area,
    );
}

fn tone_style(tone: RowTone) -> Style {
    match tone {
        RowTone::Plain | RowTone::Parent => Style::default(),
        RowTone::Heading => Style::default().add_modifier(Modifier::BOLD),
        RowTone::Muted => Style::default().fg(Color::DarkGray),
        RowTone::Added => Style::default().fg(Color::Green),
        RowTone::Removed => Style::default().fg(Color::Red),
        RowTone::Hunk => Style::default().fg(Color::Cyan),
        RowTone::Error => Style::default().fg(Color::Red),
        RowTone::SelectedParent => Style::default().add_modifier(Modifier::REVERSED),
    }
}

/// The terminal's colour capability, sniffed the same way most CLI tools do
/// when no proper terminfo query is wired: `COLORTERM` for a true-colour
/// terminal, `TERM` for the `256color` convention, `Basic` otherwise. Basic
/// is the safe default — degrading to it on an unrecognised terminal is
/// "honest" in exactly the sense #457 asks for; claiming 256 colours a
/// terminal cannot show is the failure this guards against.
fn detect_color_depth() -> ColorDepth {
    color_depth_from_env(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    )
}

fn color_depth_from_env(term: Option<&str>, colorterm: Option<&str>) -> ColorDepth {
    let rich_colorterm = colorterm.is_some_and(|v| v == "truecolor" || v == "24bit");
    let rich_term = term.is_some_and(|v| v.contains("256color"));
    if rich_colorterm || rich_term {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Basic
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use git_vista_core::diff::{CommitDiff, DiffFile};
    use git_vista_core::model::{CommitDetail, CommitSummary, GraphRow, Oid};
    use git_vista_core::status::ChangeKind;
    use git_vista_protocol::{
        GenerationToken, GitOperation, OperationHash, Plan, RecoveryStrategy, RepositoryDescriptor,
        RepositoryToken, RiskLevel, UnixSeconds, WorktreeToken,
    };
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::Terminal;

    use super::*;
    use crate::app::{Action, App, CommitPage, Data, Pane};
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

    fn reviewing() -> App {
        let mut app = loaded();
        let plan = Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("worktree-1").unwrap(),
            generation: GenerationToken::new("generation-reviewed").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_788_365_000),
            expires_at: UnixSeconds(1_788_365_300),
            risk: RiskLevel::Remote,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            advisories: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
        };
        app.receive(Data::PlanReady(Ok(serde_json::to_vec(&plan).unwrap())));
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

    const COMMIT: &str = "1111111111111111111111111111111111111111";

    fn page() -> CommitPage {
        CommitPage {
            rows: vec![GraphRow {
                commit: CommitSummary {
                    id: Oid(COMMIT.to_string()),
                    parents: Vec::new(),
                    summary: "render the detail pane".to_string(),
                    author: "Ada".to_string(),
                    time: 1_700_000_000,
                },
                row: 0,
                lane: 0,
                refs: Vec::new(),
                color: 0,
                on_remote: false,
            }],
            edges: Vec::new(),
            stubs: Vec::new(),
            lane_count: 1,
            cursor: None,
            generation: GenerationToken::new("generation-1").unwrap(),
        }
    }

    fn detail() -> CommitDetail {
        CommitDetail {
            id: Oid(COMMIT.to_string()),
            parents: Vec::new(),
            author_name: "Ada Author".to_string(),
            author_email: "ada@example.com".to_string(),
            author_time: 1_700_000_001,
            committer_name: "Casey Committer".to_string(),
            committer_email: "casey@example.com".to_string(),
            commit_time: 1_700_000_099,
            message: "subject\n\nbody".to_string(),
            on_remote: false,
        }
    }

    fn detailed(patch: &str, files: Vec<DiffFile>) -> App {
        let mut app = loaded();
        app.apply(Action::Activate);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page()),
        });
        app.apply(Action::Activate);
        app.receive(Data::Commit {
            repo: "w1".to_string(),
            id: COMMIT.to_string(),
            result: Ok(detail()),
        });
        app.receive(Data::Diff {
            repo: "w1".to_string(),
            id: COMMIT.to_string(),
            result: Ok(CommitDiff {
                id: COMMIT.to_string(),
                files,
                patch: patch.to_string(),
                truncated: false,
                against_first_parent: false,
            }),
        });
        app
    }

    fn row_containing(buffer: &Buffer, needle: &str) -> u16 {
        (buffer.area.y..buffer.area.bottom())
            .find(|y| line(buffer, *y).contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?}:\n{}", text(buffer)))
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

    // #457 (graph) and #458 (detail/diff) are wired now — the dedicated tests
    // below exercise their real content. Branches has no owning issue yet and
    // stays an honest placeholder; Commits and Main say why they are empty
    // rather than sitting blank, which is the property this test used to pin
    // for all three panes via their old `#457`/`#458`/`#459` placeholder text.
    #[test]
    fn the_still_unbuilt_branches_pane_says_so_rather_than_sitting_empty() {
        let app = loaded();
        let terminal = rendered(80, 24, &app);
        let screen = text(terminal.backend().buffer());
        assert!(
            screen.contains("branches list is not yet built"),
            "{screen}"
        );
    }

    #[test]
    fn the_commits_pane_names_why_it_is_empty_before_and_after_a_repository_is_open() {
        let mut app = loaded();
        let before = rendered(80, 24, &app);
        assert!(
            text(before.backend().buffer()).contains("Select a repository and press Enter."),
            "{}",
            text(before.backend().buffer())
        );

        app.apply(Action::Activate);
        let after = rendered(80, 24, &app);
        assert!(
            text(after.backend().buffer()).contains("No commits."),
            "{}",
            text(after.backend().buffer())
        );
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

    #[test]
    fn the_commit_selector_renders_the_existing_summary_and_selection() {
        let mut app = loaded();
        app.apply(Action::Activate);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page()),
        });
        let terminal = rendered(80, 24, &app);
        let buffer = terminal.backend().buffer();
        let y = row_containing(buffer, "1111111 render the detail pane");
        assert!(inside(layout::split(buffer.area).unwrap().commits, 1, y));
        assert!(line(buffer, y).contains("1111111 render the detail pane"));
        assert!(
            (0..buffer.area.width).any(|x| buffer
                .cell((x, y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)),
            "the selected commit row is not visibly selected"
        );
    }

    fn many_commits(n: usize) -> CommitPage {
        CommitPage {
            rows: (0..n)
                .map(|i| GraphRow {
                    commit: CommitSummary {
                        id: Oid(format!("{i:040}")),
                        parents: Vec::new(),
                        summary: format!("commit number {i}"),
                        author: "Ada".to_string(),
                        time: 1_700_000_000,
                    },
                    row: i,
                    lane: 0,
                    refs: Vec::new(),
                    color: 0,
                    on_remote: false,
                })
                .collect(),
            edges: Vec::new(),
            stubs: Vec::new(),
            lane_count: 1,
            cursor: None,
            generation: GenerationToken::new("generation-1").unwrap(),
        }
    }

    /// Pins the scroll-follows-cursor behaviour `draw_commits` adds on top of
    /// `graph::render_window`'s own windowing: with more commits than fit,
    /// the cursor's row must still be visible and the off-screen head must
    /// not leak in — the graph pane analogue of the Main pane's own
    /// windowing test.
    #[test]
    fn the_commits_pane_scrolls_to_keep_the_cursor_on_screen() {
        let mut app = loaded();
        app.apply(Action::Activate);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(many_commits(200)),
        });
        for _ in 0..199 {
            app.apply(Action::CursorDown);
        }

        let terminal = rendered(80, 24, &app);
        let screen = text(terminal.backend().buffer());
        assert!(
            screen.contains("commit number 199"),
            "the selected (last) commit must be visible:\n{screen}"
        );
        assert!(
            !screen.contains("commit number 0 "),
            "the far off-screen head must not leak in:\n{screen}"
        );
    }

    #[test]
    fn the_main_pane_draws_metadata_binary_labels_and_diff_line_colours() {
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n same\ndiff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n";
        let files = vec![DiffFile {
            path: "logo.png".to_string(),
            old_path: None,
            kind: ChangeKind::Modified,
            additions: None,
            deletions: None,
        }];
        let app = detailed(patch, files);
        let terminal = rendered(100, 30, &app);
        let buffer = terminal.backend().buffer();
        let screen = text(buffer);
        for expected in [
            "Ada Author <ada@example.com>",
            "Casey Committer <casey@example.com>",
            "binary — content not shown",
            "binary file — contents not shown",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
        }
        assert!(!screen.contains("Binary files a/logo.png"));

        for (needle, colour) in [("-old", Color::Red), ("+new", Color::Green)] {
            let y = row_containing(buffer, needle);
            let rendered = line(buffer, y);
            // `line()` concatenates one cell *symbol* per column, but this
            // row also carries the left column's own multi-byte box-drawing
            // borders before Main's content — `str::find` returns a BYTE
            // offset, which drifts from the COLUMN index the moment any of
            // those wide-byte, single-column glyphs precede the match. Count
            // characters instead so `start` is a real column.
            let start = rendered
                .char_indices()
                .position(|(byte, _)| rendered[byte..].starts_with(needle))
                .expect("needle is present, row_containing found it")
                as u16;
            for x in start..start + needle.chars().count() as u16 {
                assert_eq!(buffer.cell((x, y)).unwrap().fg, colour, "{needle} at x={x}");
            }
        }
    }

    #[test]
    fn a_long_diff_line_is_clipped_inside_the_frame_and_can_scroll_horizontally() {
        let long = "+0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-END";
        let patch = format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1 @@\n{long}\n"
        );
        let mut app = detailed(&patch, Vec::new());
        let first = rendered(80, 24, &app);
        let first_buffer = first.backend().buffer();
        let panes = layout::split(first_buffer.area).unwrap();
        let first_y = row_containing(first_buffer, "+012345");
        assert!(!line(first_buffer, first_y).contains("-END"));
        for y in panes.main.y + 1..panes.main.bottom() - 1 {
            assert_eq!(
                first_buffer
                    .cell((panes.main.right() - 1, y))
                    .unwrap()
                    .symbol(),
                "│",
                "long content overwrote the Main border at row {y}"
            );
        }

        app.apply(Action::HorizontalRight);
        let second = rendered(80, 24, &app);
        let shifted = text(second.backend().buffer());
        assert!(
            !shifted.contains("+012345"),
            "horizontal offset was ignored:\n{shifted}"
        );
        assert!(
            shifted.contains("456789"),
            "the shifted line vanished:\n{shifted}"
        );
    }

    #[test]
    fn color_depth_degrades_to_basic_unless_the_terminal_says_otherwise() {
        assert_eq!(color_depth_from_env(None, None), ColorDepth::Basic);
        assert_eq!(
            color_depth_from_env(Some("xterm"), None),
            ColorDepth::Basic,
            "a terminal that does not advertise 256colour must not be guessed into it"
        );
        assert_eq!(
            color_depth_from_env(Some("xterm-256color"), None),
            ColorDepth::Ansi256
        );
        assert_eq!(
            color_depth_from_env(Some("xterm"), Some("truecolor")),
            ColorDepth::Ansi256
        );
        assert_eq!(
            color_depth_from_env(Some("xterm"), Some("24bit")),
            ColorDepth::Ansi256
        );
        assert_eq!(
            color_depth_from_env(Some("xterm"), Some("nonsense")),
            ColorDepth::Basic,
            "an unrecognised COLORTERM value must not be treated as rich"
        );
    }

    #[test]
    fn plan_review_is_a_visible_modal_with_decision_keys_and_expiry() {
        let app = reviewing();
        let terminal = rendered(100, 30, &app);
        let buffer = terminal.backend().buffer();
        let screen = text(buffer);
        for expected in [
            "Plan review",
            "nothing runs until approval",
            "a approve",
            "Esc refuse",
            "expires: 1788365300",
            "Preconditions",
            "Expected ref changes",
            "REMOTE",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
        }
        assert!(
            buffer.content().iter().any(|cell| cell.fg == Color::Yellow),
            "the modal had no review/risk emphasis"
        );
    }

    #[test]
    fn stale_refusal_replaces_server_cause_with_the_honest_message() {
        let mut app = reviewing();
        assert_eq!(app.apply(Action::ApprovePlan).len(), 1);
        app.receive(Data::PlanSubmitted(
            crate::panes::plan_review::SubmissionOutcome::from_response(
                409,
                b"The repository changed while this plan was pending",
            ),
        ));
        let terminal = rendered(100, 30, &app);
        let screen = text(terminal.backend().buffer());
        assert!(screen.contains("Plan is stale."), "{screen}");
        assert!(!screen.contains("repository changed"), "{screen}");
    }
}
