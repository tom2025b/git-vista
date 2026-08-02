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

use crate::features::dialogs::commit::{
    adopt_seed, message_buffer, persist_key, AmendPhase, CommitIntent, MessageBuffer,
};
use crate::features::dialogs::core::{draft_scope_action, Dialog, DialogsCore, DraftScopeAction};

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

/// One console breadcrumb, first time a draft persist is refused (quota,
/// private-mode revocation). The draft then lives in-memory-only — visible
/// and submittable, but gone on suspension — and without this line a later
/// stale restore is indistinguishable from broken restore logic during the
/// iPad testbed pass. Once, not per keystroke: a refusing storage refuses
/// every write, and the console shouldn't scroll for it.
fn warn_persist_failed_once() {
    use std::cell::Cell;
    thread_local! {
        static WARNED: Cell<bool> = const { Cell::new(false) };
    }
    WARNED.with(|w| {
        if !w.replace(true) {
            web_sys::console::warn_1(
                &"git-vista: sessionStorage refused the commit draft; \
                  drafts are in-memory-only this session (#226)"
                    .into(),
            );
        }
    });
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
    /// `worktree_id`, observed by an `App` effect. `None` only before the
    /// first Frame lands, during which drafts stay in-memory-only — nothing
    /// persists under an anonymous scope, so one repository's draft can
    /// never be misfiled under another. Once known, the scope survives a
    /// frame going `None` (errored reload over a dropped tunnel): the
    /// clobber rule maps that to `KeepSignal` and the early return below
    /// leaves this value at the last-known repository.
    draft_scope: StoredValue<Option<String>>,
    /// Whether the confirm modal's second-step arm control has been pressed
    /// (M2.18b, #220).
    ///
    /// An `RwSignal`, unlike the guard beside it: the arm control and the
    /// confirm button both *render* from this — that visible state change is
    /// the whole point of a two-tap ceremony.
    ///
    /// Reset by [`Dialogs::open`] rather than by whoever raises the
    /// operation, so it cannot survive into a question it wasn't answered
    /// for. That matters most on the path `DialogsCore::open`'s own doc
    /// comment describes: `confirm.rs`'s escalation effect reopens the confirm
    /// modal *in place* with a different operation, and an arm that carried
    /// over would hand the new question a confirm button the user never armed.
    confirm_armed: RwSignal<bool>,
    /// The message being written for an **amend** (M2.19c, #224) — a second
    /// buffer, deliberately, and never persisted.
    ///
    /// `features::dialogs::commit::MessageBuffer` carries the whole argument;
    /// the short version is that amend mode *pre-fills* the box from HEAD, and
    /// routing that pre-fill through the #226 draft would overwrite (and
    /// persist over) a half-typed commit message the user still wants.
    amend_msg: RwSignal<String>,
    /// What [`Dialogs::amend_msg`] was last seeded with — the pre-filled tip
    /// message. Only ever compared, never shown: it is how
    /// `commit::adopt_seed` tells "the user hasn't touched this" from "the
    /// user typed something", so a slow `GET /api/commit/{id}` can never land
    /// on top of their words.
    amend_seed: StoredValue<String>,
    /// How the current amend attempt is going, including the guided re-check
    /// after the compare-and-swap refuses. Lives here rather than inside the
    /// modal's own view closure because retargeting the dialog at a fresh tip
    /// re-runs that closure — state created inside it would be wiped by the
    /// very step that produced it.
    amend_phase: RwSignal<AmendPhase>,
}

