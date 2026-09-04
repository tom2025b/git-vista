//! The stash drawer's reactive wrapper — wasm only (M3.24, #77).
//!
//! Every *decision* in here belongs to [`crate::features::stash::core`] and is
//! host-tested there. What this module owns is the HTTP sequencing and the
//! signals a view reads, because neither can be host-compiled: `mod api` is
//! `#[cfg(target_arch = "wasm32")]`.
//!
//! # The composed pop
//!
//! [`compose_pop`] is the reason this module exists. There is no
//! `/api/stash/pop`, so a pop is apply → read the conflict state → drop, and
//! the middle step is what makes A4 true. The gate is
//! [`core::drop_gate`]; this function does not decide anything the gate
//! decides, which is exactly why the gate is somewhere a test can reach it.

use leptos::*;

use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::DropContext;

use crate::api;
use crate::features::stash::core::{
    self as core, ApplyOutcome, ConflictScan, DropGate, DropOutcome, PopVerdict, StashWriteOutcome,
};
// #612: `PUSH_KEY`, `DrawerBusy` and `StashNotice` are decisions, not markup,
// and now live in `core` where a host test can reach them. Re-exported rather
// than repointed at every call site: `view.rs` imports all three from here and
// the names it writes are unchanged, so the move is provably behaviour-free.
pub use crate::features::stash::core::{DrawerBusy, StashNotice, PUSH_KEY};

/// Run a pop as apply-then-drop, returning what actually happened.
///
/// # Why the conflict scan runs even after a refused apply
///
/// [`core::drop_gate`] ignores the scan when the apply was refused, so the read
/// is wasted on that path — one `GET /api/conflicts` on a request that had
/// already failed. That cost is bought deliberately: the alternative is a
/// second entry point ("has the apply already settled this?") that a caller
/// could reach for *instead of* the gate, and the whole point of the gate is
/// that there is exactly one way to decide whether the destructive half runs.
/// One redundant GET on an error path is cheaper than a second door.
///
/// The `expected_oid` sent to the drop is the **same** one the apply carried —
/// re-read from nothing, recomputed from nothing. An apply does not renumber
/// the list, and the server compare-and-swaps the pair again itself before
/// mutating, so a list that moved in between is refused there rather than
/// papered over here.
pub async fn compose_pop(entry: &str, expected_oid: &str, key: IdempotencyKey) -> PopVerdict {
    // Classification is [`core::ApplyOutcome::from_write`]'s, host-tested:
    // an answered or record-recovered refusal is Refused, a lost reply whose
    // record could not settle it is Unknown — never Refused (#515). The old
    // arm here mapped every `Err` to Refused, which dressed transport loss
    // as a server decision.
    let sent = api::apply_stash_request(entry, expected_oid).await;
    // #514: the drop half must name the apply it is completing, so the server
    // can prove the tree still holds what that apply restored. Captured before
    // classification, because `ApplyOutcome` deliberately keeps only the
    // decision and not the wire detail.
    let applied_operation = match &sent {
        Ok(StashWriteOutcome::Answered { operation, .. })
        | Ok(StashWriteOutcome::Reconciled { operation, .. }) => operation.clone(),
        _ => None,
    };
    let apply = ApplyOutcome::from_write(sent);

    let scan = ConflictScan::from_fetch(api::fetch_conflicts().await);

    match core::drop_gate(&apply, &scan) {
        DropGate::Halt(verdict) => verdict,
        // No id, no drop. An apply that landed but whose operation could not
        // be named leaves nothing to prove the tree with, and the honest
        // answer is to stop rather than fall back to the unchecked drop this
        // whole change exists to remove. The entry stays, the applied changes
        // stay, and the verdict says the pop did not finish.
        DropGate::Run if applied_operation.is_none() => PopVerdict::AppliedNotDropped {
            why: "the apply succeeded but the server did not name an operation for it, \
                  so nothing could prove your changes are still in the working tree"
                .to_string(),
        },
        DropGate::Run => {
            // Same classifier discipline for the destructive half. An
            // answered 409 is a refusal (the status rides inside the
            // outcome, so the outer `Ok` still cannot be read as "it
            // worked"); a lost reply is Unknown, and the verdict says the
            // entry's fate was not observed rather than asserting it.
            let outcome = DropOutcome::from_write(
                api::drop_stash_request(
                    entry,
                    expected_oid,
                    key,
                    DropContext::CompletingPop {
                        applied_operation: applied_operation
                            .expect("the None case is handled by the guard arm above"),
                    },
                )
                .await,
            );
            core::verdict_after_drop(&outcome)
        }
    }
}

/// The drawer's own signals, created once by the Activity panel.
#[derive(Clone, Copy)]
pub struct StashDrawer {
    /// Which selectors are mid-write, each with its in-flight label.
    busy: RwSignal<DrawerBusy>,
    /// The patch of the entry currently expanded for inspection, keyed by
    /// selector. `None` means nothing is expanded — inspection is opt-in per
    /// row, not a fetch on every render.
    inspecting: RwSignal<Option<String>>,
    /// The last thing that happened, as a finished user-facing line.
    notice: RwSignal<Option<StashNotice>>,
}

impl StashDrawer {
    pub fn new() -> Self {
        StashDrawer {
            busy: create_rw_signal(DrawerBusy::default()),
            inspecting: create_rw_signal(None),
            notice: create_rw_signal(None),
        }
    }

    pub fn busy(&self) -> DrawerBusy {
        self.busy.get()
    }

    pub fn notice(&self) -> Option<StashNotice> {
        self.notice.get()
    }

    pub fn set_notice(&self, notice: StashNotice) {
        self.notice.set(Some(notice));
    }

    pub fn clear_notice(&self) {
        self.notice.set(None);
    }

    /// Which selector is expanded for inspection, if any.
    pub fn inspecting(&self) -> Option<String> {
        self.inspecting.get()
    }

    /// Toggle inspection for one entry. Tapping the open one closes it, so the
    /// control is its own undo.
    pub fn toggle_inspect(&self, selector: &str) {
        self.inspecting.update(|current| {
            if current.as_deref() == Some(selector) {
                *current = None;
            } else {
                *current = Some(selector.to_string());
            }
        });
    }

    /// Mark this entry mid-write, through [`DrawerBusy::begin`] — which owns
    /// the per-entry rule (#518) and is host-tested there.
    pub fn begin(&self, key: &str, what: &'static str) {
        self.busy.update(|busy| busy.begin(key, what));
    }

    /// Release this entry only, through [`DrawerBusy::finish`]. Same reason
    /// the key is required: an unconditional "everything idle" let whichever
    /// of two overlapping writes finished first unlock the one still in
    /// flight (#518).
    pub fn finish(&self, key: &str) {
        self.busy.update(|busy| busy.finish(key));
    }
}

impl Default for StashDrawer {
    fn default() -> Self {
        Self::new()
    }
}
