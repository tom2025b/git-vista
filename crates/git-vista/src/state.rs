//! Shared frontend state: the small data types and the signal *bundles* the
//! split view modules pass around.
//!
//! When the old monolithic `app.rs` was split, its per-overlay `RwSignal`s and
//! the context-menu/pending-op structs ended up shared across several modules
//! (`render`, `menu`, `dialogs`, `detail`, `gestures`). Rather than thread a
//! dozen individual signals through every function, the related ones are grouped
//! into small `Copy` bundles ([`Settings`], [`Overlays`]). Every Leptos handle —
//! `RwSignal`, `StoredValue`, `Resource` — is itself `Copy` (a lightweight
//! reference into the reactive arena, not the value), so a bundle is a cheap
//! handle to copy into a closure, never a clone of any actual state.

use leptos::{Resource, RwSignal, SignalSet, StoredValue};

use git_vista_core::model::CommitDetail;

/// State for the per-commit context menu (Issue #18): which commit was tapped,
/// where to draw the menu (client/viewport px, since it's an HTML overlay, not
/// part of the pan/zoomed SVG), and the commit's GitHub URL when it has one.
#[derive(Clone)]
pub struct MenuData {
    /// Full commit hash — what "Create branch" targets. For a branch stub this is
    /// its tip's commit (the branch owns no commit of its own), so branching from
    /// the stub forks off that commit.
    pub commit: String,
    /// The menu's header: a commit's short hash, or a stub's branch name — so a
    /// stub reads as the branch it is, not the commit it happens to sit on
    /// (Issue #30).
    pub header: String,
    /// Viewport x/y of the click, used to position the overlay.
    pub x: f64,
    pub y: f64,
    /// GitHub URL for the "Open on GitHub" item — a commit page for a commit dot,
    /// or the branch's tree page for a stub. `Some` only when this repo has a
    /// github.com origin *and* the target is pushed (otherwise it would 404);
    /// `None` renders the item disabled.
    pub github_url: Option<String>,
    /// Label for the "Open on GitHub" item, so a stub says "branch" and a commit
    /// says "commit".
    pub github_label: &'static str,
    /// Label for the "Create branch…" item, so a stub (which represents a branch)
    /// reads "from this branch" while a commit dot reads "from this commit".
    pub create_label: &'static str,
    /// True when this target is the current HEAD tip — the only place a new commit
    /// can land without moving HEAD, so the "Commit …" items are enabled only here
    /// (Issue #33). A branch stub is never the HEAD tip, so it's always false.
    pub is_head: bool,
    /// Local branch names living at this target: a stub's own name, or every local
    /// branch badge on a commit dot. Each yields a set of merge/push/delete items
    /// (Issue #33 follow-up). Empty => the target carries no branch, so no branch
    /// operations are shown.
    pub branches: Vec<String>,
    /// True when the menu belongs to a branch stub rather than a commit dot —
    /// picks the branch icon (vs the commit icon) for the menu header, so the
    /// header's glyph matches what the header names.
    pub is_branch: bool,
    /// GitHub web base for this repo (e.g. "https://github.com/owner/repo"), when
    /// it has a github.com origin. Used to build the "Create Pull Request" item's
    /// compare URL (`<base>/compare/main...<branch>`); `None` => no GitHub repo, so
    /// that item is omitted.
    pub repo_url: Option<String>,
    /// Any-host forge web base (ADR 0010), for the non-GitHub branch link items —
    /// shown only when [`repo_url`](Self::repo_url) is `None`, so it never
    /// duplicates the GitHub items. `None` => no usable remote.
    pub remote_web_url: Option<String>,
}

/// What the commit-message dialog (Issue #33) is collecting a message for:
/// which kind of commit, and where it should land.
#[derive(Clone)]
pub struct CommitDialog {
    /// `git commit --allow-empty` (an empty commit) vs a staged-changes commit.
    pub allow_empty: bool,
    /// Branch the commit should land on. `None` => the checked-out branch (a
    /// plain `git commit` on HEAD — every commit item on a commit dot). `Some`
    /// => a branch stub's own name: the server writes the commit object and
    /// moves just that ref, so an empty branch can take its first commit
    /// without a checkout. Only ever `Some` together with `allow_empty`.
    pub branch: Option<String>,
}

