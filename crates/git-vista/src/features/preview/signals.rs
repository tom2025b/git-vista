//! The preview panel's reactive wrapper — wasm only (M10.08 A6, #594).
//!
//! Every *decision* belongs to [`super::core`] and [`super::scene`], both
//! host-tested. What lives here is the part neither can hold: the two HTTP
//! round trips, and the one signal a dialog reads.
//!
//! # The generation tag, and what it is actually for
//!
//! A preview is slow (two round trips, one of them running real git against a
//! scratch object store) and a confirm dialog is fast — a user can open it,
//! cancel, and open a *different* one well before the first answer lands.
//! Without a tag the late reply paints the new dialog with the old
//! operation's picture, and it is a plausible-looking picture: same repository,
//! same shape, wrong operation. Nothing about it announces itself as stale.
//!
//! So every [`Preview::start`] and every [`Preview::clear`] bumps a counter,
//! the request carries the value it was issued under, and a reply whose tag no
//! longer matches is **dropped** rather than shown. Closing the dialog clears,
//! which means the guard covers cancel-then-reopen without the caller having
//! to think about it.
//!
//! This is the client half of #594's acceptance point 5 ("cancelling
//! mid-preview leaves nothing behind"). The server half already holds: the
//! engine writes only into a scratch store it sweeps.

use leptos::*;

use git_vista_protocol::GitOperation;

use crate::api;
use crate::features::freshness::core::{
    slot_when_request_failed, slot_when_requested, PlanOnScreen, PlanSlot,
};
use crate::features::preview::core::{view_of, PreviewView};

/// What the panel is showing right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewSlot {
    /// This dialog has no preview — either it is not one of the previewable
    /// operations, or no dialog is open. The panel renders nothing at all.
    Idle,
    /// A round trip is in flight.
    Pending,
    /// The server answered. **Including its refusals**: a conflict, an
    /// unsupported operation and an unavailable host all arrive here, because
    /// each is an answer rather than a failure of the request.
    Ready(PreviewView),
    /// The round trip itself failed — offline, refused by a client guard, a
    /// non-2xx status, a timeout.
    ///
    /// Deliberately distinct from `Ready(PreviewView::Unavailable { .. })`.
    /// The server saying "this host's git is too old" is a fact about the
    /// repository; the fetch never arriving is a fact about the connection,
    /// and telling a user the second when the first is true (or the reverse)
    /// sends them somewhere useless.
    Failed(String),
}

/// Proof that a rebuild-lease continuation is still the one that started it
/// — nothing has bumped `Preview`'s generation since (#664 review round 3;
/// see [`Preview::note_rebuild_started`]'s doc comment for the defect this
/// closes). The only way to mint one is `note_rebuild_started`; the only
/// ways to spend one are `note_rebuild_failed`, `note_rebuild_landed` and
/// `rebuild_is_current` — a continuation cannot write state or act on a
/// stale rebuild without presenting a token, and presenting a stale one is
/// simply inert rather than a check the caller could forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebuildToken(u64);

/// The preview panel's state, owned by `App` and handed down in `Features`.
#[derive(Clone, Copy)]
pub struct Preview {
    slot: RwSignal<PreviewSlot>,
    /// The plan this panel is showing a picture of, as M12.05 (#555) needs it:
    /// the generation it was built against, and the refs it expects to move.
    ///
    /// Kept because the picture is what the user approves. When the repository
    /// moves under it the panel must say so and the confirm control must be
    /// withdrawn — and neither is answerable without the plan's own generation.
    /// It is deliberately *not* re-fetched on a change: a plan that quietly
    /// re-derives itself is a plan the user did not approve.
    ///
    /// A [`PlanSlot`] rather than an `Option`, because "no plan yet" and "the
    /// stale plan I just threw away to make room for a replacement" must never
    /// be the same value — see `PlanSlot`'s own doc for what collapsing them
    /// cost.
    plan: RwSignal<PlanSlot>,
    /// Bumped by every start and every clear. A reply whose captured value no
    /// longer matches is discarded.
    ///
    /// A `StoredValue`, not a signal: nothing renders from it, and making it
    /// reactive would re-run every reader on a bookkeeping write.
    generation: StoredValue<u64>,
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

impl Preview {
    pub fn new() -> Self {
        Self {
            slot: create_rw_signal(PreviewSlot::Idle),
            plan: create_rw_signal(PlanSlot::Absent),
            generation: store_value(0),
        }
    }

