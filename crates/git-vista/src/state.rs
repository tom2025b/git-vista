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

use leptos::{Resource, RwSignal, StoredValue};

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
}

/// A branch operation awaiting confirmation in the modal (Issue #33 follow-up).
/// Merge and delete change history/refs and push reaches the network, so each is
/// confirmed before it runs — reusing the same in-app modal the commit dialog uses
/// (a native `confirm()` gets blocked/flashed by the webview, same as `prompt()`).
#[derive(Clone)]
pub enum PendingOp {
    /// Merge `branch` into the checked-out branch (`git merge <branch>`). `into` is
    /// the live HEAD branch, fetched when the item is clicked, so the confirmation
    /// names the true target; `None` => detached HEAD (the confirm button is disabled).
    Merge { branch: String, into: Option<String> },
    /// Push `branch` to origin (`git push origin <branch>`).
    Push { branch: String },
    /// Delete `branch` (`git branch -d <branch>`). `current` is the live HEAD branch,
    /// fetched on click; when it equals `branch` the confirm button is disabled (git
    /// refuses to delete the checked-out branch). `None` => detached HEAD (deletable).
    Delete { branch: String, current: Option<String> },
    /// Force-delete `branch` (`git branch -D <branch>`), discarding unmerged commits.
    /// Only reached after the safe [`PendingOp::Delete`] is refused with "not fully
    /// merged": the modal re-opens as this so the user can override rather than hit a
    /// dead-end error.
    ForceDelete { branch: String },
    /// Rebase the checked-out branch onto main (`git rebase main`, or `origin/main`
    /// when that remote-tracking ref exists — resolved server-side). `current` is the
    /// live HEAD branch, fetched on click, purely to name it in the dialog; `None` =>
    /// detached HEAD (the confirm button is disabled — there's no branch to rebase).
    Rebase { current: Option<String> },
}

/// How long (ms) after the commit modal opens to ignore a backdrop dismiss, so
/// iOS's synthesized post-tap "ghost click" can't close the modal it just opened.
pub const DIALOG_GUARD_MS: f64 = 400.0;

/// The persisted display settings, shared into every icon-drawing view so a
/// single toggle re-renders the whole app. Both are booleans behind signals:
/// `nerd_icons` picks the icon set (icons.rs); `show_node_icons` shows/hides the
/// glyph beside each commit dot.
#[derive(Clone, Copy)]
pub struct Settings {
    pub nerd_icons: RwSignal<bool>,
    pub show_node_icons: RwSignal<bool>,
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
    /// The open commit-message dialog, if any (Issue #33). `Some(allow_empty)`.
    pub commit_dialog: RwSignal<Option<bool>>,
    /// The text currently typed into that dialog's message box.
    pub commit_msg: RwSignal<String>,
    /// The branch operation awaiting confirmation, if any (Issue #33 follow-up).
    pub confirm_op: RwSignal<Option<PendingOp>>,
    /// The commit whose detail panel is open (Phase 10), by full hash.
    pub detail_id: RwSignal<Option<String>>,
    /// Whether the Activity panel is open (Activity/Undo feature). Created in
    /// `App` — the topbar owns its button — and threaded through here so the
    /// panel, the menu and the detail panel can keep each other exclusive
    /// (both are right-docked; stacking them would just hide one).
    pub activity_open: RwSignal<bool>,
    /// One-shot flag set by the menu's "Show diff" item: when the panel's
    /// Changes section next finishes rendering, scroll it into view, then
    /// clear the flag. A `StoredValue` (not a signal) on purpose — it's an
    /// instruction consumed by the next render, not state the UI reflects.
    pub scroll_diff: StoredValue<bool>,
    /// When the current modal was opened (ms) — the iOS ghost-click guard.
    pub dialog_opened_at: StoredValue<f64>,
    /// The App's fetch counter; bumped to re-read the repo after a write.
    pub reload: RwSignal<u32>,
}

/// The lazily-fetched commit detail (Phase 10): keyed on the open commit's hash,
/// resolving to `None` while idle, `Some(Ok/Err)` once the fetch lands. A type
/// alias so the detail panel's signature stays readable.
pub type DetailResource = Resource<Option<String>, Option<Result<CommitDetail, String>>>;
