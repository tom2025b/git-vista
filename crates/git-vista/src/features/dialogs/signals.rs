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
    adopt_seed, decode_draft, detail_read_use, encode_draft, is_reading_publication_for,
    message_buffer, persist_key, seed_outcome, AmendPhase, CommitIntent, DetailUse, DraftRecord,
    MessageBuffer, PreflightKnowledge, SeedOutcome,
};
use crate::features::dialogs::core::{
    draft_scope_action, pull_confirm_enabled, Dialog, DialogsCore, DraftScopeAction, PullTarget,
};
use git_vista_protocol::plan::MergeStrategy;

/// Best-effort handle on the tab's `localStorage`, the `prefs.rs` convention:
/// private browsing can refuse storage, in which case drafts simply stay
/// in-memory-only — degraded, never broken.
///
/// `localStorage`, not `sessionStorage` (Tom's 2026-08-17 ruling, superseding
/// #226's choice below). Two decisions on record, both kept honest:
///
/// **#226 chose `sessionStorage`.** The failure being survived was iOS
/// Safari suspending and rebuilding THIS tab's WASM module — a same-tab
/// recovery, so tab-scoped storage fit: closing the tab discarding the draft
/// was the expected outcome, a draft resurfacing days later in a fresh tab
/// was not.
///
/// **Tom ruled `localStorage` instead, 2026-08-17**, after two hard power
/// losses on this box in one day. `sessionStorage` dies with the browser
/// process; a power cut takes it out along with the tab. `localStorage`
/// survives that, at the cost of a draft that can resurface hours or days
/// later — which is why this milestone pairs the wider storage with a
/// banner (`Dialogs::draft_offer`) instead of shipping it silent: the draft
/// is never auto-filled into the textarea, only offered back with its age
/// shown, so a stale draft is a visible choice and never an ambush.
fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

// What this file decides untested, stated plainly (this whole file is
// wasm-only and never compiles under `cargo test --workspace`, so nothing
// below is a gap that could have been covered here):
//
// - The `localStorage` calls themselves — `get_item`/`set_item`/
//   `remove_item` on the real `web_sys::Storage`, including the
//   private-browsing/quota refusal path `warn_persist_failed_once` exists
//   for. Everything that decides *what* to store (`encode_draft`) and how to
//   read it back (`decode_draft`) is pure and tested in
//   `features::dialogs::commit`; only the browser API call that carries the
//   bytes across is not.
// - The Restore/Discard click handlers in `dialogs/commit.rs` — that a tap
//   on either button actually reaches `Dialogs::restore_draft` /
//   `Dialogs::discard_draft` is wiring, not logic, and this repo has no
//   harness that drives a click through Leptos outside a real browser.
// - The `<textarea prop:value=move || dialogs.message(&message_intent)>`
//   re-render after `Dialogs::restore_draft` calls `self.commit_msg.set(…)`.
//   This is the one behavior the whole banner exists for — that pressing
//   Restore visibly fills the box — and nothing in this repository can
//   check that a `prop:value` binding actually re-renders on a signal write.
// The manual iPad testbed pass this milestone's siblings (#224, #225) went
// through is the only verification any of these three get.

