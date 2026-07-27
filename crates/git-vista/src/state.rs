//! Shared frontend state: the small data types and the signal *bundles* the
//! split view modules pass around.
//!
//! When the old monolithic `app.rs` was split, its per-overlay `RwSignal`s and
//! the context-menu/pending-op structs ended up shared across several modules
//! (`render`, `menu`, `dialogs`, `detail`, `gestures`). Rather than thread a
//! dozen individual signals through every function, the related ones are grouped
//! into small `Copy` bundles ([`Settings`], [`Features`]). Every Leptos handle —
//! `RwSignal`, `StoredValue`, `Resource` — is itself `Copy` (a lightweight
//! reference into the reactive arena, not the value), so a bundle is a cheap
//! handle to copy into a closure, never a clone of any actual state.
//!
//! The `Overlays` bundle that used to live here is gone (M1.11, #64, Task 8). It held
//! thirteen fields and enforced nothing, which is how the Esc handler came to omit the
//! Activity panel and how the two right-edge panels came to close each other on different
//! ticks. Its fields moved to the features that own them —
//! [`crate::features::shell::signals::Shell`] took the six overlays and the detail panel's
//! scroll wish, `dialogs` took the commit draft, `operations` took the click-order
//! bookkeeping — and what remains here are the two plain data types the menu passes around
//! plus the bundles themselves.

use leptos::{Resource, RwSignal};

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

use crate::features::dialogs::signals::Dialogs;
use crate::features::graph::core::GraphCore;
use crate::features::operations::signals::Operations;
use crate::features::shell::signals::Shell;
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
/// an epoch bump rebuilds — an in-flight operation, a modal's ghost-click guard and every
/// open overlay all have to survive that rebuild. Bundled because they were threaded as
/// five separate parameters, which is how `graph_canvas` reached nine arguments; since
/// Task 8 it is the *only* bundle the overlay views take, the retired `Overlays` having
/// been the other.
#[derive(Clone, Copy)]
pub struct Features {
    /// The graph epoch: bumped, generation-aware, to re-read the repo after a write.
    pub graph: RwSignal<GraphCore>,
    /// The app's one iOS ghost-click guard.
    pub dialogs: Dialogs,
    /// Where writes go.
    pub operations: Operations,
    /// The app's one working-tree status read — the topbar chip and the Activity
    /// panel's status section both render from it.
    pub status: StatusResource,
    /// Every overlay the app can put on screen, and the order they were raised in
    /// (M1.11, #64, Task 8). Replaces the `Overlays` bundle: the six overlay signals are
    /// private to it, so nothing can change what is visible without the stack hearing.
    ///
    /// The Activity panel's visibility is no longer a field of its own here. It is one of
    /// those six, and handing out a second way to write it is exactly how the right edge
    /// came to be governed by two rules on two different ticks. `App` still holds the
    /// `Activity` handle directly, because the shared status read keys on it.
    pub shell: Shell,
}

/// The lazily-fetched commit detail (Phase 10): keyed on the open commit's hash,
/// resolving to `None` while idle, `Some(Ok/Err)` once the fetch lands. A type
/// alias so the detail panel's signature stays readable.
pub type DetailResource = Resource<Option<String>, Option<Result<CommitDetail, String>>>;
