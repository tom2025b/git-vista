//! The shell's state and its reducer (M10.02, #457 — phase 2a).
//!
//! Pure: [`App`] holds what the screen shows, [`App::apply`] folds one
//! [`Action`] into it and returns the [`Fetch`]es the loop must dispatch,
//! [`App::receive`] folds one [`Data`] answer back in. No terminal, no
//! socket, no thread in this file — `ui.rs` draws it, `event.rs` drives it,
//! `data.rs` answers it — so every rule below is host-tested with nothing
//! but a struct in sight, the same reasoning as `features/conflicts/markers.rs`.
//!
//! # The four panes
//!
//! A lazygit-shaped frame: a left column of three stacked panes and one main
//! pane on the right. Phase 2a fills exactly one of them — Repositories,
//! from `GET /api/catalog`, the read #456 already proved — and leaves the
//! other three as honest placeholders naming the slice that draws them
//! (#457 the graph, #458 the detail and diff, #459 the working tree). The
//! focus ring, the cursor rules and the status line are the shell's, and
//! every pane inherits them.
//!
//! # Rules the tests pin
//!
//! - Focus starts on Repositories; `Tab`/`BackTab` cycle and wrap; a digit
//!   jumps straight to that pane.
//! - A cursor never leaves its pane's rows: it stops at the last row rather
//!   than wrapping, stays at zero on an empty pane, and is clamped when the
//!   rows it indexed are replaced by fewer.
//! - A failed fetch lands on the status line as an error and **keeps the
//!   old rows** — a transient refusal must not blank a screen the user was
//!   reading — and it never ends the loop (that is `event.rs`'s side of the
//!   same rule).
//! - Refresh coalesces: while a catalog fetch is in flight, another `r` asks
//!   nothing. A held-down key must not queue fifty reads behind a slow server.

use git_vista_protocol::{RepositoryDescriptor, RepositoryKind};

/// One of the four regions of the frame, in focus-ring order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Repositories,
    Branches,
    Commits,
    Main,
}

impl Pane {
    /// Focus-ring order, which is also the drawing order and the digit order.
    pub const ALL: [Pane; 4] = [
        Pane::Repositories,
        Pane::Branches,
        Pane::Commits,
        Pane::Main,
    ];

    fn index(self) -> usize {
        Pane::ALL
            .iter()
            .position(|p| *p == self)
            .expect("every Pane is in ALL")
    }

    /// The digit that focuses this pane (`1`–`4`), shown in its title.
    pub fn number(self) -> u8 {
        self.index() as u8 + 1
    }

    /// The pane a digit key names, if any.
    pub fn from_number(n: u8) -> Option<Pane> {
        Pane::ALL.get(usize::from(n).checked_sub(1)?).copied()
    }

    /// The title drawn on the pane's border.
    pub fn title(self) -> &'static str {
        match self {
            Pane::Repositories => "Repositories",
            Pane::Branches => "Branches",
            Pane::Commits => "Commits",
            Pane::Main => "Main",
        }
    }

    pub fn next(self) -> Pane {
        Pane::ALL[(self.index() + 1) % Pane::ALL.len()]
    }

    pub fn prev(self) -> Pane {
        Pane::ALL[(self.index() + Pane::ALL.len() - 1) % Pane::ALL.len()]
    }
}

/// What a key press means, after `keys.rs` has translated it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    FocusNext,
    FocusPrev,
    Focus(Pane),
    CursorDown,
    CursorUp,
    Refresh,
}

/// A read the loop must hand to the data layer. Phase 2a has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    Catalog,
}

/// A read's answer, back from the data layer.
#[derive(Debug)]
pub enum Data {
    Catalog(Result<Vec<RepositoryDescriptor>, String>),
}

/// How the status line should be drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Error,
}

/// The one-line status strip at the bottom of the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub tone: Tone,
}

/// The shell's whole state.
#[derive(Debug)]
pub struct App {
    pub focus: Pane,
    pub catalog: Vec<RepositoryDescriptor>,
    cursors: [usize; 4],
    pub status: Status,
    /// Catalog reads dispatched and not yet answered.
    pub in_flight: u32,
    pub quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> App {
        App {
            focus: Pane::Repositories,
            catalog: Vec::new(),
            cursors: [0; 4],
            status: Status {
                text: String::from("connecting to git-vista-server…"),
                tone: Tone::Info,
            },
            in_flight: 0,
            quit: false,
        }
    }

    /// The reads to dispatch before the first key arrives.
    pub fn start(&mut self) -> Vec<Fetch> {
        self.request_catalog()
    }