    /// A tracked read — the panel re-renders from it.
    pub fn slot(&self) -> PreviewSlot {
        self.slot.get()
    }

    /// The plan on screen, and where it is in its life. A tracked read.
    pub fn plan(&self) -> PlanSlot {
        self.plan.get()
    }

    /// Ask for the preview of `op`, discarding whatever was on screen.
    ///
    /// Fires two requests in sequence: `/api/plan` for a [`Plan`], then
    /// `/api/preview` for the before/after graphs. The dialog's text is
    /// already on screen by the time this is called and never waits for it —
    /// the panel fills in beside text that is already readable.
    ///
    /// [`Plan`]: git_vista_protocol::Plan
    pub fn start(&self, op: GitOperation) {
        self.fetch(op, false);
    }

    /// Replace the plan on screen with one built against the repository as it
    /// is now — spec D4's **Rebuild**.
    ///
    /// The only difference from [`start`](Self::start) is what the slot says
    /// while the request is in flight and if it fails, and that difference is
    /// the whole point: this call follows a plan the user was just told is
    /// stale, so until a replacement actually arrives there is nothing to
    /// approve. `start`'s `Absent` would re-enable the button on the strength
    /// of having discarded the evidence.
    pub fn rebuild(&self, op: GitOperation) {
        self.fetch(op, true);
    }

    fn fetch(&self, op: GitOperation, rebuilding: bool) {
        let issued = self.bump();
        self.slot.set(PreviewSlot::Pending);
        self.plan.set(slot_when_requested(rebuilding));
        let slot = self.slot;
        let on_screen = self.plan;
        let generation = self.generation;
        spawn_local(async move {
            let (outcome, plan) = match api::plan_request(&op).await {
                Ok(plan) => {
                    // #555: what the user is about to approve, remembered
                    // before the picture is drawn — the generation the plan was
                    // built against and the refs it says it will move.
                    let on_screen = PlanOnScreen {
                        generation: plan.generation.as_str().to_string(),
                        expects: plan
                            .expected_ref_changes
                            .iter()
                            .map(|change| change.ref_name.as_str().to_string())
                            .collect(),
                    };
                    match api::preview_request(&plan).await {
                        Ok(response) => (
                            PreviewSlot::Ready(view_of(response)),
                            PlanSlot::Ready(on_screen),
                        ),
                        // The picture failed, but the PLAN arrived — and the
                        // plan is what freshness is about. #594's rule that a
                        // preview informs and never gates is exactly why this
                        // is `Ready` and not a failure.
                        Err(why) => (PreviewSlot::Failed(why), PlanSlot::Ready(on_screen)),
                    }
                }
                Err(why) => (
                    PreviewSlot::Failed(why),
                    slot_when_request_failed(rebuilding),
                ),
            };
            // The stale-response guard. `try_get_value` rather than
            // `get_value`: this future outlives nothing today, but a disposed
            // owner must drop the reply, never panic inside a browser.
            if generation.try_get_value() == Some(issued) {
                slot.set(outcome);
                on_screen.set(plan);
            }
        });
    }

    /// Blank the panel and invalidate anything in flight.
    ///
    /// Called when the confirm dialog closes and when it re-opens on an
    /// operation with no preview, so a reply that was already on the wire
    /// cannot paint a dialog it was never asked about.
    pub fn clear(&self) {
        self.bump();
        self.slot.set(PreviewSlot::Idle);
        // The plan goes with the picture. A generation left behind would make
        // the *next* dialog answer a freshness question about the last one's
        // plan — the same class of stale-reply defect the tag above exists to
        // stop, one field over.
        self.plan.set(PlanSlot::Absent);
    }