/// One console breadcrumb, first time a draft persist is refused (quota,
/// private-mode revocation). The draft then lives in-memory-only — visible
/// and submittable, but gone on reload — and without this line a later
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
                &"git-vista: localStorage refused the commit draft; \
                  drafts are in-memory-only this session (#226, localStorage 2026-08-17)"
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
    /// A stored draft offered back for restore, or `None` (localStorage
    /// ruling, 2026-08-17). `Some` for exactly as long as the banner should
    /// show: set when [`Dialogs::set_draft_scope`] reads a genuinely new
    /// repository's storage and finds a draft there, cleared by
    /// [`Dialogs::restore_draft`], [`Dialogs::discard_draft`], or the first
    /// keystroke into the box (`Dialogs::set_message` — typing starts
    /// overwriting the persisted draft, "last write wins", and the banner
    /// must not go on describing text a live keystroke has already replaced).
    ///
    /// A tracked `RwSignal`: the banner renders straight from this, the same
    /// role `pull_target` plays for the pull-strategy picker below.
    draft_offer: RwSignal<Option<DraftRecord>>,
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
    /// What the dialog has learned about the commit an amend would rewrite,
    /// and what the user has agreed to about it (M2.19d, #225).
    ///
    /// A `StoredValue`, not a signal: nothing renders from it. The banner that
    /// *does* render comes from [`AmendPhase::AwaitingPublishedConfirm`],
    /// which the submit handler enters after consulting this — so a detail read
    /// landing late changes what the next press does, not what is currently on
    /// screen, and gives no reason to re-render the modal underneath the user.
    amend_preflight: StoredValue<PreflightKnowledge>,
    /// The `{remote, branch}` a pull's strategy is being chosen for, and the
    /// picker's own visibility (#232, ADR 0044): `Some` while the picker is
    /// up, `None` otherwise.
    ///
    /// A tracked `RwSignal`, not the `StoredValue` most of this bundle's
    /// bookkeeping uses, because the picker's view renders straight from it
    /// — the same role `Shell::confirm_op` plays for the branch-op modal.
    /// It lives here rather than in `Shell` because a pull's target is
    /// picker-only state with no server round trip until a strategy is
    /// chosen: nothing outside this dialog's own lifecycle needs to see it.
    pull_target: RwSignal<Option<PullTarget>>,
    /// The strategy chosen in the picker so far. `None` until the user taps
    /// Merge or Rebase — ADR 0044 rejected any default, so this is the only
    /// representation anywhere in the client of "not yet decided", and
    /// nothing ever seeds it with a guess.
    ///
    /// Reset on every [`Dialogs::open`], same chokepoint as `confirm_armed`
    /// above, so "no pre-selected option" holds on *every* pull, not only
    /// the first (a remembered last choice would be the same defaulting ADR
    /// 0044 forbids, just delayed by one pull).
    confirm_strategy: RwSignal<Option<MergeStrategy>>,
}

impl Dialogs {
    pub fn new() -> Self {
        Self {
            core: store_value(DialogsCore::default()),
            // Blank, not seeded: the scope isn't known until the first Frame
            // lands, and seeding happens in `set_draft_scope` when it does.
            commit_msg: create_rw_signal(String::new()),
            draft_scope: store_value(None),
            draft_offer: create_rw_signal(None),
            confirm_armed: create_rw_signal(false),
            amend_msg: create_rw_signal(String::new()),
            amend_seed: store_value(String::new()),
            amend_phase: create_rw_signal(AmendPhase::Idle),
            amend_preflight: store_value(PreflightKnowledge::default()),
            pull_target: create_rw_signal(None::<PullTarget>),
            confirm_strategy: create_rw_signal(None::<MergeStrategy>),
        }
    }

    /// Observe the served repository (#226). Called by an `App` effect with
    /// every accepted Frame's `worktree_id` — which re-fires on every epoch
    /// reload, so the same-repo case MUST leave the live signal alone (the
    /// clobber rule is [`draft_scope_action`], host-tested). A genuinely new
    /// repository reads that repository's persisted draft, but — Tom's
    /// 2026-08-17 ruling — **never fills the textarea with it**: the box is
    /// left blank and, if storage held a well-formed draft, `draft_offer` is
    /// set so the banner can offer it back. This is both the reload-recovery
    /// path (fresh WASM module, first Frame lands, a stored draft is there
    /// to offer) and the repo-switch path (each repository's draft stays its
    /// own, offered only under its own scope).
    pub fn set_draft_scope(&self, worktree_id: Option<String>) {
        let action = self
            .draft_scope
            .with_value(|old| draft_scope_action(old.as_deref(), worktree_id.as_deref()));
        if action == DraftScopeAction::KeepSignal {
            return;
        }
        let offer = worktree_id.as_deref().and_then(|id| {
            let key = persist_key(MessageBuffer::Draft, id)?;
            let raw = local_storage()?.get_item(&key).ok()??;
            decode_draft(&raw)
        });
        self.draft_scope.set_value(worktree_id);
        // Blank, always — a stored draft is offered, never auto-filled.
        // Silence here is exactly the failure mode Tom vetoed: the banner
        // below is what makes a stale draft a visible choice instead of an
        // ambush.
        self.commit_msg.set(String::new());
        self.draft_offer.set(offer);
    }

