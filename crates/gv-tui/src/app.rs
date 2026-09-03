//! The terminal shell's state and reducer (M10.02/#457, extended by #459).
//!
//! Pure: [`App`] holds what the screen shows, [`App::apply`] folds one
//! [`Action`] into it and returns the [`Request`]es the loop must dispatch,
//! [`App::receive`] folds one [`Data`] answer back in. No terminal, no
//! socket, no thread in this file — `ui.rs` draws it, `event.rs` drives it,
//! `data.rs` answers it — so every rule below is host-tested with nothing
//! but a struct in sight, the same reasoning as `features/conflicts/markers.rs`.
//!
//! # The four panes
//!
//! A lazygit-shaped frame: a left column of three stacked panes and one main
//! pane on the right. Repositories selects the server session, Working Tree
//! projects the shared status and staging vocabulary, Commits renders the
//! shared graph, and Main switches among commit detail, staging diff,
//! destructive confirmation, and exact plan review. The focus ring, cursor
//! rules, and status line belong to the shell and every pane inherits them.
//!
//! # Rules the tests pin
//!
//! - Focus starts on Repositories; `Tab`/`BackTab` cycle and wrap; a digit
//!   jumps straight to that pane.
//! - A cursor never leaves its pane's rows: it stops at the last row rather
//!   than wrapping, stays at zero on an empty pane, and is clamped when the
//!   rows it indexed are replaced by fewer.
//! - A failed request lands on the status line as an error and **keeps the
//!   old rows** — a transient refusal must not blank a screen the user was
//!   reading — and it never ends the loop (that is `event.rs`'s side of the
//!   same rule).
//! - Refresh coalesces: while a catalog request is in flight, another `r` asks
//!   nothing. A held-down key must not queue fifty reads behind a slow server.

use git_vista_conflicts::core::{Pane as ConflictView, ResultRead};
use git_vista_core::diff::{BlobContent, CommitDiff};
use git_vista_core::model::{CommitDetail, Edge, FrameStub, GraphRow};
use git_vista_protocol::conflict::{ConflictedFile, Resolution};
use git_vista_protocol::{
    CommitOid, ConflictSource, GenerationToken, GitOperation, HistoryPage, OperationId,
    OperationStatus, PatchPlan, PatchPreview, Plan, RepositoryDescriptor, RepositoryKind,
    StageDirection, StagingDiff, TagDetail, WorktreePath, WorktreeStatus,
};

use crate::commands::{self, Command};
use crate::panes::conflicts::{self, ConflictsPane};
use crate::panes::detail::DetailPane;
use crate::panes::plan_review::{
    cancellable_operation, PlanApproval, PlanReviewPane, SubmissionOutcome,
};
use crate::panes::staging::StagingPane;
use crate::panes::worktree::WorktreePane;

/// The existing paged-history wire shape, instantiated with the lane core's
/// types. #458 uses its summaries as a small selector; #457 remains the owner
/// of rendering the lanes and edges themselves.
pub type CommitPage = HistoryPage<GraphRow, Edge, FrameStub>;

/// One of the four regions of the frame, in focus-ring order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Repositories,
    WorkingTree,
    Commits,
    Main,
}

impl Pane {
    /// Focus-ring order, which is also the drawing order and the digit order.
    pub const ALL: [Pane; 4] = [
        Pane::Repositories,
        Pane::WorkingTree,
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
            Pane::WorkingTree => "Working Tree",
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

/// What the frame that was just drawn can actually show.
///
/// `ui.rs` measures this from the very rects it rendered each widget into and
/// the event loop hands it to [`App::observe`] before the next key is read.
/// That indirection is the whole point: a page is then the pane's **current**
/// visible row count, which is what #625 asks for and what a constant cannot
/// be. A constant would be right in a one-third-height pane and wrong the
/// moment [`Action::ToggleMaximize`] gives that same pane the whole window —
/// wrong, that is, in exactly the state the zoom key exists to create, while
/// looking correct in every small-pane test.
///
/// The unit is the one that pane's cursor counts, not always a terminal row:
/// the graph draws a connector line between commits, so its viewport is
/// **commits** per screen rather than lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    panes: [usize; 4],
    overlay: usize,
}

impl Viewport {
    /// `panes` in [`Pane::ALL`] order; `overlay` is the body of whichever
    /// full-frame surface was up (the conflict overlay, a plan review).
    pub fn new(panes: [usize; 4], overlay: usize) -> Viewport {
        Viewport { panes, overlay }
    }

    /// Rows of that pane's own content the last frame had room for.
    pub fn rows(&self, pane: Pane) -> usize {
        self.panes[pane.index()]
    }