    /// Fold one action in; the reads it asks for come back to the loop.
    pub fn apply(&mut self, action: Action) -> Vec<Fetch> {
        match action {
            Action::Quit => {
                self.quit = true;
                Vec::new()
            }
            Action::FocusNext => {
                self.focus = self.focus.next();
                Vec::new()
            }
            Action::FocusPrev => {
                self.focus = self.focus.prev();
                Vec::new()
            }
            Action::Focus(pane) => {
                self.focus = pane;
                Vec::new()
            }
            Action::CursorDown => {
                let pane = self.focus;
                let cursor = self.cursor(pane);
                if cursor + 1 < self.rows(pane) {
                    self.cursors[pane.index()] = cursor + 1;
                }
                Vec::new()
            }
            Action::CursorUp => {
                let pane = self.focus;
                let cursor = self.cursor(pane);
                if cursor > 0 {
                    self.cursors[pane.index()] = cursor - 1;
                }
                Vec::new()
            }
            Action::Refresh => self.request_catalog(),
        }
    }

    /// Fold one answer in.
    pub fn receive(&mut self, data: Data) {
        match data {
            Data::Catalog(result) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match result {
                    Ok(catalog) => {
                        self.catalog = catalog;
                        self.clamp_cursors();
                        let n = self.catalog.len();
                        self.status = Status {
                            text: format!(
                                "{n} repositor{} · q quit · Tab focus · j/k move · r refresh",
                                if n == 1 { "y" } else { "ies" }
                            ),
                            tone: Tone::Info,
                        };
                    }
                    Err(message) => {
                        // The old rows stay: a transient refusal must not blank
                        // a screen the user was reading.
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                    }
                }
            }
        }
    }

    /// Ask for the catalog unless a catalog read is already out — a held
    /// `r` must not queue fifty reads behind a slow server.
    fn request_catalog(&mut self) -> Vec<Fetch> {
        if self.in_flight > 0 {
            return Vec::new();
        }
        self.in_flight += 1;
        vec![Fetch::Catalog]
    }

    /// After rows change, no cursor may point past the new end.
    fn clamp_cursors(&mut self) {
        for pane in Pane::ALL {
            let last = self.rows(pane).saturating_sub(1);
            let slot = &mut self.cursors[pane.index()];
            if *slot > last {
                *slot = last;
            }
        }
    }

    /// The selected row of a pane.
    pub fn cursor(&self, pane: Pane) -> usize {
        self.cursors[pane.index()]
    }

    /// How many rows a pane has to select among. Only Repositories has any
    /// in phase 2a; the others answer zero until their slices land.
    pub fn rows(&self, pane: Pane) -> usize {
        match pane {
            Pane::Repositories => self.catalog.len(),
            Pane::Branches | Pane::Commits | Pane::Main => 0,
        }
    }

    /// One catalog row as the Repositories pane lists it.
    pub fn catalog_row(repo: &RepositoryDescriptor) -> String {
        let kind = match repo.kind {
            RepositoryKind::Bare => "bare",
            RepositoryKind::MainWorktree => "main worktree",
            RepositoryKind::LinkedWorktree => "linked worktree",
        };
        let read_only = if repo.read_only { ", read-only" } else { "" };
        format!("{} ({kind}{read_only})", repo.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog exactly as the server serializes it — a wire literal, not
    /// a serialized DTO, for the reason `main.rs`'s tests give.
    const THREE: &str = r#"[
      {"repository":"r1","worktree":"w1","name":"alpha","kind":"main_worktree","read_only":false},
      {"repository":"r2","worktree":"w2","name":"beta","kind":"bare","read_only":true},
      {"repository":"r3","worktree":"w3","name":"gamma","kind":"linked_worktree","read_only":false}
    ]"#;
    const ONE: &str = r#"[
      {"repository":"r9","worktree":"w9","name":"solo","kind":"main_worktree","read_only":false}
    ]"#;

    fn catalog(wire: &str) -> Vec<RepositoryDescriptor> {
        serde_json::from_str(wire).expect("the literal is a valid catalog")
    }

    fn loaded(wire: &str) -> App {
        let mut app = App::new();
        assert_eq!(app.start(), [Fetch::Catalog]);
        app.receive(Data::Catalog(Ok(catalog(wire))));
        app
    }

    #[test]
    fn a_new_app_focuses_the_repositories_pane_and_asks_for_the_catalog_once_on_start() {
        let mut app = App::new();
        assert_eq!(app.focus, Pane::Repositories);
        assert!(!app.quit);
        assert_eq!(app.status.tone, Tone::Info);
        assert_eq!(app.start(), [Fetch::Catalog]);
        assert_eq!(app.in_flight, 1, "start counts its own read as in flight");
    }

    #[test]
    fn tab_cycles_focus_forward_and_wraps_and_shift_tab_cycles_back() {
        let mut app = App::new();
        let mut seen = vec![app.focus];
        for _ in 0..4 {
            assert!(
                app.apply(Action::FocusNext).is_empty(),
                "focus asks for no data"
            );
            seen.push(app.focus);
        }
        assert_eq!(
            seen,
            [
                Pane::Repositories,
                Pane::Branches,
                Pane::Commits,
                Pane::Main,
                Pane::Repositories
            ]
        );
        app.apply(Action::FocusPrev);
        assert_eq!(
            app.focus,
            Pane::Main,
            "backwards from the first wraps to the last"
        );
    }

    #[test]
    fn a_number_key_jumps_straight_to_that_pane() {
        let mut app = App::new();
        app.apply(Action::Focus(Pane::Commits));
        assert_eq!(app.focus, Pane::Commits);
        app.apply(Action::Focus(Pane::Repositories));
        assert_eq!(app.focus, Pane::Repositories);
        assert_eq!(Pane::from_number(1), Some(Pane::Repositories));
        assert_eq!(Pane::from_number(4), Some(Pane::Main));
        assert_eq!(Pane::from_number(0), None);
        assert_eq!(Pane::from_number(5), None);
        for pane in Pane::ALL {
            assert_eq!(Pane::from_number(pane.number()), Some(pane));
        }
    }

    #[test]
    fn the_cursor_moves_within_the_catalog_and_never_past_its_ends() {
        let mut app = loaded(THREE);
        assert_eq!(app.cursor(Pane::Repositories), 0);
        app.apply(Action::CursorUp);
        assert_eq!(app.cursor(Pane::Repositories), 0, "up at the top stays put");
        for _ in 0..5 {
            app.apply(Action::CursorDown);
        }
        assert_eq!(
            app.cursor(Pane::Repositories),
            2,
            "down stops at the last row, never wraps"
        );
        app.apply(Action::CursorUp);
        assert_eq!(app.cursor(Pane::Repositories), 1);

        // An empty pane's cursor is pinned at zero in both directions.
        app.apply(Action::Focus(Pane::Branches));
        app.apply(Action::CursorDown);
        app.apply(Action::CursorDown);
        assert_eq!(app.cursor(Pane::Branches), 0);
        app.apply(Action::CursorUp);
        assert_eq!(app.cursor(Pane::Branches), 0);
        // …and moving it left the Repositories cursor alone.
        assert_eq!(app.cursor(Pane::Repositories), 1);
    }

    #[test]
    fn a_catalog_answer_replaces_the_list_and_clamps_the_cursor() {
        let mut app = loaded(THREE);
        app.apply(Action::CursorDown);
        app.apply(Action::CursorDown);
        assert_eq!(app.cursor(Pane::Repositories), 2);
        assert_eq!(app.in_flight, 0, "the answer cleared the in-flight count");

        assert_eq!(app.apply(Action::Refresh), [Fetch::Catalog]);
        app.receive(Data::Catalog(Ok(catalog(ONE))));
        assert_eq!(app.catalog.len(), 1);
        assert_eq!(app.catalog[0].name, "solo");
        assert_eq!(
            app.cursor(Pane::Repositories),
            0,
            "a cursor past the new end is clamped, not left dangling"
        );
        assert_eq!(app.status.tone, Tone::Info);
        assert!(
            app.status.text.contains("1 repository"),
            "{}",
            app.status.text
        );
        assert!(
            app.status.text.contains("q quit"),
            "the status line says how to leave: {}",
            app.status.text
        );
    }

    #[test]
    fn a_catalog_failure_lands_on_the_status_line_as_an_error_and_keeps_the_old_list() {
        let mut app = loaded(THREE);
        app.apply(Action::Refresh);
        app.receive(Data::Catalog(Err(String::from(
            "GET /api/catalog answered 503: catalog rebuilding",
        ))));
        assert_eq!(app.status.tone, Tone::Error);
        assert!(app.status.text.contains("503"), "{}", app.status.text);
        assert!(
            app.status.text.contains("catalog rebuilding"),
            "{}",
            app.status.text
        );
        assert_eq!(
            app.catalog.len(),
            3,
            "a transient refusal must not blank the screen"
        );
        assert!(!app.quit, "a failed read never ends the session");
        assert_eq!(app.in_flight, 0);
    }

    #[test]
    fn refresh_asks_again_but_not_while_a_fetch_is_already_in_flight() {
        let mut app = App::new();
        assert_eq!(app.start(), [Fetch::Catalog]);
        assert!(
            app.apply(Action::Refresh).is_empty(),
            "coalesced: one read is already out"
        );
        assert!(app.apply(Action::Refresh).is_empty());
        assert_eq!(app.in_flight, 1);
        app.receive(Data::Catalog(Ok(Vec::new())));
        assert_eq!(
            app.apply(Action::Refresh),
            [Fetch::Catalog],
            "answered, so the next r asks again"
        );
        assert_eq!(app.in_flight, 1);
    }

    #[test]
    fn quit_sets_the_flag_and_asks_for_nothing() {
        let mut app = loaded(THREE);
        assert!(app.apply(Action::Quit).is_empty());
        assert!(app.quit);
    }

    #[test]
    fn a_catalog_row_names_the_kind_and_says_read_only_when_it_is() {
        let rows: Vec<String> = catalog(THREE).iter().map(App::catalog_row).collect();
        assert_eq!(
            rows,
            [
                "alpha (main worktree)",
                "beta (bare, read-only)",
                "gamma (linked worktree)"
            ]
        );
    }
}