    /// Note that a rebuild this `Preview` is not itself fetching has begun,
    /// and return the token its completion must present.
    ///
    /// The force-with-lease confirmation's plan does not come from here — it
    /// has no graph preview at all — but its Rebuild has the same two states,
    /// and they are held here so the dialog reads one slot rather than two.
    ///
    /// # Why this returns a token (#664 review round 3)
    ///
    /// `fetch`'s own generation guard (this module's doc comment) only
    /// protects state `fetch` itself writes. A rebuild-lease continuation is
    /// two round trips run entirely outside `fetch`, so nothing checked its
    /// generation before writing `RebuildFailed`/`Absent` or before
    /// re-opening the confirmation dialog — a held response, released after
    /// Cancel (which bumps via [`Self::clear`]) or after a newer rebuild
    /// (which bumps again via this method), would act as though it were
    /// still current. `RebuildToken` makes that unrepresentable: the only
    /// way to mint one is to call this method, and the only ways to spend
    /// one are [`Self::note_rebuild_failed`], [`Self::note_rebuild_landed`]
    /// and [`Self::rebuild_is_current`] — each checks it against the live
    /// generation before doing anything, so a stale token is inert rather
    /// than a convention a caller has to remember to honour.
    pub fn note_rebuild_started(&self) -> RebuildToken {
        let issued = self.bump();
        self.plan.set(PlanSlot::Rebuilding);
        RebuildToken(issued)
    }

    /// Note that such a rebuild did not produce a plan — unless `token` is no
    /// longer current, in which case this does nothing: a newer rebuild or a
    /// cancel already wrote whatever state now holds, and this stale reply
    /// must not overwrite it.
    pub fn note_rebuild_failed(&self, token: RebuildToken) {
        if self.rebuild_is_current(token) {
            self.plan.set(PlanSlot::RebuildFailed);
        }
    }

    /// Note that such a rebuild landed, and the replacement plan is now
    /// carried by the operation itself — unless `token` is no longer
    /// current, in which case this does nothing, for the same reason
    /// [`Self::note_rebuild_failed`] guards: a newer rebuild's own
    /// `Rebuilding` (or a cancel's `Absent`) must not be clobbered by a
    /// reply to the rebuild it replaced.
    ///
    /// A *current* call is still required, and its absence would be a defect
    /// of exactly the shape this whole slot exists to prevent: `Rebuilding`
    /// outranks the carried plan in `plan_on_screen`, so a rebuild that never
    /// says it finished leaves the confirmation disabled over a replacement
    /// that did arrive.
    pub fn note_rebuild_landed(&self, token: RebuildToken) {
        if self.rebuild_is_current(token) {
            self.plan.set(PlanSlot::Absent);
        }
    }

    /// Whether a rebuild started with `token` is still the live one —
    /// nothing (Cancel, a newer rebuild, the dialog closing and reopening)
    /// has bumped the generation since. `note_rebuild_failed` and
    /// `note_rebuild_landed` already check this before writing; call it
    /// directly for an action this `Preview` does not itself own, such as
    /// re-opening the confirmation dialog on the replacement plan (#664
    /// review round 3).
    pub fn rebuild_is_current(&self, token: RebuildToken) -> bool {
        // The comparison itself lives in `core::rebuild_token_is_current`,
        // host-tested — this wrapper only supplies the wasm-only signal read
        // (#664 review round 3; see that function's own doc comment).
        crate::features::preview::core::rebuild_token_is_current(
            self.generation.try_get_value(),
            token.0,
        )
    }