    /// The stored draft currently offered for restore, if any — a tracked
    /// read; the banner renders straight from it and disappears the instant
    /// it goes `None`.
    pub fn draft_offer(&self) -> Option<DraftRecord> {
        self.draft_offer.get()
    }

    /// Fill the box with the offered draft and dismiss the banner (the
    /// Restore button).
    ///
    /// Does not touch storage: the persisted copy already holds this exact
    /// text under this scope's key, so there is nothing to write until the
    /// user's next keystroke does it the normal way through
    /// [`Dialogs::set_message`].
    pub fn restore_draft(&self) {
        let Some(record) = self.draft_offer.get_untracked() else {
            return;
        };
        self.commit_msg.set(record.message);
        self.draft_offer.set(None);
    }

    /// Delete the offered draft from storage and dismiss the banner (the
    /// Discard button). Leaves the box exactly as it was — empty, since the
    /// box is never auto-filled — there is nothing in it to clear.
    pub fn discard_draft(&self) {
        let scope = self.draft_scope.with_value(|s| s.clone());
        if let (Some(id), Some(storage)) = (scope.as_deref(), local_storage()) {
            if let Some(key) = persist_key(MessageBuffer::Draft, id) {
                let _ = storage.remove_item(&key);
            }
        }
        self.draft_offer.set(None);
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
    /// buffer has a storage key (the draft; nothing for amend).
    ///
    /// Unbounced on purpose — a commit message is small, `localStorage`
    /// writes are synchronous and cheap, and a debounce window is exactly the
    /// keystrokes a reload or power cut would eat.
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
            if let Some(storage) = local_storage() {
                // An emptied box removes the key rather than writing
                // `{"message":"",…}` — `decode_draft` already refuses to
                // offer an empty message back, so writing it would only
                // leave a phantom key sitting in storage under this scope
                // forever, decoding to nothing on every future read.
                let result = if msg.trim().is_empty() {
                    storage.remove_item(&key)
                } else {
                    storage.set_item(&key, &encode_draft(&msg, js_sys::Date::now()))
                };
                if result.is_err() {
                    warn_persist_failed_once();
                }
            }
        });
        // The first keystroke into the (deliberately empty) draft box
        // dismisses a still-open draft-restore banner: typing has already
        // started overwriting the persisted draft above (last write wins),
        // so a banner still describing the old stored text would be
        // describing something storage no longer holds.
        //
        // Gated to `MessageBuffer::Draft` on purpose, not unconditional: a
        // keystroke into the *amend* box must never touch the plain-commit
        // draft's offer. Amend mode is reached from the context menu while a
        // draft offer can still be pending — typing an amend message must
        // leave that offer exactly as it was, so it is still there, correct
        // and un-dismissed, if the user cancels the amend and returns to a
        // plain commit. This is the same buffer-isolation rule `MessageBuffer`
        // itself exists to state.
        if buffer == MessageBuffer::Draft && self.draft_offer.get_untracked().is_some() {
            self.draft_offer.set(None);
        }
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
    /// Returns what it did to the box. The guided re-check announces the
    /// retarget in a banner that speaks about the box's contents, and it can
    /// only be honest about them if it is told: an untouched pre-fill *is*
    /// replaced here, so a banner that assumed otherwise would vouch for text
    /// this call had just thrown away.
    pub fn seed_amend_msg(&self, incoming: &str) -> SeedOutcome {
        let current = self.amend_msg.get_untracked();
        let seed = self.amend_seed.with_value(|s| s.clone());
        let adopted = adopt_seed(&current, &seed, incoming);
        if let Some(next) = &adopted {
            self.amend_msg.set(next.clone());
        }
        // The seed is recorded either way: it is the baseline the *next*
        // pre-fill compares against, and a rejected seed still means "this is
        // what the tip says", not "nothing was offered".
        self.amend_seed.set_value(incoming.to_string());
        seed_outcome(adopted.as_ref())
    }

    /// The amend attempt's phase — a tracked read; the banner, the confirm
    /// button and the busy state all render from it.
    pub fn amend_phase(&self) -> AmendPhase {
        self.amend_phase.get()
    }

    pub fn set_amend_phase(&self, phase: AmendPhase) {
        self.amend_phase.set(phase);
    }

    /// Record what `GET /api/commit/{tip}` said about whether that commit is
    /// already on a remote (#225).
    ///
    /// Called from both places the dialog reads a detail — opening amend mode
    /// from the context menu, and the guided re-check's retarget — because the
    /// pre-flight gate has to answer for whichever commit the dialog is
    /// currently pointed at, not for whichever one it was opened on.
    ///
    /// Unconditional, and therefore only safe where the caller can prove the
    /// dialog still points at `tip`. The guided re-check can: it retargets and
    /// records with no `await` between the two. The menu's opener cannot, and
    /// must go through [`Dialogs::apply_amend_detail`] instead.
    pub fn record_amend_detail(&self, tip: &str, on_remote: bool) {
        self.amend_preflight
            .update_value(|k| k.record_detail(tip, on_remote));
    }

    /// Apply a resolved `GET /api/commit/{tip}` — its published flag *and* its
    /// message — **iff that read still speaks for the dialog** (#225).
    ///
    /// The chokepoint for every detail read that crosses an `await`. The
    /// decision is [`detail_read_use`], which is host-tested; this is only the
    /// two writes it authorises, because nothing in this file compiles under
    /// `cargo test --workspace`. A source census in `features::a11y::audit`
    /// pins that the guard is consulted first and that both writes sit inside
    /// it.
    ///
    /// Applying an abandoned tip's answer is not a harmless late write:
    /// [`PreflightKnowledge`] holds one read at a time, so it *evicts* the
    /// answer for the commit on screen and the published-history ceremony
    /// stops firing for it. See [`detail_read_use`] for the whole reasoning.
    pub fn apply_amend_detail(&self, tip: &str, on_remote: bool, message: &str) -> DetailUse {
        match detail_read_use(&self.amend_phase.get_untracked(), tip) {
            DetailUse::Apply => {
                self.record_amend_detail(tip, on_remote);
                self.seed_amend_msg(message);
                DetailUse::Apply
            }
            DetailUse::Discard => DetailUse::Discard,
        }
    }

    /// Hold the confirm button while `GET /api/commit/{tip}` — the read that
    /// supplies the pre-flight's only input — is outstanding (#225).
    ///
    /// Call it in the *same synchronous handler* that opens amend mode, after
    /// [`Dialogs::open`] (which resets the phase) and before the read is
    /// spawned. Both halves of that ordering are load-bearing and neither is
    /// checkable by the compiler, so they are pinned by a source census in
    /// `features::a11y::audit`.
    pub fn begin_publication_read(&self, tip: &str) {
        self.amend_phase.set(AmendPhase::ReadingPublication {
            tip: tip.to_string(),
        });
    }

    /// Release the publication-read window for `tip` — **on both outcomes of
    /// the read**, success and failure alike (#225).
    ///
    /// Releasing on failure is not an oversight. `amend_preflight` treats an
    /// unread detail as `Unknown` and sends, deliberately (see its doc
    /// comment); holding the button shut on a failed read would instead make
    /// amend unreachable whenever a single GET went wrong. The window is for
    /// "the answer is coming", not "the answer never came".
    ///
    /// Only clears the phase if it is still *this* tip's window — the caller
    /// resumes after an `await`, and the dialog may have been reopened on
    /// another commit meanwhile.
    pub fn finish_publication_read(&self, tip: &str) {
        if is_reading_publication_for(&self.amend_phase.get_untracked(), tip) {
            self.amend_phase.set(AmendPhase::Idle);
        }
    }

    /// Record the ceremony's explicit second step for `tip` (#225).
    pub fn confirm_amend_target(&self, tip: &str) {
        self.amend_preflight.update_value(|k| k.confirm(tip));
    }

    /// What the pre-flight gate consults. Untracked by construction (a
    /// `StoredValue`), which is what the submit handler needs: it reads this
    /// inside a click handler that must not subscribe to anything.
    pub fn amend_knowledge(&self) -> PreflightKnowledge {
        self.amend_preflight.with_value(|k| k.clone())
    }

    /// Clear the amend buffer, its seed, its phase and everything the
    /// pre-flight gate knows — on a successful amend, and whenever a dialog
    /// opens for a different question.
    ///
    /// The pre-flight half matters most on the second of those: a fresh open is
    /// a fresh commit, and inheriting "the user already agreed to rewrite
    /// published history" would spend one amend's consent on another's.
    pub fn reset_amend(&self) {
        self.amend_msg.set(String::new());
        self.amend_seed.set_value(String::new());
        self.amend_phase.set(AmendPhase::Idle);
        self.amend_preflight
            .set_value(PreflightKnowledge::default());
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
    /// *opener* deliberately calls no clear at all — reopening shows the
    /// offer banner if a draft is still on record, but never auto-fills.
    /// M2.19c (#224) took the `intent` argument: an amend consumes the amend
    /// buffer, which has no persisted copy and no repository scope, so the
    /// storage half of this must not run for it — clearing a draft key on an
    /// amend's success would delete a commit message the user is still
    /// writing.
    pub fn clear_message_for(&self, intent: &CommitIntent, submitted_scope: Option<&str>) {
        match message_buffer(intent) {
            MessageBuffer::Draft => {
                if let (Some(id), Some(storage)) = (submitted_scope, local_storage()) {
                    if let Some(key) = persist_key(MessageBuffer::Draft, id) {
                        let _ = storage.remove_item(&key);
                    }
                }
                let still_current = self
                    .draft_scope
                    .with_value(|s| s.as_deref() == submitted_scope);
                if still_current {
                    self.commit_msg.set(String::new());
                    // A submitted draft leaves nothing to offer for this
                    // scope. In the ordinary path the banner is already gone
                    // — typing dismissed it via `set_message` long before the
                    // confirm button could be non-empty — this is the
                    // defensive close for any path that reaches a submit
                    // without going through that dismissal.
                    self.draft_offer.set(None);
                }
            }
            MessageBuffer::Amend => self.reset_amend(),
        }
    }

    /// Open the pull strategy picker (#232, ADR 0044) targeting the live
    /// `remote`/`branch` the caller resolved — exactly like `rebase_item`'s
    /// `fetch_head_branch()` pre-check, never a possibly-stale graph read.
    ///
    /// Starts the ghost-click guard the same as any other [`Dialogs::open`]
    /// call first (which also resets `confirm_strategy` to `None`, below),
    /// then sets the target — in that order, so the reset can never clobber
    /// the target it is about to receive.
    pub fn open_pull_picker(&self, remote: String, branch: String) {
        self.open(Dialog::PullStrategy);
        self.pull_target.set(Some(PullTarget { remote, branch }));
    }

    /// The picker's target, and its own visibility: a tracked read, `Some`
    /// exactly while the picker is up.
    pub fn pull_target(&self) -> Option<PullTarget> {
        self.pull_target.get()
    }

    /// Record the strategy chosen in the picker (ADR 0044). Only ever called
    /// from a tap on one of the two toggle buttons — never seeded, never
    /// defaulted, and nothing else in this bundle writes it.
    pub fn set_pull_strategy(&self, strategy: MergeStrategy) {
        self.confirm_strategy.set(Some(strategy));
    }

    /// The strategy chosen so far, if any — a tracked read: the two toggle
    /// buttons' `aria-pressed` and the Pull button's enabled state both
    /// render from it.
    pub fn pull_strategy(&self) -> Option<MergeStrategy> {
        self.confirm_strategy.get()
    }

    /// Whether the picker's Pull button may run yet — literally ADR 0044's
    /// acceptance criterion, computed the same way every other confirm
    /// button in this app is gated. Delegates to [`pull_confirm_enabled`],
    /// which is where the rule is host-tested.
    pub fn pull_enabled(&self) -> bool {
        pull_confirm_enabled(self.confirm_strategy.get())
    }

    /// Close the picker without dispatching anything — the Cancel tap, or a
    /// guarded backdrop dismiss. Clears the target too, so a stray re-render
    /// between this call and the next open can never show a stale one.
    pub fn close_pull_picker(&self) {
        self.close(Dialog::PullStrategy);
        self.pull_target.set(None);
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
        // ADR 0044: "no pre-selected option" has to mean every pull, not
        // only the first — reset unconditionally here, the same chokepoint
        // as `confirm_armed` above, rather than trusting each opener to
        // remember to clear it. Harmless for every dialog that isn't the
        // pull picker.
        self.confirm_strategy.set(None);
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