    /// Rows the last frame's overlay had room for.
    pub fn overlay(&self) -> usize {
        self.overlay
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
    /// `PageDown` / `PageUp` — one visible page of the focused pane (#625).
    /// How far that is comes from [`Viewport`], never from a constant.
    CursorPageDown,
    CursorPageUp,
    /// `Home` / `End` — the first and last row of the focused pane (#625).
    CursorTop,
    CursorBottom,
    /// `z` — give the focused pane the whole window, or put the four-pane
    /// shape back (#625).
    ToggleMaximize,
    Refresh,
    Activate,
    ParentPrev,
    ParentNext,
    HorizontalLeft,
    HorizontalRight,
    /// Open the conflict overlay (M10.07, #462).
    OpenConflicts,
    /// One key press inside the conflict overlay. The overlay owns its own
    /// keymap: while its text buffer has focus, `q` is a letter somebody
    /// typed, not a request to quit the program.
    Conflict(conflicts::Act),
    PreviewSelection,
    PreviewWholeTree,
    Discard,
    Approve,
    Cancel,
    ApprovePlan,
    RefusePlan,
    OpenCommand,
    CommandChar(char),
    CommandBackspace,
    SubmitCommand,
    CancelOperation,
}

/// One authenticated request the event loop hands to the data worker. Writes
/// are represented only by typed shared plans — never argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Catalog,
    Select {
        repo: String,
    },
    History {
        repo: String,
    },
    Status {
        repo: String,
    },
    Commit {
        repo: String,
        id: String,
    },
    Diff {
        repo: String,
        id: String,
    },
    /// `GET /api/conflicts` (M10.07, #462).
    Conflicts {
        repo: String,
    },
    /// `GET /api/blob/{oid}` — one stage of one conflicted path.
    ConflictStage {
        repo: String,
        path: String,
        pane: ConflictView,
        oid: String,
    },
    /// `GET /api/worktree-file/{*path}` — the result pane.
    ConflictResult {
        repo: String,
        path: String,
    },
    /// `GET /api/conflict-source/{*path}` — the marker file and its
    /// `conflict-v1:` token (ADR 0069).
    ConflictSource {
        repo: String,
        path: String,
    },
    /// `POST /api/resolve-conflict` — take a whole side, or the deletion.
    ResolveWholeFile {
        repo: String,
        path: String,
        resolution: Resolution,
    },
    /// `POST /api/resolve-conflict-content` — a block/line/hand-edited
    /// resolution (ADR 0069).
    ResolveContent {
        repo: String,
        path: String,
        expected_stages: [Option<CommitOid>; 3],
        expected_source: GenerationToken,
        content: String,
    },
    StagingDiff {
        repo: String,
        direction: StageDirection,
    },
    BuildPlan {
        repo: String,
        operation: GitOperation,
    },
    BuildPlanWire(GitOperation),
    PreviewPatch {
        repo: String,
        plan: PatchPlan,
    },
    ExecutePlan {
        repo: String,
        plan: Box<Plan>,
    },
    ExecuteReviewedPlan(PlanApproval),
    ApplyPatch {
        repo: String,
        plan: PatchPlan,
    },
    Tags {
        repo: String,
    },
    OperationByKey {
        key: String,
    },
    OperationStatus {
        id: OperationId,
    },
    CancelOperation {
        id: OperationId,
    },
}

/// A read's answer, back from the data layer.
#[derive(Debug)]
pub enum Data {
    /// Exact response bytes from `/api/plan`. #461 will produce this answer;
    /// keeping the bytes here avoids a serialize-after-review seam.
    PlanReady(Result<Vec<u8>, String>),
    Selected {
        repo: String,
        result: Result<(), String>,
    },
    Tags {
        repo: String,
        result: Result<Vec<TagDetail>, String>,
    },
    OperationByKey {
        key: String,
        result: Result<Option<OperationId>, String>,
    },
    OperationStatus(Result<OperationStatus, String>),
    OperationCancelled {
        id: OperationId,
        result: Result<String, String>,
    },
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
    Conflicts {
        repo: String,
        result: Result<Vec<ConflictedFile>, String>,
    },
    ConflictStage {
        repo: String,
        path: String,
        pane: ConflictView,
        result: Result<BlobContent, String>,
    },
    /// The result pane's read. A [`ResultRead`] rather than a `Result`
    /// because "there is no file at this path" is one of its three answers
    /// and is information, not a failure.
    ConflictResult {
        repo: String,
        path: String,
        read: ResultRead,
    },
    ConflictSource {
        repo: String,
        path: String,
        result: Result<ConflictSource, String>,
    },
    Resolved {
        repo: String,
        path: String,
        result: Result<(), String>,
    },
    Status {
        repo: String,
        result: Result<WorktreeStatus, String>,
    },
    StagingDiff {
        repo: String,
        direction: StageDirection,
        result: Result<StagingDiff, String>,
    },
    Plan {
        repo: String,
        result: Result<Plan, String>,
    },
    PatchPreview {
        repo: String,
        plan: PatchPlan,
        result: Result<PatchPreview, String>,
    },
    Written {
        repo: String,
        result: Result<String, String>,
    },
    PlanSubmitted(SubmissionOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    pub prompt: String,
    operation: GitOperation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Review {
    Operation(Box<Plan>),
    Patch {
        plan: PatchPlan,
        preview: PatchPreview,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingFile {
    repo: String,
    path: String,
    direction: StageDirection,
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

#[derive(Debug)]
pub struct TrackedExecution {
    key: String,
    id: Option<OperationId>,
    status: Option<OperationStatus>,
    cancellable: bool,
    cancel_requested: bool,
    lookup_in_flight: bool,
    status_in_flight: bool,
    cancel_in_flight: bool,
    ticks: u8,
}

/// The shell's whole state.
#[derive(Debug)]
pub struct App {
    pub focus: Pane,
    pub catalog: Vec<RepositoryDescriptor>,
    pub active_repo: Option<String>,
    /// Repository last acknowledged by `/api/select` in Active mode.
    pub writable_repo: Option<String>,
    pub commits: Vec<GraphRow>,
    /// The rest of the requested [`CommitPage`], kept beside `commits` so
    /// `ui.rs` can hand the graph renderer the same lanes core computed —
    /// no relayout, no separate request.
    pub edges: Vec<Edge>,
    pub stubs: Vec<FrameStub>,
    pub lane_count: usize,
    pub detail: DetailPane,
    /// The conflict overlay (M10.07, #462). It takes the whole frame when
    /// open, so it is not a fifth member of the focus ring.
    pub conflicts: ConflictsPane,
    /// A modal review blocks every ordinary navigation action until the user
    /// approves or refuses it. #461 hands received `/api/plan` bytes to
    /// [`App::present_plan`]; this slice owns everything after that seam.
    pub worktree: WorktreePane,
    pub staging: Option<StagingPane>,
    pub confirmation: Option<Confirmation>,
    pub review: Option<Review>,
    pub plan_review: Option<PlanReviewPane>,
    pending_file: Option<PendingFile>,
    /// Text after `:` while the command palette owns the keyboard.
    pub command_input: Option<String>,
    /// Last explicit tag listing for the active repository.
    pub tags: Vec<TagDetail>,
    pub tags_loaded: bool,
    pub execution: Option<TrackedExecution>,
    refresh_after_write: bool,
    cursors: [usize; 4],
    /// What the last drawn frame could show (#625). Written by
    /// [`App::observe`] once a frame; read whenever a page has to be a page.
    viewport: Viewport,
    /// Whether the focused pane currently has the whole window (#625).
    maximized: bool,
    pub status: Status,
    /// Catalog reads dispatched and not yet answered.
    pub in_flight: u32,
    history_in_flight: Option<String>,
    selection_in_flight: Option<String>,
    status_in_flight: Option<String>,
    write_in_flight: bool,
    execution_in_flight: bool,
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
            writable_repo: None,
            commits: Vec::new(),
            edges: Vec::new(),
            stubs: Vec::new(),
            lane_count: 0,
            detail: DetailPane::default(),
            conflicts: ConflictsPane::default(),
            worktree: WorktreePane::default(),
            staging: None,
            confirmation: None,
            review: None,
            plan_review: None,
            pending_file: None,
            command_input: None,
            tags: Vec::new(),
            tags_loaded: false,
            execution: None,
            refresh_after_write: false,
            cursors: [0; 4],
            viewport: Viewport::default(),
            maximized: false,
            status: Status {
                text: String::from("connecting to git-vista-server…"),
                tone: Tone::Info,
            },
            in_flight: 0,
            history_in_flight: None,
            selection_in_flight: None,
            status_in_flight: None,
            write_in_flight: false,
            execution_in_flight: false,
            quit: false,
        }
    }

    /// The reads to dispatch before the first key arrives.
    pub fn start(&mut self) -> Vec<Request> {
        self.request_catalog()
    }

    /// Fold one action in; the reads it asks for come back to the loop.
    pub fn apply(&mut self, action: Action) -> Vec<Request> {
        if self.plan_review.is_some() {
            return self.apply_plan_review(action);
        }
        if self.command_input.is_some() {
            return self.apply_command(action);
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
            Action::CursorPageDown => {
                let pane = self.focus;
                let page = self.page(pane);
                self.move_cursor_by(pane, page as isize);
                Vec::new()
            }
            Action::CursorPageUp => {
                let pane = self.focus;
                let page = self.page(pane);
                self.move_cursor_by(pane, -(page as isize));
                Vec::new()
            }
            Action::CursorTop => {
                self.cursors[self.focus.index()] = 0;
                Vec::new()
            }
            Action::CursorBottom => {
                let pane = self.focus;
                self.cursors[pane.index()] = self.rows(pane).saturating_sub(1);
                Vec::new()
            }
            Action::ToggleMaximize => {
                self.maximized = !self.maximized;
                Vec::new()
            }
            Action::Refresh => {
                let mut requests = self.request_catalog();
                requests.extend(self.request_status());
                requests
            }
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
                if self.focus == Pane::Main && self.staging.is_none() && self.review.is_none() {
                    self.detail.scroll_horizontal(-4);
                }
                Vec::new()
            }
            Action::HorizontalRight => {
                if self.focus == Pane::Main && self.staging.is_none() && self.review.is_none() {
                    self.detail.scroll_horizontal(4);
                }
                Vec::new()
            }
            Action::OpenConflicts => self.open_conflicts(),
            Action::Conflict(act) => {
                let requests = self.conflicts.apply(act);
                self.status = Status {
                    text: self.conflicts_status_text(),
                    tone: Tone::Info,
                };
                self.dispatch_conflict(requests)
            }
            Action::PreviewSelection => self.preview_selection(),
            Action::PreviewWholeTree => self.preview_whole_tree(),
            Action::Discard => self.confirm_discard(),
            Action::Approve => self.approve(),
            Action::Cancel => {
                if self.execution_in_flight {
                    self.status = Status {
                        text: String::from(
                            "the reviewed plan was already submitted; wait for its outcome",
                        ),
                        tone: Tone::Error,
                    };
                    return Vec::new();
                }
                self.confirmation = None;
                self.review = None;
                // A file shortcut begins before its staging diff exists.
                // Refusal must invalidate that earlier intent too, otherwise
                // the late diff can still turn it into a PatchPreview.
                self.pending_file = None;
                self.write_in_flight = false;
                self.status = Status {
                    text: String::from("cancelled · no changes were made"),
                    tone: Tone::Info,
                };
                self.clamp_cursors();
                Vec::new()
            }
            Action::OpenCommand => self.open_command(),
            Action::ApprovePlan
            | Action::RefusePlan
            | Action::CommandChar(_)
            | Action::CommandBackspace
            | Action::SubmitCommand
            | Action::CancelOperation => Vec::new(),
        }
    }

    /// Fold one answer in.
    pub fn receive(&mut self, data: Data) -> Vec<Request> {
        match data {
            Data::Selected { repo, result } => {
                match result {
                    Ok(()) if self.active_repo.as_deref() == Some(repo.as_str()) => {
                        self.selection_in_flight = None;
                        self.writable_repo = Some(repo.clone());
                        self.history_in_flight = Some(repo.clone());
                        self.status_in_flight = Some(repo.clone());
                        self.worktree.begin_load();
                        self.status = Status {
                            text: String::from("loading history and working tree…"),
                            tone: Tone::Info,
                        };
                        return vec![
                            Request::History { repo: repo.clone() },
                            Request::Status { repo },
                        ];
                    }
                    Ok(()) => {}
                    Err(message) if self.active_repo.as_deref() == Some(repo.as_str()) => {
                        self.selection_in_flight = None;
                        self.writable_repo = None;
                        self.status = Status {
                            text: format!("repository is not writable: {message}"),
                            tone: Tone::Error,
                        };
                    }
                    Err(_) => {}
                }
                Vec::new()
            }
            Data::Tags { repo, result } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
                }
                match result {
                    Ok(tags) => {
                        self.tags = tags;
                        self.tags_loaded = true;
                        let names = self
                            .tags
                            .iter()
                            .map(|tag| tag.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.status = Status {
                            text: if names.is_empty() {
                                String::from("no tags")
                            } else {
                                format!("{} tag(s): {names}", self.tags.len())
                            },
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
                Vec::new()
            }
            Data::PlanReady(result) => {
                match result {
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
                }
                Vec::new()
            }
            Data::Catalog(result) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                match result {
                    Ok(catalog) => {
                        self.catalog = catalog;
                        if self.active_repo.as_ref().is_some_and(|active| {
                            !self.catalog.iter().any(|repo| repo.worktree == *active)
                        }) {
                            self.active_repo = None;
                            self.writable_repo = None;
                            self.tags.clear();
                            self.tags_loaded = false;
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
                Vec::new()
            }
            Data::History { repo, result } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
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
                Vec::new()
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
                Vec::new()
            }
            Data::Conflicts { repo, result } => {
                self.conflicts.receive_conflicts(&repo, result);
                self.refresh_conflicts_status();
                Vec::new()
            }
            Data::ConflictStage {
                repo,
                path,
                pane,
                result,
            } => {
                self.conflicts.receive_stage(&repo, &path, pane, result);
                Vec::new()
            }
            Data::ConflictResult { repo, path, read } => {
                self.conflicts.receive_result(&repo, &path, read);
                Vec::new()
            }
            Data::ConflictSource { repo, path, result } => {
                self.conflicts.receive_source(&repo, &path, result);
                self.refresh_conflicts_status();
                Vec::new()
            }
            Data::Resolved { repo, path, result } => {
                // A resolution changes the working tree and the index, so the
                // list this overlay is showing is stale by definition. It is
                // refetched rather than patched in place: `conflicts::scan` is
                // stateless and re-reads git on every call (ADR 0063), and a
                // client that edited its own copy of the list would be
                // asserting an outcome it never observed.
                let refetch = self.conflicts.receive_resolved(&repo, &path, result);
                self.refresh_conflicts_status();
                if refetch {
                    self.dispatch_conflict(vec![conflicts::Request::Conflicts])
                } else {
                    Vec::new()
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
                Vec::new()
            }
            Data::Status { repo, result } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
                }
                self.status_in_flight = None;
                let error = result.as_ref().err().cloned();
                self.worktree.receive(result);
                self.clamp_cursors();
                self.status = error.map_or_else(
                    || Status {
                        text: String::from(
                            "working tree ready · Space preview · a all · d discard",
                        ),
                        tone: Tone::Info,
                    },
                    |text| Status {
                        text,
                        tone: Tone::Error,
                    },
                );
                Vec::new()
            }
            Data::StagingDiff {
                repo,
                direction,
                result,
            } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
                }
                match result {
                    Ok(diff) => {
                        self.staging = Some(StagingPane::new(direction, diff));
                        self.confirmation = None;
                        self.review = None;
                        self.cursors[Pane::Main.index()] = 0;
                        self.focus = Pane::Main;
                        if let Some(pending) = self.pending_file.take().filter(|pending| {
                            pending.repo == repo && pending.direction == direction
                        }) {
                            return self.preview_file_from_loaded_diff(pending.path);
                        }
                        self.status = Status {
                            text: format!(
                                "{} diff · Space previews file/hunk/line",
                                direction_words(direction)
                            ),
                            tone: Tone::Info,
                        };
                    }
                    Err(message) => {
                        self.pending_file = None;
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                    }
                }
                Vec::new()
            }
            Data::Plan { repo, result } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
                }
                if !self.write_in_flight {
                    return Vec::new();
                }
                self.write_in_flight = false;
                match result {
                    Ok(plan) => {
                        self.confirmation = None;
                        self.review = Some(Review::Operation(Box::new(plan)));
                        self.cursors[Pane::Main.index()] = 0;
                        self.focus = Pane::Main;
                        self.status = Status {
                            text: String::from("review the plan · y approve · n/Esc refuse"),
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
                Vec::new()
            }
            Data::PatchPreview { repo, plan, result } => {
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
                }
                if !self.write_in_flight {
                    return Vec::new();
                }
                self.write_in_flight = false;
                match result {
                    Ok(preview) => {
                        if preview.generation != plan.generation {
                            self.status = Status {
                                text: String::from(
                                    "preview generation did not match the submitted patch plan",
                                ),
                                tone: Tone::Error,
                            };
                            return Vec::new();
                        }
                        self.review = Some(Review::Patch { plan, preview });
                        self.cursors[Pane::Main.index()] = 0;
                        self.focus = Pane::Main;
                        self.status = Status {
                            text: String::from("review the patch plan · y approve · n/Esc refuse"),
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
                Vec::new()
            }
            Data::Written { repo, result } => {
                // The data worker is ordered: this completion releases every
                // later request, including a repository selection queued
                // while the write ran. Clear the global busy state before
                // deciding whether the old repository's result is still
                // relevant to the visible pane.
                self.write_in_flight = false;
                self.execution_in_flight = false;
                if self.active_repo.as_deref() != Some(repo.as_str()) {
                    return Vec::new();
                }
                match result {
                    Ok(message) => {
                        self.review = None;
                        self.confirmation = None;
                        self.staging = None;
                        self.cursors[Pane::Main.index()] = 0;
                        self.status = Status {
                            text: if message.trim().is_empty() {
                                String::from("write completed · refreshing working tree…")
                            } else {
                                message
                            },
                            tone: Tone::Info,
                        };
                        self.request_status()
                    }
                    Err(message) => {
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                        Vec::new()
                    }
                }
            }
            Data::OperationByKey { key, result } => {
                let Some(execution) = self.execution.as_mut().filter(|item| item.key == key) else {
                    return Vec::new();
                };
                execution.lookup_in_flight = false;
                match result {
                    Ok(Some(id)) => {
                        execution.id = Some(id);
                        execution.ticks = 9;
                        self.status = Status {
                            text: String::from("operation admitted · waiting for progress…"),
                            tone: Tone::Info,
                        };
                    }
                    Ok(None) => {
                        execution.ticks = 9;
                    }
                    Err(message) => {
                        self.status = Status {
                            text: format!("could not locate operation progress: {message}"),
                            tone: Tone::Error,
                        };
                    }
                }
                Vec::new()
            }
            Data::OperationStatus(result) => {
                match result {
                    Ok(operation) => {
                        let Some(execution) = self
                            .execution
                            .as_mut()
                            .filter(|item| item.id.as_ref().is_some_and(|id| *id == operation.id))
                        else {
                            return Vec::new();
                        };
                        execution.status_in_flight = false;
                        execution.ticks = 0;
                        self.status = Status {
                            text: operation_progress_line(&operation),
                            tone: if operation.state == git_vista_protocol::OperationState::Failed {
                                Tone::Error
                            } else {
                                Tone::Info
                            },
                        };
                        execution.status = Some(operation);
                    }
                    Err(message) => {
                        if let Some(execution) = self.execution.as_mut() {
                            execution.status_in_flight = false;
                        }
                        self.status = Status {
                            text: format!("could not read operation progress: {message}"),
                            tone: Tone::Error,
                        };
                    }
                }
                Vec::new()
            }
            Data::OperationCancelled { id, result } => {
                let Some(execution) = self
                    .execution
                    .as_mut()
                    .filter(|item| item.id.as_ref() == Some(&id))
                else {
                    return Vec::new();
                };
                execution.cancel_in_flight = false;
                self.status = match result {
                    Ok(message) => Status {
                        text: if message.trim().is_empty() {
                            String::from("cancellation requested; waiting for the terminal outcome")
                        } else {
                            message
                        },
                        tone: Tone::Info,
                    },
                    Err(message) => Status {
                        text: format!("cancellation was refused: {message}"),
                        tone: Tone::Error,
                    },
                };
                Vec::new()
            }
            Data::PlanSubmitted(outcome) => {
                self.execution = None;
                let success = matches!(outcome, SubmissionOutcome::Executed(_));
                let message = outcome.message();
                if success {
                    self.plan_review = None;
                    self.refresh_after_write = true;
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
                Vec::new()
            }
        }
    }

    /// Poll the tracked operation at a bounded cadence. The event loop calls
    /// this only on its 50 ms tick; ten ticks means at most two requests per
    /// second, and each in-flight flag prevents a slow server from queuing
    /// duplicates behind itself.
    pub fn tick(&mut self) -> Vec<Request> {
        if self.refresh_after_write {
            self.refresh_after_write = false;
            if let Some(repo) = self.active_repo.clone() {
                let mut requests = vec![Request::History { repo: repo.clone() }];
                if self.tags_loaded {
                    requests.push(Request::Tags { repo });
                }
                return requests;
            }
        }
        let Some(execution) = self.execution.as_mut() else {
            return Vec::new();
        };
        if execution
            .status
            .as_ref()
            .is_some_and(OperationStatus::is_terminal)
        {
            return Vec::new();
        }
        if execution.cancel_requested && execution.cancellable && !execution.cancel_in_flight {
            if let Some(id) = execution.id.clone() {
                execution.cancel_in_flight = true;
                return vec![Request::CancelOperation { id }];
            }
        }
        execution.ticks = execution.ticks.saturating_add(1);
        if execution.ticks < 10 {
            return Vec::new();
        }
        execution.ticks = 0;
        if let Some(id) = execution.id.clone() {
            if execution.status_in_flight {
                Vec::new()
            } else {
                execution.status_in_flight = true;
                vec![Request::OperationStatus { id }]
            }
        } else if execution.lookup_in_flight {
            Vec::new()
        } else {
            execution.lookup_in_flight = true;
            vec![Request::OperationByKey {
                key: execution.key.clone(),
            }]
        }
    }

    /// Open the immutable review modal from the exact bytes `/api/plan`
    /// returned. This is the integration seam for #461's operation builders.
    fn present_plan(&mut self, wire: Vec<u8>) -> Result<(), String> {
        self.plan_review = Some(PlanReviewPane::from_wire(wire)?);
        self.status = Status {
            text: String::from("review the plan · a approve · Esc refuse · j/k scroll"),
            tone: Tone::Info,
        };
        Ok(())
    }

    fn apply_plan_review(&mut self, action: Action) -> Vec<Request> {
        match action {
            Action::Approve | Action::ApprovePlan => {
                let Some(review) = self.plan_review.as_mut() else {
                    return Vec::new();
                };
                let cancellable = review.cancellable();
                let Some(approval) = review.approve() else {
                    return Vec::new();
                };
                let key = approval.key().to_string();
                self.execution = Some(TrackedExecution {
                    key: key.clone(),
                    id: None,
                    status: None,
                    cancellable,
                    cancel_requested: false,
                    lookup_in_flight: true,
                    status_in_flight: false,
                    cancel_in_flight: false,
                    ticks: 0,
                });
                self.status = Status {
                    text: String::from("submitting reviewed plan for server re-validation…"),
                    tone: Tone::Info,
                };
                vec![
                    Request::ExecuteReviewedPlan(approval),
                    Request::OperationByKey { key },
                ]
            }
            Action::CancelOperation => self.request_cancel(),
            Action::Cancel | Action::RefusePlan => {
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
            Action::CursorPageDown | Action::CursorPageUp => {
                // The modal's own body, not a pane's — it is drawn inside a
                // margin over the whole frame, so it is taller than any of
                // them and pages by more.
                let page = self.viewport.overlay().max(1) as isize;
                let delta = if action == Action::CursorPageDown {
                    page
                } else {
                    -page
                };
                if let Some(review) = self.plan_review.as_mut() {
                    review.scroll(delta);
                }
                Vec::new()
            }
            Action::CursorTop => {
                if let Some(review) = self.plan_review.as_mut() {
                    review.scroll_to(0);
                }
                Vec::new()
            }
            Action::CursorBottom => {
                if let Some(review) = self.plan_review.as_mut() {
                    review.scroll_to(usize::MAX);
                }
                Vec::new()
            }
            Action::Quit => {
                self.quit = true;
                Vec::new()
            }
            Action::FocusNext
            | Action::FocusPrev
            | Action::Focus(_)
            | Action::Refresh
            | Action::Activate
            | Action::ParentPrev
            | Action::ParentNext
            | Action::HorizontalLeft
            | Action::HorizontalRight
            | Action::PreviewSelection
            | Action::PreviewWholeTree
            | Action::Discard
            | Action::OpenCommand
            | Action::CommandChar(_)
            | Action::CommandBackspace
            | Action::SubmitCommand
            // #462's two, ignored here for #461's own reason: a plan review is
            // waiting on a decision, and opening the conflict overlay would
            // put a full-screen takeover in front of an approval the user has
            // neither given nor refused. The modal is dismissed by deciding,
            // never by navigating away from it.
            | Action::OpenConflicts
            | Action::Conflict(_)
            // Zoom is a shape the panes take, and no pane is reachable while
            // a plan is waiting to be approved or refused. Toggling it under
            // the modal would rearrange a frame the user cannot see.
            | Action::ToggleMaximize => Vec::new(),
        }
    }

    fn request_cancel(&mut self) -> Vec<Request> {
        let Some(execution) = self.execution.as_mut() else {
            return Vec::new();
        };
        if !execution.cancellable {
            self.status = Status {
                text: String::from("this operation is short-lived and cannot be cancelled"),
                tone: Tone::Error,
            };
            return Vec::new();
        }
        execution.cancel_requested = true;
        if let Some(id) = execution.id.clone().filter(|_| !execution.cancel_in_flight) {
            execution.cancel_in_flight = true;
            self.status = Status {
                text: String::from(
                    "requesting transfer cancellation · a push may already have published",
                ),
                tone: Tone::Info,
            };
            vec![Request::CancelOperation { id }]
        } else {
            self.status = Status {
                text: String::from("cancellation queued while the operation id is located…"),
                tone: Tone::Info,
            };
            Vec::new()
        }
    }

    fn open_command(&mut self) -> Vec<Request> {
        if self.active_repo.is_none() {
            self.status = Status {
                text: String::from("select a repository before planning a write"),
                tone: Tone::Error,
            };
        } else if self.writable_repo != self.active_repo {
            self.status = Status {
                text: String::from("this repository is not open for writes"),
                tone: Tone::Error,
            };
        } else {
            self.command_input = Some(String::new());
            self.status = Status {
                text: commands::HELP.to_string(),
                tone: Tone::Info,
            };
        }
        Vec::new()
    }

    fn apply_command(&mut self, action: Action) -> Vec<Request> {
        match action {
            Action::Quit => {
                self.quit = true;
                Vec::new()
            }
            Action::RefusePlan => {
                self.command_input = None;
                self.status = Status {
                    text: String::from("command cancelled · nothing was sent"),
                    tone: Tone::Info,
                };
                Vec::new()
            }
            Action::CommandChar(character) => {
                if let Some(input) = self.command_input.as_mut() {
                    input.push(character);
                }
                Vec::new()
            }
            Action::CommandBackspace => {
                if let Some(input) = self.command_input.as_mut() {
                    input.pop();
                }
                Vec::new()
            }
            Action::SubmitCommand => {
                let input = self.command_input.take().unwrap_or_default();
                match commands::parse(&input) {
                    Ok(Command::Plan(operation)) => {
                        self.status = Status {
                            text: String::from("building a reviewable plan…"),
                            tone: Tone::Info,
                        };
                        vec![Request::BuildPlanWire(operation)]
                    }
                    Ok(Command::ListTags) => {
                        let repo = self
                            .active_repo
                            .clone()
                            .expect("the palette opens only with an active repository");
                        self.status = Status {
                            text: String::from("loading tags…"),
                            tone: Tone::Info,
                        };
                        vec![Request::Tags { repo }]
                    }
                    Ok(Command::Help) => {
                        self.status = Status {
                            text: commands::HELP.to_string(),
                            tone: Tone::Info,
                        };
                        Vec::new()
                    }
                    Err(message) => {
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    fn activate(&mut self) -> Vec<Request> {
        match self.focus {
            Pane::Repositories => self.activate_repository(),
            Pane::WorkingTree => self.open_selected_staging_diff(),
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
                if self.staging.is_some() || self.review.is_some() || self.confirmation.is_some() {
                    return Vec::new();
                }
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

    /// Open the conflict overlay on the active repository.
    ///
    /// Refused with a sentence when nothing is selected. The overlay reads and
    /// writes one repository, and there is no defensible default for which —
    /// picking one would be resolving conflicts in a repository the user never
    /// chose.
    fn open_conflicts(&mut self) -> Vec<Request> {
        let Some(repo) = self.active_repo.clone() else {
            self.status = Status {
                text: String::from("select a repository first — Enter on a row in Repositories"),
                tone: Tone::Error,
            };
            return Vec::new();
        };
        let requests = self.conflicts.open(repo);
        let fetches = self.dispatch_conflict(requests);
        self.refresh_conflicts_status();
        fetches
    }

    /// Give the overlay's requests the repository they act on.
    ///
    /// The pane names a path; which repository that path is in belongs to the
    /// shell, and keeping it here means the overlay cannot address one
    /// repository while the shell believes it is showing another.
    fn dispatch_conflict(&mut self, requests: Vec<conflicts::Request>) -> Vec<Request> {
        let Some(repo) = self.conflicts.repo().map(str::to_string) else {
            return Vec::new();
        };
        requests
            .into_iter()
            .map(|request| match request {
                conflicts::Request::Conflicts => Request::Conflicts { repo: repo.clone() },
                conflicts::Request::Stage { path, pane, oid } => Request::ConflictStage {
                    repo: repo.clone(),
                    path,
                    pane,
                    oid,
                },
                conflicts::Request::Result { path } => Request::ConflictResult {
                    repo: repo.clone(),
                    path,
                },
                conflicts::Request::Source { path } => Request::ConflictSource {
                    repo: repo.clone(),
                    path,
                },
                conflicts::Request::ResolveWholeFile { path, resolution } => {
                    Request::ResolveWholeFile {
                        repo: repo.clone(),
                        path,
                        resolution,
                    }
                }
                conflicts::Request::ResolveContent {
                    path,
                    expected_stages,
                    expected_source,
                    content,
                } => Request::ResolveContent {
                    repo: repo.clone(),
                    path,
                    expected_stages,
                    expected_source,
                    content,
                },
            })
            .collect()
    }

    fn refresh_conflicts_status(&mut self) {
        if !self.conflicts.is_open() {
            return;
        }
        let error = self.conflicts.message().is_some_and(|(_, error)| error);
        self.status = Status {
            text: self.conflicts_status_text(),
            tone: if error { Tone::Error } else { Tone::Info },
        };
    }

    /// The overlay's status line: whatever it last had to say, then the keys
    /// that are live on the screen it is showing.
    fn conflicts_status_text(&self) -> String {
        match self.conflicts.message() {
            Some((message, _)) => format!("{message} · {}", self.conflicts.hint()),
            None => self.conflicts.hint(),
        }
    }

    fn activate_repository(&mut self) -> Vec<Request> {
        let Some(repo) = self
            .catalog
            .get(self.cursor(Pane::Repositories))
            .map(|repo| repo.worktree.clone())
        else {
            return Vec::new();
        };
        if self.selection_in_flight.as_deref() == Some(repo.as_str()) {
            return Vec::new();
        }
        if self.active_repo.as_deref() != Some(repo.as_str()) {
            self.writable_repo = None;
            self.tags.clear();
            self.tags_loaded = false;
            self.commits.clear();
            self.edges.clear();
            self.stubs.clear();
            self.lane_count = 0;
            self.worktree.clear();
            self.staging = None;
            self.confirmation = None;
            self.review = None;
            self.cursors[Pane::Commits.index()] = 0;
            self.cursors[Pane::WorkingTree.index()] = 0;
            self.cursors[Pane::Main.index()] = 0;
            self.detail = DetailPane::default();
        }
        self.active_repo = Some(repo.clone());
        self.selection_in_flight = Some(repo.clone());
        self.focus = Pane::WorkingTree;
        self.status = Status {
            text: String::from("selecting repository…"),
            tone: Tone::Info,
        };
        vec![Request::Select { repo }]
    }

    fn open_detail(&mut self, repo: String, id: String) -> Vec<Request> {
        self.staging = None;
        self.confirmation = None;
        self.review = None;
        self.detail.open(repo.clone(), id.clone());
        self.cursors[Pane::Main.index()] = 0;
        self.focus = Pane::Main;
        self.status = Status {
            text: format!("loading {}…", short_id(&id)),
            tone: Tone::Info,
        };
        vec![
            Request::Commit {
                repo: repo.clone(),
                id: id.clone(),
            },
            Request::Diff { repo, id },
        ]
    }

    fn open_selected_staging_diff(&mut self) -> Vec<Request> {
        let Some(repo) = self.active_repo.clone() else {
            return Vec::new();
        };
        let Some(row) = self.worktree.row(self.cursor(Pane::WorkingTree)) else {
            return Vec::new();
        };
        let Some(direction) = row.file_direction else {
            self.status = Status {
                text: match row.section {
                    crate::panes::worktree::Section::Untracked => String::from(
                        "untracked files are not in the shared partial diff; press a to stage all",
                    ),
                    _ => String::from("that status row has no staging diff"),
                },
                tone: Tone::Error,
            };
            return Vec::new();
        };
        self.pending_file = None;
        self.status = Status {
            text: format!("loading {} diff…", direction_words(direction)),
            tone: Tone::Info,
        };
        vec![Request::StagingDiff { repo, direction }]
    }

    fn preview_selection(&mut self) -> Vec<Request> {
        if self.write_in_flight || self.review.is_some() || self.confirmation.is_some() {
            return Vec::new();
        }
        match self.focus {
            Pane::WorkingTree => {
                let Some(repo) = self.active_repo.clone() else {
                    return Vec::new();
                };
                let Some(row) = self.worktree.row(self.cursor(Pane::WorkingTree)) else {
                    return Vec::new();
                };
                let Some(direction) = row.file_direction else {
                    self.status = Status {
                        text: if row.section == crate::panes::worktree::Section::Untracked {
                            String::from(
                                "a single untracked file has no shared diff preview; press a to stage all",
                            )
                        } else {
                            String::from("that row cannot be staged or unstaged")
                        },
                        tone: Tone::Error,
                    };
                    return Vec::new();
                };
                self.pending_file = Some(PendingFile {
                    repo: repo.clone(),
                    path: row.path.clone(),
                    direction,
                });
                self.status = Status {
                    text: format!("building {} file preview…", direction_words(direction)),
                    tone: Tone::Info,
                };
                vec![Request::StagingDiff { repo, direction }]
            }
            Pane::Main => {
                let Some(staging) = self.staging.as_ref() else {
                    return Vec::new();
                };
                let Some(descriptor) = self.active_descriptor() else {
                    return Vec::new();
                };
                match staging.plan_for_row(
                    self.cursor(Pane::Main),
                    &descriptor.repository,
                    &descriptor.worktree,
                ) {
                    Ok(plan) => self.request_patch_preview(plan),
                    Err(message) => {
                        self.status = Status {
                            text: message,
                            tone: Tone::Error,
                        };
                        Vec::new()
                    }
                }
            }
            Pane::Repositories | Pane::Commits => Vec::new(),
        }
    }

    fn preview_file_from_loaded_diff(&mut self, path: String) -> Vec<Request> {
        let Some(staging) = self.staging.as_ref() else {
            return Vec::new();
        };
        let Some(descriptor) = self.active_descriptor() else {
            return Vec::new();
        };
        match staging.plan_for_file(&path, &descriptor.repository, &descriptor.worktree) {
            Ok(plan) => self.request_patch_preview(plan),
            Err(message) => {
                self.status = Status {
                    text: message,
                    tone: Tone::Error,
                };
                Vec::new()
            }
        }
    }

    fn request_patch_preview(&mut self, plan: PatchPlan) -> Vec<Request> {
        let Some(repo) = self.active_repo.clone() else {
            return Vec::new();
        };
        self.write_in_flight = true;
        self.execution_in_flight = false;
        self.status = Status {
            text: String::from("asking the server to preview the exact patch plan…"),
            tone: Tone::Info,
        };
        vec![Request::PreviewPatch { repo, plan }]
    }

    fn preview_whole_tree(&mut self) -> Vec<Request> {
        if self.focus != Pane::WorkingTree || self.write_in_flight || self.review.is_some() {
            return Vec::new();
        }
        let Some(row) = self.worktree.row(self.cursor(Pane::WorkingTree)) else {
            return Vec::new();
        };
        let Some(direction) = row.section.whole_direction() else {
            self.status = Status {
                text: String::from("that section has no whole-tree staging action"),
                tone: Tone::Error,
            };
            return Vec::new();
        };
        let operation = match direction {
            StageDirection::Stage => GitOperation::StageAll,
            StageDirection::Unstage => GitOperation::UnstageAll,
        };
        self.request_operation_plan(operation)
    }

    fn confirm_discard(&mut self) -> Vec<Request> {
        if self.focus != Pane::WorkingTree || self.write_in_flight || self.review.is_some() {
            return Vec::new();
        }
        let Some(row) = self.worktree.row(self.cursor(Pane::WorkingTree)) else {
            return Vec::new();
        };
        if !row.discardable {
            self.status = Status {
                text: String::from("discard is available only for unstaged tracked changes"),
                tone: Tone::Error,
            };
            return Vec::new();
        }
        let path = match WorktreePath::new(row.path.clone()) {
            Ok(path) => path,
            Err(error) => {
                self.status = Status {
                    text: error.to_string(),
                    tone: Tone::Error,
                };
                return Vec::new();
            }
        };
        self.confirmation = Some(Confirmation {
            prompt: format!(
                "Discard unstaged changes to {}? This permanently loses its uncommitted work. y confirm · n/Esc keep it",
                row.path
            ),
            operation: GitOperation::DiscardTrackedPaths { paths: vec![path] },
        });
        self.review = None;
        self.cursors[Pane::Main.index()] = 0;
        self.focus = Pane::Main;
        self.status = Status {
            text: String::from("destructive discard is guarded · y confirm · n/Esc keep it"),
            tone: Tone::Error,
        };
        Vec::new()
    }

    fn approve(&mut self) -> Vec<Request> {
        if self.write_in_flight {
            return Vec::new();
        }
        if let Some(confirmation) = self.confirmation.take() {
            return self.request_operation_plan(confirmation.operation);
        }
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        let Some(repo) = self.active_repo.clone() else {
            return Vec::new();
        };
        self.write_in_flight = true;
        self.execution_in_flight = true;
        self.status = Status {
            text: String::from("submitting the reviewed plan unchanged…"),
            tone: Tone::Info,
        };
        match review {
            Review::Operation(plan) => vec![Request::ExecutePlan {
                repo,
                plan: plan.clone(),
            }],
            Review::Patch { plan, .. } => vec![Request::ApplyPatch {
                repo,
                plan: plan.clone(),
            }],
        }
    }

    fn request_operation_plan(&mut self, operation: GitOperation) -> Vec<Request> {
        let Some(repo) = self.active_repo.clone() else {
            return Vec::new();
        };
        self.write_in_flight = true;
        self.execution_in_flight = false;
        self.status = Status {
            text: String::from("asking the server to build a reviewable plan…"),
            tone: Tone::Info,
        };
        vec![Request::BuildPlan { repo, operation }]
    }

    fn request_status(&mut self) -> Vec<Request> {
        let Some(repo) = self.active_repo.clone() else {
            return Vec::new();
        };
        if self.status_in_flight.as_deref() == Some(repo.as_str()) {
            return Vec::new();
        }
        self.status_in_flight = Some(repo.clone());
        self.worktree.begin_load();
        vec![Request::Status { repo }]
    }

    fn active_descriptor(&self) -> Option<&RepositoryDescriptor> {
        let active = self.active_repo.as_deref()?;
        self.catalog.iter().find(|repo| repo.worktree == active)
    }

    fn main_rows(&self) -> usize {
        if let Some(review) = &self.review {
            review_lines(review).len()
        } else if let Some(confirmation) = &self.confirmation {
            confirmation.prompt.lines().count().max(1)
        } else if let Some(staging) = &self.staging {
            staging.rows().len()
        } else {
            self.detail.row_count()
        }
    }

    /// Ask for the catalog unless a catalog read is already out — a held
    /// `r` must not queue fifty reads behind a slow server.
    fn request_catalog(&mut self) -> Vec<Request> {
        if self.in_flight > 0 {
            return Vec::new();
        }
        self.in_flight += 1;
        vec![Request::Catalog]
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

    /// Record what the frame just drawn could show (#625). The event loop
    /// calls this after every draw, so by the time a key is read the numbers
    /// describe the frame the user is looking at — including a terminal that
    /// was resized and a pane that was just zoomed.
    pub fn observe(&mut self, viewport: Viewport) {
        self.viewport = viewport;
        self.conflicts.observe(viewport.overlay());
    }

    /// The pane the zoom key has maximized, if any.
    ///
    /// Zoom follows focus rather than pinning the pane it was pressed on:
    /// `z` and then Tab shows the next pane full-height, which is what
    /// somebody who wants a big look at something else means. It also means
    /// there is no stale "zoomed pane" to reconcile when focus moves.
    pub fn maximized(&self) -> Option<Pane> {
        self.maximized.then_some(self.focus)
    }

    /// One page of a pane, in the units that pane's cursor counts.
    ///
    /// Never zero: a pane squeezed to no interior still moves by one row, so
    /// `PageDown` degrades to `j` rather than silently doing nothing.
    fn page(&self, pane: Pane) -> usize {
        self.viewport.rows(pane).max(1)
    }

    /// Move a pane's cursor by `delta`, clamped to the rows it actually has.
    fn move_cursor_by(&mut self, pane: Pane, delta: isize) {
        let last = self.rows(pane).saturating_sub(1);
        let slot = &mut self.cursors[pane.index()];
        *slot = slot.saturating_add_signed(delta).min(last);
    }

    /// How many rows a pane has to select among.
    pub fn rows(&self, pane: Pane) -> usize {
        match pane {
            Pane::Repositories => self.catalog.len(),
            Pane::WorkingTree => self.worktree.rows().len(),
            Pane::Commits => self.commits.len(),
            Pane::Main => self.main_rows(),
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

pub fn review_lines(review: &Review) -> Vec<String> {
    match review {
        Review::Operation(plan) => {
            let mut lines = vec![String::from("SERVER PLAN — review before execution")];
            let json = serde_json::to_string_pretty(plan)
                .unwrap_or_else(|error| format!("could not render plan: {error}"));
            lines.extend(json.lines().map(str::to_string));
            lines.push(String::from("y approve unchanged · n/Esc refuse"));
            lines
        }
        Review::Patch { plan, preview } => {
            let mut lines = vec![String::from("PATCH PLAN — review before execution")];
            let json = serde_json::to_string_pretty(plan)
                .unwrap_or_else(|error| format!("could not render patch plan: {error}"));
            lines.extend(json.lines().map(str::to_string));
            if !preview.whole_files.is_empty() {
                lines.push(format!("whole files: {}", preview.whole_files.join(", ")));
            }
            lines.push(String::from("exact patch:"));
            if preview.patch.is_empty() {
                lines.push(String::from("(no hunk bytes; whole-file pathspecs above)"));
            } else {
                lines.extend(preview.patch.lines().map(str::to_string));
            }
            lines.push(String::from("y approve unchanged · n/Esc refuse"));
            lines
        }
    }
}

fn direction_words(direction: StageDirection) -> &'static str {
    match direction {
        StageDirection::Stage => "stage (worktree → index)",
        StageDirection::Unstage => "unstage (index → HEAD)",
    }
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(7)]
}

fn operation_progress_line(operation: &OperationStatus) -> String {
    if let Some(progress) = operation.progress {
        let phase = match progress.phase {
            git_vista_protocol::TransferPhase::Enumerating => "enumerating",
            git_vista_protocol::TransferPhase::Counting => "counting",
            git_vista_protocol::TransferPhase::Compressing => "compressing",
            git_vista_protocol::TransferPhase::Receiving => "receiving",
            git_vista_protocol::TransferPhase::Writing => "writing",
            git_vista_protocol::TransferPhase::Resolving => "resolving",
        };
        let cancel = if cancellable_operation(&operation.operation) {
            " · c cancel"
        } else {
            ""
        };
        return progress.percent.map_or_else(
            || format!("transfer {phase}{cancel}"),
            |percent| format!("transfer {phase} {percent}%{cancel}"),
        );
    }
    let stage = match operation.stage {
        git_vista_protocol::OperationStage::Queued => "queued",
        git_vista_protocol::OperationStage::Planning => "planning",
        git_vista_protocol::OperationStage::Waiting => "waiting for repository",
        git_vista_protocol::OperationStage::Checking => "checking reviewed plan",
        git_vista_protocol::OperationStage::Executing => "executing",
        git_vista_protocol::OperationStage::Finished => "finished",
    };
    format!("operation {stage}")
}

#[cfg(test)]
mod tests {
    use git_vista_core::model::{CommitSummary, Oid};
    use git_vista_protocol::{
        ChangeKind, ChangeSides, GenerationToken, OperationHash, OperationId, OperationStage,
        OperationState, OperationStatus, Plan, Precondition, RecoveryStrategy, RemoteName,
        RepositoryToken, RiskLevel, SelectionShape, StatusEntry, TransferPhase, TransferProgress,
        UnixSeconds, WorktreeToken,
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

    /// A catalog of `n` entries, for the paging tests: they need more rows
    /// than a page so that a clamp at the end cannot be mistaken for a page.
    fn long_catalog(n: usize) -> Vec<RepositoryDescriptor> {
        let entries: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"repository":"r{i}","worktree":"w{i}","name":"repo-{i}","kind":"bare","read_only":true}}"#
                )
            })
            .collect();
        catalog(&format!("[{}]", entries.join(",")))
    }

    fn loaded(wire: &str) -> App {
        let mut app = App::new();
        assert_eq!(app.start(), [Request::Catalog]);
        app.receive(Data::Catalog(Ok(catalog(wire))));
        app
    }

    fn select_first(app: &mut App) {
        assert_eq!(
            app.apply(Action::Activate),
            [Request::Select {
                repo: "w1".to_string()
            }]
        );
        assert_eq!(
            app.receive(Data::Selected {
                repo: "w1".to_string(),
                result: Ok(()),
            }),
            [
                Request::History {
                    repo: "w1".to_string()
                },
                Request::Status {
                    repo: "w1".to_string()
                }
            ]
        );
    }

    fn plan_wire() -> Vec<u8> {
        plan_wire_for(GitOperation::StageAll)
    }

    fn plan_wire_for(operation: GitOperation) -> Vec<u8> {
        serde_json::to_vec(&Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("worktree-1").unwrap(),
            generation: GenerationToken::new("generation-reviewed").unwrap(),
            operation,
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

    fn running_transfer(id: &OperationId, operation: GitOperation) -> OperationStatus {
        OperationStatus {
            id: id.clone(),
            state: OperationState::Running,
            stage: OperationStage::Executing,
            operation,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("worktree-1").unwrap(),
            accepted_at: UnixSeconds(1_788_365_000),
            ended_at: None,
            status: None,
            message: None,
            generation: None,
            recovery: None,
            recovers: None,
            progress: Some(TransferProgress {
                phase: TransferPhase::Receiving,
                percent: Some(42),
                objects: Some(42),
                total_objects: Some(100),
            }),
        }
    }

    /// INVARIANT (#625): a page is what the LAST FRAME said that pane could
    /// show — per pane, in that pane's own units, and re-read every time.
    ///
    /// Observed twice at two different sizes on purpose. One observation
    /// proves only that some number was used; two prove the number is the
    /// current one. A page size captured once at startup, or fixed at a
    /// constant, passes the first half of this test and fails the second —
    /// and that is the bug this slice exists not to ship, because it would
    /// be invisible until somebody zoomed a pane.
    ///
    /// MUTATION 1 (remove): move one row, ignoring the viewport.
    /// MUTATION 2 (weaken): use a constant page size.
    #[test]
    fn a_page_is_the_observed_height_of_the_focused_pane_and_is_re_read_every_time() {
        let mut app = App::new();
        app.receive(Data::Catalog(Ok(long_catalog(400))));

        app.observe(Viewport::new([6, 4, 3, 21], 0));
        app.apply(Action::CursorPageDown);
        assert_eq!(app.cursor(Pane::Repositories), 6);
        app.apply(Action::CursorPageDown);
        assert_eq!(app.cursor(Pane::Repositories), 12);
        app.apply(Action::CursorPageUp);
        assert_eq!(app.cursor(Pane::Repositories), 6);

        // The terminal grew, or the pane was zoomed. The next page is the
        // new height, not the one that was true a frame ago.
        app.observe(Viewport::new([18, 16, 9, 57], 0));
        app.apply(Action::CursorPageDown);
        assert_eq!(
            app.cursor(Pane::Repositories),
            24,
            "the page was still measured against the old, smaller frame"
        );
    }

    /// INVARIANT (#625): each pane pages by ITS OWN height, so moving focus
    /// changes how far a page goes. One shared page size would be right for
    /// at most one pane of four.
    #[test]
    fn each_pane_pages_by_its_own_measured_height() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[
                ("a".repeat(40).as_str(), "first"),
                ("b".repeat(40).as_str(), "second"),
                ("c".repeat(40).as_str(), "third"),
                ("d".repeat(40).as_str(), "fourth"),
            ])),
        });
        app.observe(Viewport::new([6, 4, 3, 21], 0));

        app.apply(Action::Focus(Pane::Commits));
        app.apply(Action::CursorPageDown);
        assert_eq!(
            app.cursor(Pane::Commits),
            3,
            "the graph pages by commits, and three fit"
        );

        app.apply(Action::Focus(Pane::Repositories));
        app.apply(Action::CursorPageDown);
        assert_eq!(
            app.cursor(Pane::Repositories),
            2,
            "three repositories, so a six-row page stops at the last one"
        );
    }

    #[test]
    fn paging_stops_at_both_ends_and_home_and_end_reach_them_directly() {
        let mut app = App::new();
        app.receive(Data::Catalog(Ok(long_catalog(20))));
        app.observe(Viewport::new([6, 4, 3, 21], 0));

        app.apply(Action::CursorPageUp);
        assert_eq!(app.cursor(Pane::Repositories), 0, "no wrap off the top");

        for _ in 0..10 {
            app.apply(Action::CursorPageDown);
        }
        assert_eq!(
            app.cursor(Pane::Repositories),
            19,
            "paging past the end must stop on the last row, not run off it"
        );

        app.apply(Action::CursorTop);
        assert_eq!(app.cursor(Pane::Repositories), 0);
        app.apply(Action::CursorBottom);
        assert_eq!(app.cursor(Pane::Repositories), 19);
    }

    /// An empty pane, and a pane too short to hold a row, are both real: the
    /// catalog is empty before the first answer, and a pane can be squeezed
    /// to nothing by a small terminal. Neither may panic, and neither may
    /// leave the cursor pointing at a row that is not there.
    #[test]
    fn paging_an_empty_or_squeezed_pane_is_safe_and_still_moves_by_one() {
        let mut empty = App::new();
        empty.observe(Viewport::new([6, 4, 3, 21], 0));
        for action in [
            Action::CursorPageDown,
            Action::CursorPageUp,
            Action::CursorTop,
            Action::CursorBottom,
        ] {
            empty.apply(action);
            assert_eq!(empty.cursor(Pane::Repositories), 0, "{action:?}");
        }

        // A frame so short the pane has no interior at all. `PageDown` then
        // behaves like `j` rather than doing nothing — a key that is silently
        // inert is worse than one that under-delivers.
        let mut squeezed = App::new();
        squeezed.receive(Data::Catalog(Ok(long_catalog(20))));
        squeezed.observe(Viewport::new([0, 0, 0, 0], 0));
        squeezed.apply(Action::CursorPageDown);
        assert_eq!(squeezed.cursor(Pane::Repositories), 1);
    }

    /// INVARIANT (#625): zoom is a toggle on whatever has focus, and it
    /// follows focus rather than pinning the pane it was pressed on.
    #[test]
    fn the_zoom_toggle_names_the_focused_pane_and_follows_focus() {
        let mut app = App::new();
        assert_eq!(app.maximized(), None, "the shell opens with four panes");

        app.apply(Action::ToggleMaximize);
        assert_eq!(app.maximized(), Some(Pane::Repositories));

        app.apply(Action::FocusNext);
        assert_eq!(
            app.maximized(),
            Some(Pane::WorkingTree),
            "zoom follows focus; a stale zoomed pane would need reconciling"
        );

        app.apply(Action::ToggleMaximize);
        assert_eq!(app.maximized(), None, "the same key puts the shape back");
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

    fn status(entries: Vec<StatusEntry>) -> WorktreeStatus {
        WorktreeStatus {
            generation: GenerationToken::new("status-v1:test").unwrap(),
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            ahead: 0,
            behind: 0,
            entries,
        }
    }

    fn ready_status(app: &mut App, entries: Vec<StatusEntry>) {
        app.receive(Data::Status {
            repo: "w1".to_string(),
            result: Ok(status(entries)),
        });
    }

    fn changed(path: &str, sides: ChangeSides) -> StatusEntry {
        StatusEntry::Changed {
            path: path.to_string(),
            sides,
            submodule: None,
            binary: false,
        }
    }

    fn plan(operation: GitOperation) -> Plan {
        Plan {
            repository: RepositoryToken::new("r1").unwrap(),
            worktree: WorktreeToken::new("w1").unwrap(),
            generation: GenerationToken::new("status-v1:test").unwrap(),
            operation,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(100),
            expires_at: UnixSeconds(200),
            risk: RiskLevel::Safe,
            preconditions: vec![Precondition::CleanWorktree],
            expected_ref_changes: Vec::new(),
            advisories: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
        }
    }

    fn staging_diff(direction: StageDirection) -> (StageDirection, StagingDiff) {
        (
            direction,
            StagingDiff {
                generation: GenerationToken::new("diff-v1:test").unwrap(),
                patch: "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n"
                    .to_string(),
                truncated: false,
            },
        )
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
        assert_eq!(app.start(), [Request::Catalog]);
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
                Pane::WorkingTree,
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
        app.apply(Action::Focus(Pane::WorkingTree));
        app.apply(Action::CursorDown);
        app.apply(Action::CursorDown);
        assert_eq!(app.cursor(Pane::WorkingTree), 0);
        app.apply(Action::CursorUp);
        assert_eq!(app.cursor(Pane::WorkingTree), 0);
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

        assert_eq!(app.apply(Action::Refresh), [Request::Catalog]);
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
        assert_eq!(app.start(), [Request::Catalog]);
        assert!(
            app.apply(Action::Refresh).is_empty(),
            "coalesced: one read is already out"
        );
        assert!(app.apply(Action::Refresh).is_empty());
        assert_eq!(app.in_flight, 1);
        app.receive(Data::Catalog(Ok(Vec::new())));
        assert_eq!(
            app.apply(Action::Refresh),
            [Request::Catalog],
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

    /// INVARIANT: selecting a repository activates the server session before
    /// any selection-scoped read is dispatched.
    ///
    /// MUTATION 1 (remove): replace the Select request with an immediate Status read.
    /// MUTATION 2 (weaken): dispatch History but omit Status after selection.
    #[test]
    fn activating_the_selected_repository_selects_it_before_scoped_reads() {
        let mut app = loaded(THREE);
        app.apply(Action::CursorDown);

        assert_eq!(
            app.apply(Action::Activate),
            [Request::Select {
                repo: "w2".to_string()
            }]
        );
        assert_eq!(app.active_repo.as_deref(), Some("w2"));
        assert_eq!(app.focus, Pane::WorkingTree);
        assert_eq!(
            app.receive(Data::Selected {
                repo: "w2".to_string(),
                result: Ok(()),
            }),
            [
                Request::History {
                    repo: "w2".to_string()
                },
                Request::Status {
                    repo: "w2".to_string()
                }
            ],
            "session selection must complete before any selection-scoped read"
        );
    }

    #[test]
    fn writable_selection_precedes_a_closed_command_and_shared_plan_review() {
        let mut app = loaded(THREE);
        let requests = app.apply(Action::Activate);
        assert!(matches!(
            requests.as_slice(),
            [Request::Select { repo }] if repo == "w1"
        ));

        assert!(app.apply(Action::OpenCommand).is_empty());
        assert!(
            app.command_input.is_none(),
            "selection was not acknowledged"
        );
        app.receive(Data::Selected {
            repo: "w1".to_string(),
            result: Ok(()),
        });
        app.apply(Action::OpenCommand);
        assert_eq!(app.command_input.as_deref(), Some(""));
        for character in "branch delete topic".chars() {
            app.apply(Action::CommandChar(character));
        }
        assert_eq!(
            app.apply(Action::SubmitCommand),
            [Request::BuildPlanWire(GitOperation::DeleteBranch {
                branch: git_vista_protocol::BranchName::new("topic").unwrap(),
            })]
        );
        assert!(app.command_input.is_none());

        app.receive(Data::PlanReady(Ok(plan_wire())));
        assert!(app.plan_review.is_some());
        assert!(app.apply(Action::FocusNext).is_empty());
        assert!(matches!(
            app.apply(Action::ApprovePlan).as_slice(),
            [
                Request::ExecuteReviewedPlan(_),
                Request::OperationByKey { .. }
            ]
        ));
    }

    #[test]
    fn malformed_commands_and_palette_cancellation_send_nothing() {
        let mut app = loaded(THREE);
        app.apply(Action::Activate);
        app.receive(Data::Selected {
            repo: "w1".to_string(),
            result: Ok(()),
        });

        app.apply(Action::OpenCommand);
        for character in "push main origin --force".chars() {
            app.apply(Action::CommandChar(character));
        }
        assert!(app.apply(Action::SubmitCommand).is_empty());
        assert!(app.status.text.contains("unknown push flag"));

        app.apply(Action::OpenCommand);
        app.apply(Action::CommandChar('x'));
        app.apply(Action::CommandBackspace);
        assert_eq!(app.command_input.as_deref(), Some(""));
        assert!(app.apply(Action::RefusePlan).is_empty());
        assert!(app.command_input.is_none());
        assert!(app.status.text.contains("nothing was sent"));
    }

    #[test]
    fn a_history_answer_populates_commits_and_enter_requests_detail_and_diff() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "first"), (COMMIT_2, "second")])),
        });
        assert_eq!(app.rows(Pane::Commits), 2);
        app.apply(Action::Focus(Pane::Commits));
        app.apply(Action::CursorDown);

        assert_eq!(
            app.apply(Action::Activate),
            [
                Request::Commit {
                    repo: "w1".to_string(),
                    id: COMMIT_2.to_string(),
                },
                Request::Diff {
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
        select_first(&mut app);
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
        select_first(&mut app);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "merge")])),
        });
        app.apply(Action::Focus(Pane::Commits));
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
                Request::Commit {
                    repo: "w1".to_string(),
                    id: PARENT_2.to_string(),
                },
                Request::Diff {
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
        select_first(&mut app);
        app.receive(Data::History {
            repo: "w1".to_string(),
            result: Ok(page(&[(COMMIT_1, "long")])),
        });
        app.apply(Action::Focus(Pane::Commits));
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

    /// INVARIANT: whole-tree staging and unstaging submit only a server-built
    /// Plan, unchanged, after it has been visible and explicitly approved.
    ///
    /// MUTATION 1 (remove): discard the server Plan instead of storing Review.
    /// MUTATION 2 (weaken): map the unstage direction to StageAll too.
    #[test]
    fn whole_tree_stage_and_unstage_wait_for_and_submit_the_exact_reviewed_plan() {
        for (sides, expected) in [
            (
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
                GitOperation::StageAll,
            ),
            (
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Modified,
                },
                GitOperation::UnstageAll,
            ),
        ] {
            let mut app = loaded(THREE);
            select_first(&mut app);
            ready_status(&mut app, vec![changed("a.txt", sides)]);

            assert_eq!(
                app.apply(Action::PreviewWholeTree),
                [Request::BuildPlan {
                    repo: "w1".to_string(),
                    operation: expected.clone(),
                }]
            );
            assert!(
                app.apply(Action::Approve).is_empty(),
                "approval before the server plan arrives cannot write"
            );

            let reviewed = plan(expected);
            assert!(app
                .receive(Data::Plan {
                    repo: "w1".to_string(),
                    result: Ok(reviewed.clone()),
                })
                .is_empty());
            assert_eq!(
                app.review,
                Some(Review::Operation(Box::new(reviewed.clone())))
            );
            assert_eq!(
                app.apply(Action::Approve),
                [Request::ExecutePlan {
                    repo: "w1".to_string(),
                    plan: Box::new(reviewed.clone()),
                }],
                "the visible Plan is the exact value submitted"
            );
            assert!(app.apply(Action::Cancel).is_empty());
            assert_eq!(app.review, Some(Review::Operation(Box::new(reviewed))));
            assert!(app.status.text.contains("already submitted"));
        }
    }

    /// INVARIANT: destructive discard has both a path-specific loss warning
    /// and the ordinary full-Plan review; neither confirmation itself writes.
    ///
    /// MUTATION 1 (remove): invert the discardable-path eligibility guard.
    /// MUTATION 2 (weaken): omit the permanent/uncommitted-loss wording.
    #[test]
    fn discard_requires_loss_confirmation_then_exact_plan_review() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        ready_status(
            &mut app,
            vec![changed(
                "precious.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            )],
        );

        assert!(app.apply(Action::Discard).is_empty());
        let warning = &app.confirmation.as_ref().unwrap().prompt;
        assert!(warning.contains("precious.txt"), "{warning}");
        assert!(warning.contains("permanently"), "{warning}");
        assert!(warning.contains("uncommitted"), "{warning}");

        let expected = GitOperation::DiscardTrackedPaths {
            paths: vec![WorktreePath::new("precious.txt").unwrap()],
        };
        assert_eq!(
            app.apply(Action::Approve),
            [Request::BuildPlan {
                repo: "w1".to_string(),
                operation: expected.clone(),
            }]
        );
        assert!(app.apply(Action::Approve).is_empty());

        let reviewed = plan(expected);
        app.receive(Data::Plan {
            repo: "w1".to_string(),
            result: Ok(reviewed.clone()),
        });
        assert_eq!(
            app.apply(Action::Approve),
            [Request::ExecutePlan {
                repo: "w1".to_string(),
                plan: Box::new(reviewed),
            }]
        );
    }

    /// INVARIANT: a file shortcut is resolved against the pinned shared diff,
    /// previewed by the server, then applies the exact reviewed PatchPlan.
    ///
    /// MUTATION 1 (remove): drop the pending file instead of requesting preview.
    /// MUTATION 2 (weaken): coerce the file shortcut into the first hunk.
    #[test]
    fn file_staging_uses_pinned_diff_preview_and_applies_the_exact_reviewed_patch_plan() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        ready_status(
            &mut app,
            vec![changed(
                "a.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            )],
        );

        assert_eq!(
            app.apply(Action::PreviewSelection),
            [Request::StagingDiff {
                repo: "w1".to_string(),
                direction: StageDirection::Stage,
            }]
        );
        let (direction, diff) = staging_diff(StageDirection::Stage);
        let requests = app.receive(Data::StagingDiff {
            repo: "w1".to_string(),
            direction,
            result: Ok(diff),
        });
        let [Request::PreviewPatch {
            repo,
            plan: submitted,
        }] = requests.as_slice()
        else {
            panic!("file shortcut did not request one patch preview: {requests:?}");
        };
        assert_eq!(repo, "w1");
        assert_eq!(submitted.generation.as_str(), "diff-v1:test");
        assert!(matches!(
            submitted.files[0].selection,
            SelectionShape::EntireFile
        ));
        assert!(app.apply(Action::Approve).is_empty());

        let reviewed = submitted.clone();
        app.receive(Data::PatchPreview {
            repo: "w1".to_string(),
            plan: reviewed.clone(),
            result: Ok(PatchPreview {
                generation: reviewed.generation.clone(),
                patch: String::new(),
                whole_files: vec!["a.txt".to_string()],
            }),
        });
        assert_eq!(
            app.apply(Action::Approve),
            [Request::ApplyPatch {
                repo: "w1".to_string(),
                plan: reviewed,
            }]
        );
    }

    /// INVARIANT: refusal invalidates an outstanding preview and late network
    /// answers cannot reopen an approval surface or become executable.
    ///
    /// MUTATION 1 (remove): do not clear `write_in_flight` on Cancel.
    /// MUTATION 2 (weaken): accept Plan/PatchPreview without the pending gate.
    #[test]
    fn cancellation_makes_late_plan_and_patch_preview_answers_inert() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        ready_status(
            &mut app,
            vec![changed(
                "a.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            )],
        );

        assert!(matches!(
            app.apply(Action::PreviewWholeTree).as_slice(),
            [Request::BuildPlan { .. }]
        ));
        app.apply(Action::Cancel);
        app.receive(Data::Plan {
            repo: "w1".to_string(),
            result: Ok(plan(GitOperation::StageAll)),
        });
        assert!(app.review.is_none());
        assert!(app.apply(Action::Approve).is_empty());

        let direction = StageDirection::Stage;
        let (_, diff) = staging_diff(direction);
        app.receive(Data::StagingDiff {
            repo: "w1".to_string(),
            direction,
            result: Ok(diff),
        });
        let preview_requests = app.apply(Action::PreviewSelection);
        let [Request::PreviewPatch { plan, .. }] = preview_requests.as_slice() else {
            panic!("selected staging row did not request a preview");
        };
        let submitted = plan.clone();
        app.apply(Action::Cancel);
        app.receive(Data::PatchPreview {
            repo: "w1".to_string(),
            plan: submitted.clone(),
            result: Ok(PatchPreview {
                generation: submitted.generation.clone(),
                patch: "exact".to_string(),
                whole_files: Vec::new(),
            }),
        });
        assert!(app.review.is_none());
        assert!(app.apply(Action::Approve).is_empty());
    }

    /// INVARIANT: cancelling a Working Tree file shortcut invalidates the
    /// pending file before its staging diff can answer.
    ///
    /// MUTATION 1 (remove): retain `pending_file` in the Cancel arm.
    /// MUTATION 2 (invert): leave the reducer busy after cancellation.
    #[test]
    fn cancelling_a_pending_file_makes_its_late_staging_diff_non_executable() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        ready_status(
            &mut app,
            vec![changed(
                "a.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            )],
        );

        assert!(matches!(
            app.apply(Action::PreviewSelection).as_slice(),
            [Request::StagingDiff { .. }]
        ));
        app.apply(Action::Cancel);
        assert!(
            app.pending_file.is_none(),
            "cancel retained the file intent"
        );
        assert!(!app.write_in_flight, "cancel left the reducer busy");
        let (direction, diff) = staging_diff(StageDirection::Stage);
        assert!(
            app.receive(Data::StagingDiff {
                repo: "w1".to_string(),
                direction,
                result: Ok(diff),
            })
            .is_empty(),
            "a cancelled file intent escaped as a late PatchPreview request"
        );
        assert!(app.review.is_none());
        assert!(!app.write_in_flight);
        assert!(app.apply(Action::Approve).is_empty());
    }

    /// INVARIANT: Space on an untracked row never invents a partial plan;
    /// only the separately reviewed StageAll path can include that file.
    ///
    /// MUTATION 1 (remove): give the untracked row a file direction.
    /// MUTATION 2 (weaken): downgrade the actionable refusal to Info.
    #[test]
    fn space_on_untracked_refuses_partial_staging_and_points_to_stage_all() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        ready_status(
            &mut app,
            vec![StatusEntry::Untracked {
                path: "new.txt".to_string(),
                binary: false,
            }],
        );

        assert!(app.apply(Action::PreviewSelection).is_empty());
        assert_eq!(app.status.tone, Tone::Error);
        assert!(app.status.text.contains("untracked"), "{}", app.status.text);
        assert!(app.status.text.contains("stage all"), "{}", app.status.text);
        assert!(app.pending_file.is_none());
        assert!(!app.write_in_flight);
        assert!(app.review.is_none());
    }

    /// INVARIANT: completion of an already-submitted write releases the
    /// reducer even if the user selected another repository while it ran.
    ///
    /// MUTATION 1 (remove): move busy-state cleanup below the active-repo
    /// stale-answer guard.
    /// MUTATION 2 (weaken): clear `execution_in_flight` but leave the shared
    /// `write_in_flight` gate set.
    #[test]
    fn old_repo_write_completion_cannot_freeze_writes_after_a_repo_switch() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        ready_status(
            &mut app,
            vec![changed(
                "a.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            )],
        );
        app.apply(Action::PreviewWholeTree);
        app.receive(Data::Plan {
            repo: "w1".to_string(),
            result: Ok(plan(GitOperation::StageAll)),
        });
        assert!(matches!(
            app.apply(Action::Approve).as_slice(),
            [Request::ExecutePlan { .. }]
        ));

        app.apply(Action::Focus(Pane::Repositories));
        app.apply(Action::CursorDown);
        assert_eq!(
            app.apply(Action::Activate),
            [Request::Select {
                repo: "w2".to_string()
            }]
        );
        assert_eq!(app.active_repo.as_deref(), Some("w2"));

        assert!(app
            .receive(Data::Written {
                repo: "w1".to_string(),
                result: Ok(String::from("staged")),
            })
            .is_empty());
        assert!(
            !app.execution_in_flight,
            "old completion left execution pinned"
        );
        assert!(
            !app.write_in_flight,
            "old completion left preview/write gate pinned"
        );

        app.receive(Data::Selected {
            repo: "w2".to_string(),
            result: Ok(()),
        });
        app.receive(Data::Status {
            repo: "w2".to_string(),
            result: Ok(status(vec![changed(
                "b.txt",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            )])),
        });
        assert!(matches!(
            app.apply(Action::PreviewWholeTree).as_slice(),
            [Request::BuildPlan { repo, .. }] if repo == "w2"
        ));
    }

    /// INVARIANT: the visible review includes every server Plan field and the
    /// exact patch preview, not a lossy client-side summary.
    ///
    /// MUTATION 1 (remove): omit the serialized Plan/PatchPlan body.
    /// MUTATION 2 (weaken): display only patch length or whole-file count.
    #[test]
    fn review_lines_show_the_complete_plan_and_exact_patch_bytes() {
        let operation = plan(GitOperation::StageAll);
        let operation_lines = review_lines(&Review::Operation(Box::new(operation))).join("\n");
        for field in [
            "generation",
            "operation_hash",
            "issued_at",
            "expires_at",
            "risk",
            "preconditions",
            "expected_ref_changes",
            "advisories",
            "recovery",
        ] {
            assert!(
                operation_lines.contains(field),
                "missing {field}: {operation_lines}"
            );
        }

        let pane = StagingPane::new(StageDirection::Stage, staging_diff(StageDirection::Stage).1);
        let patch_plan = pane.plan_for_row(2, "r1", "w1").unwrap();
        let exact = "diff --git a/a.txt b/a.txt\n-old\n+new\n";
        let patch_lines = review_lines(&Review::Patch {
            plan: patch_plan.clone(),
            preview: PatchPreview {
                generation: patch_plan.generation,
                patch: exact.to_string(),
                whole_files: vec!["whole.bin".to_string()],
            },
        })
        .join("\n");
        assert!(patch_lines.contains("whole.bin"), "{patch_lines}");
        assert!(patch_lines.contains(exact.trim()), "{patch_lines}");
        assert!(
            patch_lines.contains("\"select\": \"lines\""),
            "{patch_lines}"
        );
    }

    /// INVARIANT: a preview cannot be approved under a generation other than
    /// the exact PatchPlan generation the terminal submitted.
    ///
    /// MUTATION 1 (remove): delete the generation comparison.
    /// MUTATION 2 (weaken): compare only the `diff-v1:` prefix.
    #[test]
    fn mismatched_patch_preview_generation_is_never_reviewable() {
        let mut app = loaded(THREE);
        select_first(&mut app);
        let pane = StagingPane::new(StageDirection::Stage, staging_diff(StageDirection::Stage).1);
        let submitted = pane.plan_for_row(0, "r1", "w1").unwrap();
        app.request_patch_preview(submitted.clone());

        app.receive(Data::PatchPreview {
            repo: "w1".to_string(),
            plan: submitted,
            result: Ok(PatchPreview {
                generation: GenerationToken::new("diff-v1:different").unwrap(),
                patch: String::new(),
                whole_files: vec!["a.txt".to_string()],
            }),
        });
        assert!(app.review.is_none());
        assert_eq!(app.status.tone, Tone::Error);
        assert!(app.status.text.contains("generation did not match"));
        assert!(app.apply(Action::Approve).is_empty());
    }

    #[test]
    fn immutable_plan_review_blocks_staging_and_submits_the_exact_wire() {
        let mut app = loaded(THREE);
        let wire = format!(
            " {}\n",
            serde_json::to_string(&plan(GitOperation::StageAll)).unwrap()
        )
        .into_bytes();
        app.receive(Data::PlanReady(Ok(wire.clone())));

        assert!(app.plan_review.is_some());
        let focus = app.focus;
        assert!(app.apply(Action::FocusNext).is_empty());
        assert_eq!(app.focus, focus, "the modal leaked navigation to the shell");
        assert!(app.apply(Action::PreviewWholeTree).is_empty());

        let requests = app.apply(Action::ApprovePlan);
        let [Request::ExecuteReviewedPlan(approval), Request::OperationByKey { key }] =
            requests.as_slice()
        else {
            panic!("approval did not produce one execution and one lookup: {requests:?}");
        };
        assert_eq!(approval.body(), wire);
        assert_eq!(key, approval.key());
        assert!(
            app.apply(Action::Approve).is_empty(),
            "double approval escaped"
        );
        assert!(app.apply(Action::Cancel).is_empty());
        assert!(
            app.plan_review.is_some(),
            "an in-flight approval was dismissed"
        );
    }

    #[test]
    fn a_successful_approval_closes_the_modal_and_surfaces_the_receipt() {
        let mut app = loaded(THREE);
        app.receive(Data::PlanReady(Ok(plan_wire())));
        assert_eq!(app.apply(Action::ApprovePlan).len(), 2);
        app.receive(Data::PlanSubmitted(SubmissionOutcome::Executed(
            "Staged all changes.".to_string(),
        )));
        assert!(app.plan_review.is_none());
        assert_eq!(app.status.text, "Staged all changes.");
        assert_eq!(app.status.tone, Tone::Info);
        // This test has no active repository, so the refresh marker drains
        // without fabricating a target.
        assert!(app.tick().is_empty());
    }

    #[test]
    fn remote_execution_lookup_drives_typed_progress_and_cancellation() {
        let operation = GitOperation::FetchRemote {
            remote: RemoteName::new("origin").unwrap(),
        };
        let mut app = loaded(THREE);
        app.receive(Data::PlanReady(Ok(plan_wire_for(operation.clone()))));
        let requests = app.apply(Action::ApprovePlan);
        let [Request::ExecuteReviewedPlan(_), Request::OperationByKey { key }] =
            requests.as_slice()
        else {
            panic!("approval did not start execution plus lookup: {requests:?}");
        };
        let key = key.clone();
        let id = OperationId::new("op_0123456789abcdef").unwrap();
        app.receive(Data::OperationByKey {
            key,
            result: Ok(Some(id.clone())),
        });
        assert_eq!(app.tick(), [Request::OperationStatus { id: id.clone() }]);
        app.receive(Data::OperationStatus(Ok(running_transfer(&id, operation))));
        assert!(
            app.status.text.contains("receiving 42%"),
            "{}",
            app.status.text
        );

        assert_eq!(
            app.apply(Action::CancelOperation),
            [Request::CancelOperation { id: id.clone() }]
        );
        app.receive(Data::OperationCancelled {
            id,
            result: Ok(String::from("Cancellation requested.")),
        });
        assert!(app.status.text.contains("Cancellation requested"));
    }

    #[test]
    fn cancel_before_operation_identity_is_queued_but_local_writes_refuse_it() {
        let remote = GitOperation::PushBranch {
            branch: git_vista_protocol::BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: git_vista_protocol::ForcePublish::None,
        };
        let mut app = loaded(THREE);
        app.receive(Data::PlanReady(Ok(plan_wire_for(remote))));
        let requests = app.apply(Action::ApprovePlan);
        let [_, Request::OperationByKey { key }] = requests.as_slice() else {
            panic!("missing operation lookup: {requests:?}");
        };
        assert!(app.apply(Action::CancelOperation).is_empty());
        assert!(app.status.text.contains("queued"));
        let id = OperationId::new("op_cancel_later").unwrap();
        app.receive(Data::OperationByKey {
            key: key.clone(),
            result: Ok(Some(id.clone())),
        });
        assert_eq!(app.tick(), [Request::CancelOperation { id }]);

        let mut local = loaded(THREE);
        local.receive(Data::PlanReady(Ok(plan_wire())));
        local.apply(Action::ApprovePlan);
        assert!(local.apply(Action::CancelOperation).is_empty());
        assert!(local.status.text.contains("cannot be cancelled"));
    }
}