// The branch-operation vocabulary moved to `features/operations/kind.rs` in M1.11
// (#64): it is framework-free, so it compiles and is unit-tested on the host target,
// while this module is wasm-only. Re-exported under its old name so the ~40 existing
// `PendingOp::…` call sites in `menu.rs`, `dialogs/` and `activity.rs` keep reading
// naturally; `dialogs/confirm.rs` still matches one arm per `api.rs` function.
pub use crate::features::operations::kind::OperationKind as PendingOp;

use crate::features::activity::signals::Activity;
use crate::features::dialogs::signals::{Dialogs, Viewer};
use crate::features::graph::core::GraphCore;
use crate::features::operations::core::{IntentSeq, PendingIntent};
use crate::features::operations::signals::Operations;
use crate::features::status::signals::StatusResource;

// `DIALOG_GUARD_MS` used to live here. It moved to `features/dialogs/core.rs` in M1.11
// (#64), next to the comparison that reads it — a guard window stated apart from the
// arithmetic it governs is a number nobody can check. Both are host-tested there now.

/// What the full-screen viewer (viewer.rs) is showing: a commit's whole diff
/// (the detail panel's "Expand Full Diff"), or one file's full content at a
/// commit (tapping a file in the diff list). Both get one Print / Save PDF.
#[derive(Clone, PartialEq, Eq)]
pub enum ViewerDoc {
    /// The full (uncapped) diff of one commit, by full hash.
    Diff { id: String },
    /// One file's content at one commit.
    File { id: String, path: String },
}

/// The persisted display settings, shared into every icon-drawing view so a
/// single toggle re-renders the whole app. Both are booleans behind signals:
/// `nerd_icons` picks the icon set (icons.rs); `show_node_icons` shows/hides the
/// glyph beside each commit dot.
#[derive(Clone, Copy)]
pub struct Settings {
    pub nerd_icons: RwSignal<bool>,
    pub show_node_icons: RwSignal<bool>,
}

/// The feature handles `App` owns and hands down to the graph canvas (M1.11, #64).
///
/// Every one of these is created **above** `graph_canvas`, so it outlives the canvas that
/// an epoch bump rebuilds — an in-flight operation, a modal's ghost-click guard and the
/// panel's visibility all have to survive that rebuild. Bundled for the same reason
/// [`Overlays`] is: they were threaded as five separate parameters, which is how
/// `graph_canvas` reached nine arguments.
#[derive(Clone, Copy)]
pub struct Features {
    /// The graph epoch: bumped, generation-aware, to re-read the repo after a write.
    pub graph: RwSignal<GraphCore>,
    /// The Activity panel's visibility.
    pub activity: Activity,
    /// The app's one iOS ghost-click guard.
    pub dialogs: Dialogs,
    /// Where writes go.
    pub operations: Operations,
    /// The app's one working-tree status read — the topbar chip and the Activity
    /// panel's status section both render from it.
    pub status: StatusResource,
}

