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
use crate::features::freshness::core::PlanOnScreen;
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
    /// The plan this panel is showing a picture of, as M12.05 (#555) needs it:
    /// the generation it was built against, and the refs it expects to move.
    ///
    /// Kept because the picture is what the user approves. When the repository
    /// moves under it the panel must say so and the confirm control must be
    /// withdrawn — and neither is answerable without the plan's own generation.
    /// It is deliberately *not* re-fetched on a change: a plan that quietly
    /// re-derives itself is a plan the user did not approve.
    plan: RwSignal<Option<PlanOnScreen>>,
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
            plan: create_rw_signal(None),
            generation: store_value(0),
        }
    }

    /// A tracked read — the panel re-renders from it.
    pub fn slot(&self) -> PreviewSlot {
        self.slot.get()
    }

    /// The plan on screen, if one was built. A tracked read.
    pub fn plan(&self) -> Option<PlanOnScreen> {
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
        let issued = self.bump();
        self.slot.set(PreviewSlot::Pending);
        self.plan.set(None);
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
                        Ok(response) => (PreviewSlot::Ready(view_of(response)), Some(on_screen)),
                        Err(why) => (PreviewSlot::Failed(why), Some(on_screen)),
                    }
                }
                Err(why) => (PreviewSlot::Failed(why), None),
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
        self.plan.set(None);
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
