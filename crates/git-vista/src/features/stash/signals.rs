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
    TreeState,
};

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

/// The busy key for the push control, which has no stash entry of its own.
///
/// A reserved sentinel rather than `""`: an empty string silently matches no
/// row, so a push in flight left every control enabled — including its own
/// button, where a second tap would stash the already-stashed tree again. The
/// value cannot collide with a real key: rows key on the entry's commit OID,
/// which is hex, and this is not.
pub const PUSH_KEY: &str = "\u{0}push";

/// What the drawer is currently doing, so a view can disable controls and say
/// why without inventing its own notion of "busy".
///
/// One entry per stash entry with a write in flight, not one overwriteable
/// slot (#518): the drawer lists many entries and writes overlap — start an
/// apply on one row and then a drop on another, and a single slot would
/// re-enable the first row mid-flight, then let whichever write finished
/// first unlock everything.
///
/// # Keys are the entry's commit OID, never its selector
///
/// `stash@{N}` is a *position*, and positions renumber on every drop: an
/// apply in flight on `stash@{1}` while a drop on `stash@{0}` completes
/// would leave a selector-keyed lock pointing at whichever entry the list
/// now shows at `{1}` — the wrong row locked, the working row free. The
/// commit OID names the entry itself, which no renumbering moves. (Found by
/// the #518 review pass, both reviewers independently.) [`PUSH_KEY`] is the
/// one non-OID key, for the control with no entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DrawerBusy {
    working: std::collections::HashMap<String, &'static str>,
}

impl DrawerBusy {
    /// Whether this entry specifically is mid-write. `key` is the entry's
    /// commit OID (or [`PUSH_KEY`]) — see the type doc for why never a
    /// selector.
    pub fn locked(&self, key: &str) -> bool {
        self.working.contains_key(key)
    }

    /// The label to show on the row that is working.
    pub fn label(&self, key: &str) -> Option<&'static str> {
        self.working.get(key).copied()
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

/// A finished, user-facing outcome line plus the conflicted paths it carries.
///
/// The paths ride along so the view can offer a route into the shared conflict
/// workflow (A3) rather than rendering a stash-shaped conflict UI of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashNotice {
    pub headline: String,
    /// True only when the operation genuinely finished. A view must scale its
    /// styling on this and never on "the request returned".
    pub complete: bool,
    /// What is true of the user's working tree, carried structurally rather
    /// than left only inside [`Self::headline`]'s prose. A user whose pop
    /// "failed" still needs to know their files moved — and after a refused
    /// apply with an unreadable tree, that it could not be established.
    pub tree: Option<TreeState>,
    /// Whether the stash entry this action targeted is still in the drawer —
    /// `None` when this client has no way to know (a lost reply whose record
    /// could not settle it, #515), or when the action has no target entry
    /// (push). A `bool` here forced a guess, and the guess shipped as fact.
    pub entry_retained: Option<bool>,
    pub conflicted: Vec<String>,
    pub unreadable: Vec<String>,
}

impl StashNotice {
    /// Build the notice for a composed pop. Every field comes from the
    /// verdict's own accessors, so the view cannot disagree with the gate about
    /// whether the pop finished.
    pub fn from_pop(verdict: &PopVerdict) -> Self {
        StashNotice {
            headline: verdict.headline(),
            complete: verdict.is_complete(),
            tree: Some(verdict.tree()),
            entry_retained: verdict.entry_retained(),
            conflicted: verdict.conflicted_paths().to_vec(),
            unreadable: verdict.unreadable_paths().to_vec(),
        }
    }

    /// A notice for the simple writes, honest about lost replies (#515).
    ///
    /// The caller states the entry's fate PER OUTCOME, because the truth
    /// differs per action: a successful drop removed the entry
    /// (`Some(false)`), a successful apply kept it (`Some(true)`), a push
    /// has no target entry at all (`None`) — and an unrecoverable lost reply
    /// is `None` for every action whose success would have changed the
    /// drawer, because a value here is a claim the user acts on.
    ///
    /// `tree` stays `None` throughout: the simple writes say their effect in
    /// prose, and only a composed pop has an effect the prose cannot fully
    /// carry (see [`Self::from_pop`]).
    pub fn from_write(
        sent: Result<StashWriteOutcome, String>,
        done: &str,
        entry_on_success: Option<bool>,
        entry_on_failure: Option<bool>,
        unknown: (&str, Option<bool>),
    ) -> Self {
        let (headline, complete, entry_retained) = match sent {
            // A local refusal never left the device, so nothing changed.
            Err(local) => (local, false, entry_on_failure),
            Ok(StashWriteOutcome::Answered { ok: true, .. })
            | Ok(StashWriteOutcome::Reconciled { ok: true, .. }) => {
                (done.to_string(), true, entry_on_success)
            }
            Ok(StashWriteOutcome::Answered {
                ok: false, message, ..
            })
            | Ok(StashWriteOutcome::Reconciled {
                ok: false, message, ..
            }) => (message, false, entry_on_failure),
            // Lost and unrecoverable: the one arm that may not claim an
            // outcome. `complete` is false NOT because it failed — nobody
            // knows — but because a view must never style this as done.
            Ok(StashWriteOutcome::Unknown { why }) => {
                let (hint, entry) = unknown;
                (format!("{hint}\n\n{why}"), false, entry)
            }
        };
        StashNotice {
            headline,
            complete,
            tree: None,
            entry_retained,
            conflicted: Vec::new(),
            unreadable: Vec::new(),
        }
    }
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

    /// Mark this entry mid-write. `key` is the entry's commit OID (or
    /// [`PUSH_KEY`]); inserts into the per-entry map rather than replacing a
    /// single slot, so an overlapping write on another row neither relabels
    /// nor re-enables this one (#518).
    pub fn begin(&self, key: &str, what: &'static str) {
        self.busy.update(|busy| {
            busy.working.insert(key.to_string(), what);
        });
    }

    /// Release this entry only. Takes the key for the same reason `begin`
    /// does: an unconditional "everything idle" let whichever of two
    /// overlapping writes finished first unlock the one still in flight.
    pub fn finish(&self, key: &str) {
        self.busy.update(|busy| {
            busy.working.remove(key);
        });
    }
}

impl Default for StashDrawer {
    fn default() -> Self {
        Self::new()
    }
}