/// The mutually-exclusive overlay signals (context menu, the two modals, the
/// detail panel) plus the ghost-click guard timestamp and the shared fetch
/// counter — everything the menu items and modals need to open, close and
/// trigger a re-read. Bundled so the menu/dialog/detail builders take one `Copy`
/// handle instead of seven separate signals.
#[derive(Clone, Copy)]
pub struct Overlays {
    /// The open context menu, if any (Issue #18). `None` => no menu.
    pub menu: RwSignal<Option<MenuData>>,
    /// The open commit-message dialog, if any (Issue #33).
    pub commit_dialog: RwSignal<Option<CommitDialog>>,
    /// The text currently typed into that dialog's message box.
    pub commit_msg: RwSignal<String>,
    /// The branch operation awaiting confirmation, if any (Issue #33 follow-up).
    pub confirm_op: RwSignal<Option<PendingOp>>,
    /// The commit whose detail panel is open (Phase 10), by full hash.
    pub detail_id: RwSignal<Option<String>>,
    /// The full-screen viewer (full diff / full file). Sits on top of the detail
    /// panel it was opened from. A [`Viewer`] rather than a bare signal since
    /// M1.11 (#64): `detail.rs` used to construct the document and poke the raw
    /// signal itself.
    pub viewer: Viewer,
    /// The Activity panel's visibility (Activity/Undo feature). Created in `App` —
    /// the topbar owns its button — and threaded through here so the panel, the
    /// menu and the detail panel can keep each other exclusive (both are
    /// right-docked; stacking them would just hide one).
    pub activity: Activity,
    /// One-shot flag set by the menu's "Show diff" item: when the panel's
    /// Changes section next finishes rendering, scroll it into view, then
    /// clear the flag. A `StoredValue` (not a signal) on purpose — it's an
    /// instruction consumed by the next render, not state the UI reflects.
    pub scroll_diff: StoredValue<bool>,
    /// The iOS ghost-click guard (M1.11, #64). Replaces `dialog_opened_at:
    /// StoredValue<f64>` — and, once every modal was routed through it, the two
    /// further clocks (`reset_opened_at`, `open_opened_at`) that `App` used to keep
    /// alongside it. One guard, one owner, one tested rule.
    pub dialogs: Dialogs,
    /// Mints the click-order sequence for branch operations (M1.11, #64). A
    /// `StoredValue`, not a signal: minting is bookkeeping done inside an event
    /// handler, and nothing renders from it.
    pub intent_seq: StoredValue<IntentSeq>,
    /// The newest branch-operation intent that has actually reached
    /// [`Overlays::confirm_op`]. A menu item's `fetch_head_branch()` pre-check
    /// resolves in network order, so each continuation compares against this
    /// before committing and a straggler from an earlier click is dropped
    /// instead of reopening its dialog over the one the user is looking at.
    pub pending_intent: StoredValue<Option<PendingIntent>>,
    /// The graph epoch (M1.11, #64): bumped, generation-aware, to re-read the repo
    /// after a write. Replaces the old bare `reload: RwSignal<u32>` counter.
    pub graph: RwSignal<GraphCore>,
    /// Where writes go (M1.11, #64). Created in `App`, **above** `graph_canvas`, so an
    /// in-flight operation outlives the canvas an epoch bump rebuilds.
    pub operations: Operations,
}

impl Overlays {
    /// Right-edge exclusivity, the detail-panel direction: opening the detail panel on
    /// `id` closes Activity, because both dock the same edge and would otherwise stack
    /// and hide one another.
    ///
    /// Collapses what were two identical pairs of raw signal pokes in `menu.rs` ("View
    /// details" and "Show diff") into one named place (M1.11, #64, Task 7). This is an
    /// intermediate, not the fix: the rule still lives in two methods that each writer
    /// must remember to call, rather than in one stack that makes the invariant
    /// unrepresentable. Task 8's `shell.overlay_stack` replaces both outright.
    pub fn open_detail_panel(&self, id: String) {
        self.activity.close();
        self.detail_id.set(Some(id));
    }

    /// The reverse direction: opening Activity closes the detail panel.
    ///
    /// Still driven by a reactive effect in `activity.rs` that fires one tick *after*
    /// the panel's visibility flips — not synchronously from the topbar button that
    /// flipped it — so both panels can still render together for a frame. Naming the
    /// write does not close that window; only Task 8's single dismiss path does.
    pub fn close_detail_for_activity(&self) {
        self.detail_id.set(None);
    }
}

/// The lazily-fetched commit detail (Phase 10): keyed on the open commit's hash,
/// resolving to `None` while idle, `Some(Ok/Err)` once the fetch lands. A type
/// alias so the detail panel's signature stays readable.
pub type DetailResource = Resource<Option<String>, Option<Result<CommitDetail, String>>>;
