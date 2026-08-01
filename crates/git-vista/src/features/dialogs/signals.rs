//! The single holder of [`DialogsCore`], plus the viewer overlay's handle — wasm only
//! (M1.11, #64).
//!
//! One [`Dialogs`] replaces the three `StoredValue<f64>` guard clocks that lived in
//! `app/canvas.rs` and `app/mod.rs`. A `StoredValue`, not an `RwSignal`, on purpose: the
//! guard is bookkeeping consulted inside event handlers and nothing renders from it, which
//! is exactly what the three clocks it replaces were. Backing it with a signal would add a
//! reactive write to eleven click handlers that never had one.
//!
//! Created once in `App` rather than per canvas mount. The old `dialog_opened_at` was
//! created inside `graph_canvas`, so every epoch bump rebuilt it back to the `0.0`
//! never-opened sentinel; hoisting it means a rebuild while a modal is up no longer
//! silently drops that modal's guard.

use leptos::{
    create_rw_signal, store_value, RwSignal, SignalGet, SignalGetUntracked, SignalSet, StoredValue,
};

use crate::features::dialogs::core::{
    commit_draft_key, draft_scope_action, Dialog, DialogsCore, DraftScopeAction,
};

/// Best-effort handle on the tab's `sessionStorage`, the `prefs.rs`
/// convention: private browsing can refuse storage, in which case drafts
/// simply stay in-memory-only — degraded, never broken.
///
/// `sessionStorage`, not `localStorage`, on purpose (#226): the failure being
/// survived is iOS Safari suspending and rebuilding THIS tab's WASM module.
/// A draft is tab-scoped work in progress, not a durable preference — closing
/// the tab discarding it is the expected outcome, a draft resurfacing days
/// later in a fresh tab is not.
fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.session_storage().ok().flatten())
}

/// The app's one ghost-click guard, and the commit modal's message draft.
#[derive(Clone, Copy)]
pub struct Dialogs {
    core: StoredValue<DialogsCore>,
    /// The text currently typed into the commit modal's message box.
    ///
    /// Moved here from the `Overlays` bundle in Task 8 (M1.11, #64): it is the commit
    /// dialog's *content*, so it belongs to the dialogs feature rather than to the overlay
    /// stack, which only decides what is on screen. One consequence is deliberate and
    /// worth naming — living in `App` rather than in `graph_canvas`, a half-typed message
    /// now survives the canvas rebuild an epoch bump causes, where before it was silently
    /// discarded.
    commit_msg: RwSignal<String>,
    /// Which repository the draft belongs to (#226): the accepted Frame's
    /// `worktree_id`, observed by an `App` effect. `None` until the first
    /// Frame lands (or on a degraded Frame), during which drafts stay
    /// in-memory-only — nothing persists under an anonymous scope, so one
    /// repository's draft can never be misfiled under another.
    draft_scope: StoredValue<Option<String>>,
}

impl Dialogs {
    pub fn new() -> Self {
        Self {
            core: store_value(DialogsCore::default()),
            // Blank, not seeded: the scope isn't known until the first Frame
            // lands, and seeding happens in `set_draft_scope` when it does.
            commit_msg: create_rw_signal(String::new()),
            draft_scope: store_value(None),
        }
    }

    /// Observe the served repository (#226). Called by an `App` effect with
    /// every accepted Frame's `worktree_id` — which re-fires on every epoch
    /// reload, so the same-repo case MUST leave the live signal alone (the
    /// clobber rule is [`draft_scope_action`], host-tested). A genuinely new
    /// repository swaps the signal for that repository's persisted draft:
    /// this is both the suspension-recovery path (fresh WASM module, first
    /// Frame lands, draft comes back) and the repo-switch path (each repo's
    /// draft stays its own).
    pub fn set_draft_scope(&self, worktree_id: Option<String>) {
        let action = self
            .draft_scope
            .with_value(|old| draft_scope_action(old.as_deref(), worktree_id.as_deref()));
        if action == DraftScopeAction::KeepSignal {
            return;
        }
        let restored = worktree_id
            .as_deref()
            .and_then(|id| session_storage()?.get_item(&commit_draft_key(id)).ok()?)
            .unwrap_or_default();
        self.draft_scope.set_value(worktree_id);
        self.commit_msg.set(restored);
    }

