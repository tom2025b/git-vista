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

/// The preview panel's state, owned by `App` and handed down in `Features`.
#[derive(Clone, Copy)]
pub struct Preview {
    slot: RwSignal<PreviewSlot>,
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
            generation: store_value(0),
        }
    }

    /// A tracked read — the panel re-renders from it.
    pub fn slot(&self) -> PreviewSlot {
        self.slot.get()
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
        let issued = self.bump();
        self.slot.set(PreviewSlot::Pending);
        let slot = self.slot;
        let generation = self.generation;
        spawn_local(async move {
            let outcome = match api::plan_request(&op).await {
                Ok(plan) => match api::preview_request(&plan).await {
                    Ok(response) => PreviewSlot::Ready(view_of(response)),
                    Err(why) => PreviewSlot::Failed(why),
                },
                Err(why) => PreviewSlot::Failed(why),
            };
            // The stale-response guard. `try_get_value` rather than
            // `get_value`: this future outlives nothing today, but a disposed
            // owner must drop the reply, never panic inside a browser.
            if generation.try_get_value() == Some(issued) {
                slot.set(outcome);
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