impl Dialogs {
    pub fn new() -> Self {
        Self {
            core: store_value(DialogsCore::default()),
            // Blank, not seeded: the scope isn't known until the first Frame
            // lands, and seeding happens in `set_draft_scope` when it does.
            commit_msg: create_rw_signal(String::new()),
            draft_scope: store_value(None),
            confirm_armed: create_rw_signal(false),
            amend_msg: create_rw_signal(String::new()),
            amend_seed: store_value(String::new()),
            amend_phase: create_rw_signal(AmendPhase::Idle),
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
            .and_then(|id| {
                let key = persist_key(MessageBuffer::Draft, id)?;
                session_storage()?.get_item(&key).ok()?
            })
            .unwrap_or_default();
        self.draft_scope.set_value(worktree_id);
        self.commit_msg.set(restored);
    }

    /// A tracked read of the message `intent` is editing — the modal's
    /// `<textarea>` and its confirm button both render from it.
    ///
    /// Which buffer that is comes from `commit::message_buffer`, not from a
    /// match written out again here: the draft-vs-amend split is the thing
    /// #226 and #224 have to agree on, so it is decided in one host-tested
    /// place and consumed everywhere else.
    pub fn message(&self, intent: &CommitIntent) -> String {
        match message_buffer(intent) {
            MessageBuffer::Draft => self.commit_msg.get(),
            MessageBuffer::Amend => self.amend_msg.get(),
        }
    }

    /// An untracked read, for the submit handler that must not subscribe.
    pub fn message_untracked(&self, intent: &CommitIntent) -> String {
        match message_buffer(intent) {
            MessageBuffer::Draft => self.commit_msg.get_untracked(),
            MessageBuffer::Amend => self.amend_msg.get_untracked(),
        }
    }

    /// Update the message `intent` is editing, persisting it **iff** its
    /// buffer has a storage key (#226 for the draft; nothing for amend).
    ///
    /// Unbounced on purpose — a commit message is small, `sessionStorage`
    /// writes are synchronous and cheap, and a debounce window is exactly the
    /// keystrokes an iOS suspension would eat.
    pub fn set_message(&self, intent: &CommitIntent, msg: String) {
        let buffer = message_buffer(intent);
        self.draft_scope.with_value(|scope| {
            let Some(id) = scope.as_deref() else { return };
            // `None` here is not "no storage available" — it is a buffer that
            // must not persist at all, which is the whole guarantee amend mode
            // rests on.
            let Some(key) = persist_key(buffer, id) else {
                return;
            };
            if let Some(storage) = session_storage() {
                if storage.set_item(&key, &msg).is_err() {
                    warn_persist_failed_once();
                }
            }
        });
        match buffer {
            MessageBuffer::Draft => self.commit_msg.set(msg),
            MessageBuffer::Amend => self.amend_msg.set(msg),
        }
    }

    /// Offer `incoming` as the amend box's pre-filled message.
    ///
    /// Adopted only if the user has not typed since the last seed
    /// (`commit::adopt_seed`), so the `GET /api/commit/{id}` behind the
    /// pre-fill can land whenever it lands — including after the user has
    /// started writing, or after the guided re-check has retargeted the
    /// dialog at a different tip — without ever eating their words.
    pub fn seed_amend_msg(&self, incoming: &str) {
        let current = self.amend_msg.get_untracked();
        let seed = self.amend_seed.with_value(|s| s.clone());
        if let Some(next) = adopt_seed(&current, &seed, incoming) {
            self.amend_msg.set(next);
        }
        // The seed is recorded either way: it is the baseline the *next*
        // pre-fill compares against, and a rejected seed still means "this is
        // what the tip says", not "nothing was offered".
        self.amend_seed.set_value(incoming.to_string());
    }

    /// The amend attempt's phase — a tracked read; the banner, the confirm
    /// button and the busy state all render from it.
    pub fn amend_phase(&self) -> AmendPhase {
        self.amend_phase.get()
    }

    pub fn set_amend_phase(&self, phase: AmendPhase) {
        self.amend_phase.set(phase);
    }

    /// Clear the amend buffer, its seed and its phase — on a successful amend,
    /// and whenever a dialog opens for a different question.
    pub fn reset_amend(&self) {
        self.amend_msg.set(String::new());
        self.amend_seed.set_value(String::new());
        self.amend_phase.set(AmendPhase::Idle);
    }

    /// The scope the draft belongs to right now — captured by the commit
    /// dialog's submit handler *before* the request is spawned, so the clear
    /// on success targets the repository that was actually submitted.
    pub fn draft_scope_snapshot(&self) -> Option<String> {
        self.draft_scope.with_value(|s| s.clone())
    }

    /// Discard the draft submitted under `submitted_scope` — persisted copy
    /// unconditionally, live signal only if that repository is still the one
    /// being served (#226). The distinction matters because this runs in the
    /// commit request's completion callback, not at submit time: if the
    /// served repository changed while the POST was in flight, clearing
    /// "the current draft" would delete the *new* repository's draft and
    /// leave the submitted one to resurrect from storage later. The dialog
    /// *opener* deliberately calls no clear at all, because opening is how a
    /// suspension-recovered draft comes back.
    /// M2.19c (#224) took the `intent` argument: an amend consumes the amend
    /// buffer, which has no persisted copy and no repository scope, so the
    /// storage half of this must not run for it — clearing a draft key on an
    /// amend's success would delete a commit message the user is still
    /// writing.
    pub fn clear_message_for(&self, intent: &CommitIntent, submitted_scope: Option<&str>) {
        match message_buffer(intent) {
            MessageBuffer::Draft => {
                if let (Some(id), Some(storage)) = (submitted_scope, session_storage()) {
                    if let Some(key) = persist_key(MessageBuffer::Draft, id) {
                        let _ = storage.remove_item(&key);
                    }
                }
                let still_current = self
                    .draft_scope
                    .with_value(|s| s.as_deref() == submitted_scope);
                if still_current {
                    self.commit_msg.set(String::new());
                }
            }
            MessageBuffer::Amend => self.reset_amend(),
        }
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
        // Every open is a new question, so the two-tap arm starts over (#220).
        // Here rather than in the openers for the same reason the guard stamp
        // is here: eleven call sites getting it right by repetition is what
        // M1.11 was cleaning up.
        self.confirm_armed.set(false);
        // Same rule for the amend attempt (#224): a fresh open must not
        // inherit the previous amend's banner, its half-finished re-check, or
        // a pre-fill taken from a tip that is no longer what the dialog is
        // pointed at. Note what does *not* route through here — the guided
        // re-check retargets the open dialog without calling `open`, precisely
        // so the message survives it.
        self.reset_amend();
    }

    /// Press the confirm modal's second-step arm control (#220). One-way
    /// within a single question — the only thing that clears it is a fresh
    /// [`Dialogs::open`].
    pub fn arm_confirm(&self) {
        self.confirm_armed.set(true);
    }

    /// Whether the second step has been taken. A **tracked** read: the arm
    /// control's own label and the confirm button's enabled state both
    /// re-render from it.
    pub fn confirm_armed(&self) -> bool {
        self.confirm_armed.get()
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
