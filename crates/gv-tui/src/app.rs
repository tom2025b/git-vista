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

use git_vista_core::diff::CommitDiff;
use git_vista_core::model::{CommitDetail, Edge, FrameStub, GraphRow};
use git_vista_protocol::{HistoryPage, RepositoryDescriptor, RepositoryKind};

use crate::panes::detail::DetailPane;
use crate::panes::plan_review::{PlanApproval, PlanReviewPane, SubmissionOutcome};

/// The existing paged-history wire shape, instantiated with the lane core's
/// types. #458 uses its summaries as a small selector; #457 remains the owner
/// of rendering the lanes and edges themselves.
pub type CommitPage = HistoryPage<GraphRow, Edge, FrameStub>;

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
    Activate,
    ParentPrev,
    ParentNext,
    HorizontalLeft,
    HorizontalRight,
    ApprovePlan,
    RefusePlan,
}

/// A read the loop must hand to the data layer. Phase 2a has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    Catalog,
    History { repo: String },
    Commit { repo: String, id: String },
    Diff { repo: String, id: String },
    ExecutePlan(PlanApproval),
}

/// A read's answer, back from the data layer.
#[derive(Debug)]
pub enum Data {
    /// Exact response bytes from `/api/plan`. #461 will produce this answer;
    /// keeping the bytes here avoids a serialize-after-review seam.
    #[allow(dead_code)] // The producer is deliberately #461, which this pane gates.
    PlanReady(Result<Vec<u8>, String>),
    Catalog(Result<Vec<RepositoryDescriptor>, String>),
    History {
        repo: String,
        result: Result<CommitPage, String>,
    },
    Commit {
        repo: String,
        id: String,
        result: Result<CommitDetail, String>,
    },
    Diff {
        repo: String,
        id: String,
        result: Result<CommitDiff, String>,
    },
    PlanSubmitted(SubmissionOutcome),
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
    pub active_repo: Option<String>,
    pub commits: Vec<GraphRow>,
    /// The rest of the fetched [`CommitPage`], kept beside `commits` so
    /// `ui.rs` can hand the graph renderer the same lanes core computed —
    /// no relayout, no separate fetch.
    pub edges: Vec<Edge>,
    pub stubs: Vec<FrameStub>,
    pub lane_count: usize,
    pub detail: DetailPane,
    /// A modal review blocks every ordinary navigation action until the user
    /// approves or refuses it. #461 hands received `/api/plan` bytes to
    /// [`App::present_plan`]; this slice owns everything after that seam.
    pub plan_review: Option<PlanReviewPane>,
    cursors: [usize; 4],
    pub status: Status,
    /// Catalog reads dispatched and not yet answered.
    pub in_flight: u32,
    history_in_flight: Option<String>,
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
            active_repo: None,
            commits: Vec::new(),
            edges: Vec::new(),
            stubs: Vec::new(),
            lane_count: 0,
            detail: DetailPane::default(),
            plan_review: None,
            cursors: [0; 4],
            status: Status {
                text: String::from("connecting to git-vista-server…"),
                tone: Tone::Info,
            },
            in_flight: 0,
            history_in_flight: None,
            quit: false,
        }
    }

    /// The reads to dispatch before the first key arrives.
    pub fn start(&mut self) -> Vec<Fetch> {
        self.request_catalog()
    }

    /// Fold one action in; the reads it asks for come back to the loop.
    pub fn apply(&mut self, action: Action) -> Vec<Fetch> {
        if self.plan_review.is_some() {
            return self.apply_plan_review(action);
        }
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
            Action::Activate => self.activate(),
            Action::ParentPrev => {
                if self.focus == Pane::Main {
                    self.detail.select_parent(-1);
                }
                Vec::new()
            }
            Action::ParentNext => {
                if self.focus == Pane::Main {
                    self.detail.select_parent(1);
                }
                Vec::new()
            }
            Action::HorizontalLeft => {
                if self.focus == Pane::Main {
                    self.detail.scroll_horizontal(-4);
                }
                Vec::new()
            }
            Action::HorizontalRight => {
                if self.focus == Pane::Main {
                    self.detail.scroll_horizontal(4);
                }
                Vec::new()
            }
            Action::ApprovePlan | Action::RefusePlan => Vec::new(),
        }
    }

    /// Fold one answer in.
    pub fn receive(&mut self, data: Data) {
        match data {
            Data::PlanReady(result) => match result {
                Ok(wire) => {
                    if let Err(message) = self.present_plan(wire) {
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                    }
                }
                Err(message) => {
                    self.status = Status {
                        text: message,
                        tone: Tone::Error,
                    };
                }
            },
            Data::Catalog(result) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match result {
                    Ok(catalog) => {
                        self.catalog = catalog;
                        if self.active_repo.as_ref().is_some_and(|active| {
                            !self.catalog.iter().any(|repo| repo.worktree == *active)
                        }) {
                            self.active_repo = None;
                            self.commits.clear();
                            self.edges.clear();
                            self.stubs.clear();
                            self.lane_count = 0;
                            self.detail = DetailPane::default();
                        }
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
            Data::History { repo, result } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return;
                }
                self.history_in_flight = None;
                match result {
                    Ok(page) => {
                        self.commits = page.rows;
                        self.edges = page.edges;
                        self.stubs = page.stubs;
                        self.lane_count = page.lane_count;
                        self.clamp_cursors();
                        self.status = Status {
                            text: format!(
                                "{} commits · Enter opens detail · q quit · Tab focus",
                                self.commits.len()
                            ),
                            tone: Tone::Info,
                        };
                    }
                    Err(message) => {
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                    }
                }
            }
            Data::Commit { repo, id, result } => {
                let error = result.as_ref().err().cloned();
                if self.detail.receive_detail(&repo, &id, result) {
                    self.clamp_cursors();
                    self.status = error.map_or_else(
                        || Status {
                            text: String::from(
                                "commit loaded · [/] parent · arrows scroll · q quit",
                            ),
                            tone: Tone::Info,
                        },
                        |text| Status {
                            text,
                            tone: Tone::Error,
                        },
                    );
                }
            }
            Data::Diff { repo, id, result } => {
                let error = result.as_ref().err().cloned();
                if self.detail.receive_diff(&repo, &id, result) {
                    self.clamp_cursors();
                    self.status = error.map_or_else(
                        || Status {
                            text: String::from(
                                "detail ready · j/k vertical · ←/→ horizontal · [/] parent",
                            ),
                            tone: Tone::Info,
                        },
                        |text| Status {
                            text,
                            tone: Tone::Error,
                        },
                    );
                }
            }
            Data::PlanSubmitted(outcome) => {
                let success = matches!(outcome, SubmissionOutcome::Executed(_));
                let message = outcome.message();
                if success {
                    self.plan_review = None;
                    self.status = Status {
                        text: message,
                        tone: Tone::Info,
                    };
                } else if let Some(review) = self.plan_review.as_mut() {
                    review.receive(outcome);
                    self.status = Status {
                        text: message,
                        tone: Tone::Error,
                    };
                }
            }
        }
    }

    /// Open the immutable review modal from the exact bytes `/api/plan`
    /// returned. This is the integration seam for #461's operation builders.
    fn present_plan(&mut self, wire: Vec<u8>) -> Result<(), String> {
        let review = PlanReviewPane::from_wire(wire)?;
        self.plan_review = Some(review);
        self.status = Status {
            text: String::from("review the plan · a approve · Esc refuse · j/k scroll"),
            tone: Tone::Info,
        };
        Ok(())
    }

    fn apply_plan_review(&mut self, action: Action) -> Vec<Fetch> {
        match action {
            Action::Quit => {
                self.quit = true;
                Vec::new()
            }
            Action::ApprovePlan => {
                let Some(approval) = self.plan_review.as_mut().and_then(PlanReviewPane::approve)
                else {
                    return Vec::new();
                };
                self.status = Status {
                    text: String::from("submitting reviewed plan for server re-validation…"),
                    tone: Tone::Info,
                };
                vec![Fetch::ExecutePlan(approval)]
            }
            Action::RefusePlan => {
                let submitting = self
                    .plan_review
                    .as_ref()
                    .is_some_and(PlanReviewPane::is_submitting);
                if submitting {
                    self.status = Status {
                        text: String::from(
                            "approval is already submitted; wait for the server's answer",
                        ),
                        tone: Tone::Error,
                    };
                } else {
                    self.plan_review = None;
                    self.status = Status {
                        text: String::from("plan refused locally · nothing was executed"),
                        tone: Tone::Info,
                    };
                }
                Vec::new()
            }
            Action::CursorDown => {
                if let Some(review) = self.plan_review.as_mut() {
                    review.scroll(1);
                }
                Vec::new()
            }
            Action::CursorUp => {
                if let Some(review) = self.plan_review.as_mut() {
                    review.scroll(-1);
                }
                Vec::new()
            }
            // The modal owns the keyboard until a decision is made. In
            // particular, refresh cannot silently replace a stale plan.
            Action::FocusNext
            | Action::FocusPrev
            | Action::Focus(_)
            | Action::Refresh
            | Action::Activate
            | Action::ParentPrev
            | Action::ParentNext
            | Action::HorizontalLeft
            | Action::HorizontalRight => Vec::new(),
        }
    }

    fn activate(&mut self) -> Vec<Fetch> {
        match self.focus {
            Pane::Repositories => self.activate_repository(),
            Pane::Branches => Vec::new(),
            Pane::Commits => {
                let Some(repo) = self.active_repo.clone() else {
                    return Vec::new();
                };
                let Some(id) = self
                    .commits
                    .get(self.cursor(Pane::Commits))
                    .map(|row| row.commit.id.0.clone())
                else {
                    return Vec::new();
                };
                self.open_detail(repo, id)
            }
            Pane::Main => {
                let Some(repo) = self.active_repo.clone() else {
                    return Vec::new();
                };
                let Some(id) = self.detail.selected_parent().map(str::to_string) else {
                    return Vec::new();
                };
                self.open_detail(repo, id)
            }
        }
    }

    fn activate_repository(&mut self) -> Vec<Fetch> {
        let Some(repo) = self
            .catalog
            .get(self.cursor(Pane::Repositories))
            .map(|repo| repo.worktree.clone())
        else {
            return Vec::new();
        };
        if self.history_in_flight.as_deref() == Some(repo.as_str()) {
            return Vec::new();
        }
        if self.active_repo.as_deref() != Some(repo.as_str()) {
            self.commits.clear();
            self.edges.clear();
            self.stubs.clear();
            self.lane_count = 0;
            self.cursors[Pane::Commits.index()] = 0;
            self.cursors[Pane::Main.index()] = 0;
            self.detail = DetailPane::default();
        }
        self.active_repo = Some(repo.clone());
        self.history_in_flight = Some(repo.clone());
        self.focus = Pane::Commits;
        self.status = Status {
            text: String::from("loading commits…"),
            tone: Tone::Info,
        };
        vec![Fetch::History { repo }]
    }

    fn open_detail(&mut self, repo: String, id: String) -> Vec<Fetch> {
        self.detail.open(repo.clone(), id.clone());
        self.cursors[Pane::Main.index()] = 0;
        self.focus = Pane::Main;
        self.status = Status {
            text: format!("loading {}…", short_id(&id)),
            tone: Tone::Info,
        };
        vec![
            Fetch::Commit {
                repo: repo.clone(),
                id: id.clone(),
            },
            Fetch::Diff { repo, id },
        ]
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
            Pane::Branches => 0,
            Pane::Commits => self.commits.len(),
            Pane::Main => self.detail.row_count(),
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

fn short_id(id: &str) -> &str {
    &id[..id.len().min(7)]
}

#[cfg(test)]
mod tests {
    use git_vista_core::model::{CommitSummary, Oid};
    use git_vista_protocol::{
        GenerationToken, GitOperation, OperationHash, Plan, RecoveryStrategy, RepositoryToken,
        RiskLevel, UnixSeconds, WorktreeToken,
    };

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

    fn plan_wire() -> Vec<u8> {
        serde_json::to_vec(&Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("worktree-1").unwrap(),
            generation: GenerationToken::new("generation-reviewed").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_788_365_000),
            expires_at: UnixSeconds(1_788_365_300),
            risk: RiskLevel::Safe,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            advisories: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
        })
        .unwrap()
    }

    fn page(rows: &[(&str, &str)]) -> CommitPage {
        CommitPage {
            rows: rows
                .iter()
                .enumerate()
                .map(|(row, (id, summary))| GraphRow {
                    commit: CommitSummary {
                        id: Oid((*id).to_string()),
                        parents: Vec::new(),
                        summary: (*summary).to_string(),
                        author: "Ada".to_string(),
                        time: 1_700_000_000,
                    },
                    row,
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

    fn detail(id: &str, parents: &[&str]) -> CommitDetail {
        CommitDetail {
            id: Oid(id.to_string()),
            parents: parents.iter().map(|id| Oid((*id).to_string())).collect(),
            author_name: "Ada".to_string(),
            author_email: "ada@example.com".to_string(),
            author_time: 1,
            committer_name: "Casey".to_string(),
            committer_email: "casey@example.com".to_string(),
            commit_time: 2,
            message: "message".to_string(),
            on_remote: false,
        }
    }

    const COMMIT_1: &str = "1111111111111111111111111111111111111111";
    const COMMIT_2: &str = "2222222222222222222222222222222222222222";
    const PARENT_1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

    #[test]
    fn activating_the_selected_repository_requests_its_bounded_history_page() {
        let mut app = loaded(THREE);
        app.apply(Action::CursorDown);

        assert_eq!(
            app.apply(Action::Activate),
            [Fetch::History {
                repo: "w2".to_string()
            }]
        );
        assert_eq!(app.active_repo.as_deref(), Some("w2"));
        assert_eq!(app.focus, Pane::Commits);
    }

    #[test]
    fn a_history_answer_populates_commits_and_enter_requests_detail_and_diff() {
        let mut app = loaded(THREE);
        assert_eq!(app.apply(Action::Activate).len(), 1);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "first"), (COMMIT_2, "second")])),
        });
        assert_eq!(app.rows(Pane::Commits), 2);
        app.apply(Action::CursorDown);

        assert_eq!(
            app.apply(Action::Activate),
            [
                Fetch::Commit {
                    repo: "w1".to_string(),
                    id: COMMIT_2.to_string(),
                },
                Fetch::Diff {
                    repo: "w1".to_string(),
                    id: COMMIT_2.to_string(),
                },
            ]
        );
        assert_eq!(app.focus, Pane::Main);
        assert_eq!(app.detail.current(), Some(("w1", COMMIT_2)));
    }

    #[test]
    fn a_late_history_answer_for_the_previous_repository_is_ignored() {
        let mut app = loaded(THREE);
        app.apply(Action::Activate);
        app.apply(Action::Focus(Pane::Repositories));
        app.apply(Action::CursorDown);
        app.apply(Action::Activate);

        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "stale")])),
        });
        assert!(app.commits.is_empty());
        app.receive(Data::History {
            repo: "w2".to_string(),
            result: Ok(page(&[(COMMIT_2, "current")])),
        });
        assert_eq!(app.commits[0].commit.summary, "current");
    }

    #[test]
    fn a_selected_parent_can_be_opened_from_the_main_pane() {
        let mut app = loaded(THREE);
        app.apply(Action::Activate);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "merge")])),
        });
        app.apply(Action::Activate);
        app.receive(Data::Commit {
            repo: "w1".to_string(),
            id: COMMIT_1.to_string(),
            result: Ok(detail(COMMIT_1, &[PARENT_1, PARENT_2])),
        });
        app.apply(Action::ParentNext);

        assert_eq!(
            app.apply(Action::Activate),
            [
                Fetch::Commit {
                    repo: "w1".to_string(),
                    id: PARENT_2.to_string(),
                },
                Fetch::Diff {
                    repo: "w1".to_string(),
                    id: PARENT_2.to_string(),
                },
            ]
        );
        assert_eq!(app.detail.current(), Some(("w1", PARENT_2)));
    }

    #[test]
    fn main_vertical_and_horizontal_scroll_positions_are_independent() {
        let mut app = loaded(THREE);
        app.apply(Action::Activate);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "long")])),
        });
        app.apply(Action::Activate);
        app.receive(Data::Commit {
            repo: "w1".to_string(),
            id: COMMIT_1.to_string(),
            result: Ok(detail(COMMIT_1, &[])),
        });

        app.apply(Action::CursorDown);
        app.apply(Action::HorizontalRight);
        assert_eq!(app.cursor(Pane::Main), 1);
        assert_eq!(app.detail.horizontal(), 4);
        app.apply(Action::HorizontalLeft);
        assert_eq!(app.cursor(Pane::Main), 1);
        assert_eq!(app.detail.horizontal(), 0);
    }

    #[test]
    fn a_received_plan_blocks_navigation_until_it_is_refused_locally() {
        let mut app = loaded(THREE);
        app.receive(Data::PlanReady(Ok(plan_wire())));
        assert!(app.plan_review.is_some());

        let focus = app.focus;
        assert!(app.apply(Action::FocusNext).is_empty());
        assert_eq!(app.focus, focus, "the shell moved underneath its modal");
        assert!(app.apply(Action::Refresh).is_empty());

        assert!(app.apply(Action::RefusePlan).is_empty());
        assert!(app.plan_review.is_none());
        assert_eq!(
            app.status.text,
            "plan refused locally · nothing was executed"
        );
    }

    #[test]
    fn approval_mints_one_submission_and_a_stale_answer_is_not_retried() {
        let wire = plan_wire();
        let mut app = loaded(THREE);
        app.receive(Data::PlanReady(Ok(wire.clone())));

        let requests = app.apply(Action::ApprovePlan);
        let [Fetch::ExecutePlan(approval)] = requests.as_slice() else {
            panic!("approval did not produce exactly one execute request: {requests:?}");
        };
        assert_eq!(approval.body(), wire);
        assert!(
            app.apply(Action::ApprovePlan).is_empty(),
            "a repeated key press minted another request"
        );

        app.receive(Data::PlanSubmitted(SubmissionOutcome::Stale));
        assert!(app.plan_review.is_some(), "the refusal details vanished");
        assert_eq!(
            app.status.text,
            "Plan is stale. It was not executed. Build and review a new plan."
        );
        assert!(
            app.apply(Action::Refresh).is_empty(),
            "a stale refusal silently rebuilt the plan"
        );
    }

    #[test]
    fn a_successful_approval_closes_the_modal_and_surfaces_the_receipt() {
        let mut app = loaded(THREE);
        app.receive(Data::PlanReady(Ok(plan_wire())));
        assert_eq!(app.apply(Action::ApprovePlan).len(), 1);
        app.receive(Data::PlanSubmitted(SubmissionOutcome::Executed(
            "Staged all changes.".to_string(),
        )));
        assert!(app.plan_review.is_none());
        assert_eq!(app.status.text, "Staged all changes.");
        assert_eq!(app.status.tone, Tone::Info);
    }
}