    /// Take the next generation and return it.
    fn bump(&self) -> u64 {
        let next = self
            .generation
            .try_get_value()
            .unwrap_or_default()
            .wrapping_add(1);
        self.generation.set_value(next);
        next
    }
}

// ---------------------------------------------------------------------------
// The animation clock (#591).
// ---------------------------------------------------------------------------

/// Whether the platform has asked for less motion.
///
/// A one-time read at the moment a picture becomes ready, not a live
/// subscription — the animation only ever runs once per fresh preview (see
/// [`Playback::start`]), so there is no ongoing animation for a later
/// preference change to interrupt. `false` on any host that cannot answer
/// (no `window`, `matchMedia` unsupported): every one of those is safer to
/// treat as "no preference stated" than as "reduce", since the honest
/// fallback either way is the same static after-picture this panel already
/// draws — the animation is purely additive.
pub fn prefers_reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok())
        .flatten()
        .is_some_and(|m| m.matches())
}

/// Drives the before→after animation's progress clock.
///
/// Every *decision* about what progress means — how it maps to a pixel, an
/// opacity, whether a label may show — lives in
/// [`crate::features::preview::tween`], host-tested. What this holds is the
/// one thing a pure function cannot: a running clock, ticked by
/// `request_animation_frame` and read by [`Playback::progress`], which the
/// wasm-only [`crate::dialogs::preview_panel`] view samples every frame.
#[derive(Clone, Copy)]
pub struct Playback {
    /// `tween::progress_at(elapsed)`, recomputed every tick.
    progress: RwSignal<f64>,
    /// Bumped by every [`Playback::start`], so a `request_animation_frame`
    /// callback scheduled under a superseded run stops rescheduling itself
    /// rather than fighting a newer run for the same signal — the same
    /// stale-reply guard [`Preview::generation`] uses, for the same reason.
    generation: StoredValue<u64>,
}

impl Default for Playback {
    fn default() -> Self {
        Self::new()
    }
}

impl Playback {
    pub fn new() -> Self {
        Self {
            progress: create_rw_signal(0.0),
            generation: store_value(0),
        }
    }

    /// A tracked read — the animated view re-renders from it every tick.
    pub fn progress(&self) -> f64 {
        self.progress.get()
    }

    /// Start (or restart) the transition from its beginning.
    ///
    /// `reduced_motion` jumps straight to the resting frame and schedules no
    /// frame at all — the rule #591 states explicitly: the animation must
    /// degrade to the end state rather than ever being the only way to see
    /// the result, and the cheapest way to guarantee that is to never enter
    /// the loop that could show anything else.
    pub fn start(&self, reduced_motion: bool) {
        let mine = self.bump();
        if reduced_motion {
            self.progress.set(1.0);
            return;
        }
        self.progress.set(0.0);
        self.schedule(mine, now_ms());
    }

    fn bump(&self) -> u64 {
        let next = self
            .generation
            .try_get_value()
            .unwrap_or_default()
            .wrapping_add(1);
        self.generation.set_value(next);
        next
    }

    /// Schedule the next frame, and every frame after it until progress
    /// reaches `1.0` or a later [`Playback::start`] supersedes `mine`.
    fn schedule(&self, mine: u64, started_at: f64) {
        let this = *self;
        request_animation_frame(move || {
            // Superseded (a Replay, or a new preview loaded) — stop, and
            // leave whatever the newer run already set alone. `try_get_value`
            // rather than `get_value`: this callback outlives nothing today,
            // but a disposed owner must drop the tick, never panic.
            if this.generation.try_get_value() != Some(mine) {
                return;
            }
            let elapsed = now_ms() - started_at;
            let t = crate::features::preview::tween::progress_at(elapsed);
            this.progress.set(t);
            if t < 1.0 {
                this.schedule(mine, started_at);
            }
        });
    }
}

/// Wall-clock milliseconds, for measuring elapsed animation time. Not
/// monotonic (a system clock adjustment could move it), but the same
/// primitive this codebase already uses for elapsed-time arithmetic
/// (`api.rs`'s request-id noise, `datetime.rs`'s relative-time math) rather
/// than reaching for `Performance`, which would cost this crate a new
/// web-sys feature for no practical gain at animation-frame granularity.
fn now_ms() -> f64 {
    js_sys::Date::now()
}
