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

use crate::api;
use crate::features::stash::core::{
    self as core, ApplyOutcome, ConflictScan, DropGate, DropOutcome, PopVerdict, TreeState,
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
    let apply = match api::apply_stash_request(entry, expected_oid).await {
        Ok(()) => ApplyOutcome::Applied,
        Err(why) => ApplyOutcome::Refused(why),
    };

    let scan = ConflictScan::from_fetch(api::fetch_conflicts().await);

    match core::drop_gate(&apply, &scan) {
        DropGate::Halt(verdict) => verdict,
        DropGate::Run => {
            let outcome = match api::drop_stash_request(entry, expected_oid, key).await {
                // A receipt is not a success. `send_write_with_key` returns
                // `Ok` for any answered request, including a 409 — the status
                // lives in `receipt.ok`, and reading the `Ok` as "it worked"
                // is how a failed drop would be reported as a finished pop.
                Ok(receipt) if receipt.ok => DropOutcome::Dropped,
                Ok(receipt) => DropOutcome::Refused(receipt.message),
                Err(why) => DropOutcome::Refused(why),
            };
            core::verdict_after_drop(&outcome)
        }
    }
}

/// What the drawer is currently doing, so a view can disable controls and say
/// why without inventing its own notion of "busy".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawerBusy {
    Idle,
    /// A write is in flight against this selector. Held per-selector rather
    /// than as one global flag: the drawer lists many entries and locking all
    /// of them because one is being dropped would be a lie about the others.
    Working {
        selector: String,
        what: &'static str,
    },
}

impl DrawerBusy {
    /// Whether this selector specifically is mid-write.
    pub fn locked(&self, selector: &str) -> bool {
        matches!(self, DrawerBusy::Working { selector: s, .. } if s == selector)
    }

    /// The label to show on the row that is working.
    pub fn label(&self, selector: &str) -> Option<&'static str> {
        match self {
            DrawerBusy::Working { selector: s, what } if s == selector => Some(what),
            _ => None,
        }
    }
}

/// The drawer's own signals, created once by the Activity panel.
#[derive(Clone, Copy)]
pub struct StashDrawer {
    /// Which selector, if any, is mid-write.
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
    /// Whether the stash entry is still in the drawer.
    pub entry_retained: bool,
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

    /// A notice for the simple writes, which either worked or did not.
    pub fn from_result(result: Result<(), String>, done: &str) -> Self {
        match result {
            // The simple writes each say their own effect in `done`, and a
            // second structural line would repeat it — so `tree` is None here
            // rather than a guess. Only a composed pop has an effect the prose
            // cannot fully carry.
            Ok(()) => StashNotice {
                headline: done.to_string(),
                complete: true,
                tree: None,
                entry_retained: false,
                conflicted: Vec::new(),
                unreadable: Vec::new(),
            },
            Err(why) => StashNotice {
                headline: why,
                complete: false,
                tree: None,
                entry_retained: true,
                conflicted: Vec::new(),
                unreadable: Vec::new(),
            },
        }
    }
}

impl StashDrawer {
    pub fn new() -> Self {
        StashDrawer {
            busy: create_rw_signal(DrawerBusy::Idle),
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

    pub fn begin(&self, selector: &str, what: &'static str) {
        self.busy.set(DrawerBusy::Working {
            selector: selector.to_string(),
            what,
        });
    }

    pub fn finish(&self) {
        self.busy.set(DrawerBusy::Idle);
    }
}

impl Default for StashDrawer {
    fn default() -> Self {
        Self::new()
    }
}