    /// A tracked read — the modal's `<textarea>` and its Commit button both render from it.
    pub fn commit_msg(&self) -> String {
        self.commit_msg.get()
    }

    /// An untracked read, for the submit handler that must not subscribe.
    pub fn commit_msg_untracked(&self) -> String {
        self.commit_msg.get_untracked()
    }

    /// Update the draft, persisting every change (#226): unbounced on
    /// purpose — a commit message is small, `sessionStorage` writes are
    /// synchronous and cheap, and a debounce window is exactly the keystrokes
    /// an iOS suspension would eat.
    pub fn set_commit_msg(&self, msg: String) {
        self.draft_scope.with_value(|scope| {
            if let (Some(id), Some(storage)) = (scope.as_deref(), session_storage()) {
                let _ = storage.set_item(&commit_draft_key(id), &msg);
            }
        });
        self.commit_msg.set(msg);
    }

    /// Discard the draft — signal AND persisted copy (#226), so a submitted
    /// or deliberately-cleared message can't resurrect as a stale draft.
    /// Called on successful commit submit; the dialog *opener* deliberately
    /// does not call this any more, because opening is how a suspension-
    /// recovered draft comes back.
    pub fn clear_commit_msg(&self) {
        self.draft_scope.with_value(|scope| {
            if let (Some(id), Some(storage)) = (scope.as_deref(), session_storage()) {
                let _ = storage.remove_item(&commit_draft_key(id));
            }
        });
        self.commit_msg.set(String::new());
    }

    /// Record that `d` is opening now, and start its guard window.
    ///
    /// Call this immediately *before* setting the modal's own signal, and — where the
    /// opener closes the context menu in the same handler — before `menu.set(None)`:
    /// closing the menu disposes the handler's reactive owner, after which further writes
    /// are unreliable. That ordering rule is unchanged from the eleven inlined
    /// `set_value(js_sys::Date::now())` calls this replaces.
    pub fn open(&self, d: Dialog) {
        self.core.update_value(|c| c.open(d, js_sys::Date::now()));
    }

    /// Record that `d` closed. A no-op if some other dialog has since replaced it.
    ///
    /// Only the backdrop-dismiss paths call this today. A modal also closes via its own
    /// Cancel/Confirm button and via Esc, and neither routes through here, so the core's
    /// record of *which* dialog is up can lag reality. That is why this type deliberately
    /// exposes no `open_dialog()` reader: the only thing it is authoritative about is the
    /// guard window, which depends on the stamp alone.
    ///
    /// Task 8 made *visibility* single-pathed through
    /// [`crate::features::shell::signals::Shell`], but deliberately did not fold the guard
    /// into it: the stack decides what is on screen, the guard decides whether a tap
    /// arriving right now is real. Two rules, two owners. The record here is still only
    /// worth as much as its stamp.
    pub fn close(&self, d: Dialog) {
        self.core.update_value(|c| c.close(d));
    }

    /// Whether a backdrop dismiss arriving right now should be honoured, or is iOS's
    /// synthesized post-tap ghost click landing on the modal it just opened.
    ///
    /// Named for what the call site is deciding; delegates to
    /// [`DialogsCore::may_confirm`], which is where the arithmetic is tested.
    pub fn may_dismiss(&self) -> bool {
        self.core.with_value(|c| c.may_confirm(js_sys::Date::now()))
    }
}

impl Default for Dialogs {
    fn default() -> Self {
        Self::new()
    }
}

// The `Viewer` handle used to live here. Task 7 put it in this module for want of a
// better one — the doc comment said so outright, "because it is the other overlay the
// detail panel opens". Task 8 gave overlays an actual owner, so the full-screen viewer is
// now one entry in `features::shell`'s stack like the rest, and the separate handle is
// gone rather than left as a second way to open the same thing. It was never a [`Dialog`]:
// it has no backdrop dismiss and so has never consulted the guard.
