//! Modal state: which dialog is up, and the ghost-click guard that decides whether a
//! backdrop tap is a real dismiss (M1.11, #64).
//!
//! Framework-free (M1.11 D1), so the guard arithmetic is unit-tested on the host instead
//! of only ever being exercised by a thumb on an iPad.
//!
//! Before M1.11 there was no guard *abstraction* at all. There were **three** independent
//! `StoredValue<f64>` clocks — `Overlays::dialog_opened_at` (shared by the commit and
//! confirm modals), `open_opened_at` (the Open-URL modal, also written by the picker) and
//! `reset_opened_at` (the reset modal) — and every one of the eleven open sites inlined
//! `set_value(js_sys::Date::now())` while every one of the four dismiss sites inlined
//! `js_sys::Date::now() - guard.get_value() > DIALOG_GUARD_MS`. The rule was correct in all
//! four places by repetition, not by construction.
//!
//! Collapsing them into one clock is safe *because* this core also records **which** dialog
//! that clock belongs to: [`DialogsCore::open`] replaces, so at most one guarded dialog is
//! ever the guarded one. The three separate clocks were never deliberate isolation — they
//! were three copies of the same idea that happened never to overlap.

/// A modal that dismisses by backdrop tap, and therefore needs the guard.
///
/// The full-screen viewer (`viewer.rs`) is deliberately **not** here: it has no backdrop
/// dismiss at all, only an explicit Close button, so it has never consulted the guard.
/// Adding it would be new behaviour, not an extraction of existing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    /// The commit-message dialog (Issue #33) — `Overlays::commit_dialog`.
    Commit,
    /// The branch-operation / undo confirmation (Issue #33 follow-up) — `Overlays::confirm_op`.
    Confirm,
    /// The "Reset Test Repo" confirmation.
    Reset,
    /// The "Open URL…" clone prompt, reachable from the topbar and from the picker.
    OpenUrl,
}

/// How long (ms) after a modal opens to ignore a backdrop dismiss.
///
/// iOS synthesizes a "click" a few ms after the real tap that opened the modal. The modal's
/// full-screen backdrop lands under that tap point, so without the guard the ghost click
/// dismisses the modal it just opened. 400 ms was tuned on the iPad this app is used from;
/// it is comfortably longer than the synthesized delay and shorter than any deliberate
/// second tap.
pub const DIALOG_GUARD_MS: f64 = 400.0;

/// Which guarded modal is up, and when it opened.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DialogsCore {
    open: Option<Dialog>,
    /// When the open dialog opened, in milliseconds (a `js_sys::Date::now()` reading in
    /// the browser, an arbitrary monotonic-enough scale in tests).
    ///
    /// `None`, not the old `store_value(0.0_f64)` sentinel. `0.0` only behaved as
    /// "never opened" because `Date::now()` is ~1.7e12 and therefore always more than
    /// 400 ms past it — correct in the browser, but by accident of the epoch's origin
    /// rather than by construction. Stating it as `Option` means the guard cannot be
    /// wrong on any other clock scale.
    opened_at: Option<f64>,
}

impl DialogsCore {
    /// Record that `d` just opened at `now_ms`, and start its guard window.
    ///
    /// Always restamps, including when the same dialog is already open. That is not an
    /// edge case — `dialogs/confirm.rs`'s escalation effect reopens the confirm modal in
    /// place when a safe delete is refused as "not fully merged", swapping the pending
    /// operation for its force variant. The modal never visually closes, but the user is
    /// now looking at a *different* question, so the guard has to restart or the tap that
    /// answered the previous question could dismiss the new one.
    pub fn open(&mut self, d: Dialog, now_ms: f64) {
        self.open = Some(d);
        self.opened_at = Some(now_ms);
    }

    /// Clear the record if `d` is the dialog currently held.
    ///
    /// Matching on the dialog keeps a late close from a modal that has already been
    /// replaced from clearing its successor. The modal *signals* themselves are still
    /// separate and still closed directly by their own views; this core owning the whole
    /// open/close lifecycle is Task 8's overlay stack.
    pub fn close(&mut self, d: Dialog) {
        if self.open == Some(d) {
            self.open = None;
        }
    }

    /// The guarded dialog currently up, if any.
    pub fn open_dialog(&self) -> Option<Dialog> {
        self.open
    }

    /// Whether a dismiss arriving at `now_ms` should be honoured.
    ///
    /// `>` not `>=`, matching the comparison every inlined call site used, so the extraction
    /// cannot shift the boundary by one millisecond. Nothing ever opened means nothing is
    /// being protected, so the dismiss goes through.
    pub fn may_confirm(&self, now_ms: f64) -> bool {
        match self.opened_at {
            None => true,
            Some(at) => now_ms - at > DIALOG_GUARD_MS,
        }
    }
}

/// What the Open-URL dialog must do once its clone request settles (#260).
///
/// Extracted pure so the settlement rules are host-tested — `dialogs/open_url.rs`
/// is wasm-only and this crate has no wasm harness, the same gap that motivated
/// `print_button_copy`. The view consumes this by exhaustive destructuring (no
/// `..`), so adding a field here refuses to compile until the view handles it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneSettlement<T> {
    /// Clear the `cloning` busy flag. Both arms: the request is over either way.
    pub clear_busy: bool,
    /// Close the dialog. Success only — an error keeps it up so the URL and the
    /// user's context survive for a retry.
    pub close_dialog: bool,
    /// Clear the URL field. Success only, same reasoning as `close_dialog`.
    pub clear_url: bool,
    /// Re-read the graph. **Both arms — this is #260's recovery.** A timeout or
    /// dropped-tunnel error does not mean the clone failed: the server may have
    /// finished and already moved its current selection (`clone.rs` runs
    /// `set_current` before replying). The frame request follows the server's
    /// current selection, so bumping on the error arm makes a
    /// completed-but-lost clone appear instead of staying silently absent.
    ///
    /// Deliberately uniform: it also fires for definite failures (bad URL,
    /// offline pre-flight refusal), where the extra refetch is harmless noise.
    /// Splitting the arms needs a typed error out of `clone_request`, which
    /// belongs to #263's operation-tracking work, not here.
    pub bump_epoch: bool,
    /// Hand this descriptor to the Visualize/Active mode screen. Success only.
    pub mode_screen_for: Option<T>,
    /// Show this to the user. Error only.
    pub alert: Option<String>,
}

/// The settlement rules for a finished clone request, both arms.
pub fn clone_settlement<T>(outcome: Result<T, String>) -> CloneSettlement<T> {
    match outcome {
        Ok(descriptor) => CloneSettlement {
            clear_busy: true,
            close_dialog: true,
            clear_url: true,
            bump_epoch: true,
            mode_screen_for: Some(descriptor),
            alert: None,
        },
        Err(e) => CloneSettlement {
            clear_busy: true,
            close_dialog: false,
            clear_url: false,
            bump_epoch: true,
            mode_screen_for: None,
            // Self-qualifying on purpose: `clone_request` collapses every
            // failure into one string, so this can't tell a timeout (clone may
            // have finished) from a bad URL (it definitely didn't). "If this
            // was a network drop or timeout" lets the user apply the error
            // they can see; an unconditional "it may have finished" would
            // steer them wrong on definite failures.
            alert: Some(format!(
                "Couldn't clone:\n{e}\n\nIf this was a network drop or timeout, \
                 the clone may still have finished — check the repository \
                 picker before retrying, or a retry can create a duplicate \
                 clone."
            )),
        },
    }
}

/// Whether the Open-URL dialog may be dismissed right now (#260).
///
/// A clone in flight pins the dialog open: dismissing it doesn't cancel the
/// request, it just makes the app *look* idle while a clone is still running —
/// the "acted like it worked" half of #260. `guard_allows` is the existing
/// iOS ghost-click verdict ([`DialogsCore::may_confirm`]); this composes with
/// it rather than replacing it.
pub fn clone_dialog_may_dismiss(cloning: bool, guard_allows: bool) -> bool {
    !cloning && guard_allows
}

/// The sessionStorage key holding the commit-message draft for one repository
/// (#226). Keyed by the Frame's `worktree_id` so a draft typed against one
/// repository can never surface in another's commit dialog.
pub fn commit_draft_key(worktree_id: &str) -> String {
    format!("gv-commit-draft:{worktree_id}")
}

/// What the draft signal should do when the served repository (the draft
/// *scope*) is observed again (#226).
///
/// The scope is re-observed on **every** epoch reload — Refresh, a settled
/// write, drift — almost always with the same repository. Reseeding from
/// storage on those would clobber whatever the user has typed since the last
/// persist tick, so the rule is: reseed **only when the repository actually
/// changed**. That decision lives here, host-tested, because the signal
/// wiring around it is wasm-only and untestable in this repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftScopeAction {
    /// Same repository as before — leave the live signal alone.
    KeepSignal,
    /// A different repository (or the first one seen) — replace the signal
    /// with that repository's persisted draft, or blank if none.
    SeedFromStorage,
}

pub fn draft_scope_action(old: Option<&str>, new: Option<&str>) -> DraftScopeAction {
    if old == new {
        DraftScopeAction::KeepSignal
    } else {
        DraftScopeAction::SeedFromStorage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUARD_MS: f64 = DIALOG_GUARD_MS;

    #[test]
    fn a_confirm_inside_the_guard_window_is_refused() {
        // Protects against a tap that was already travelling when the dialog opened.
        let mut d = DialogsCore::default();
        d.open(Dialog::Confirm, 1_000.0);
        assert!(!d.may_confirm(1_000.0 + GUARD_MS - 1.0));
    }

    #[test]
    fn a_confirm_after_the_guard_window_is_allowed() {
        let mut d = DialogsCore::default();
        d.open(Dialog::Confirm, 1_000.0);
        assert!(d.may_confirm(1_000.0 + GUARD_MS + 1.0));
    }

    #[test]
    fn reopening_restamps_the_guard() {
        // The menu race made this load-bearing: if the dialog's target changes, the guard
        // must restart, not carry over from the previous open.
        let mut d = DialogsCore::default();
        d.open(Dialog::Confirm, 1_000.0);
        d.open(Dialog::Confirm, 5_000.0);
        assert!(!d.may_confirm(5_000.0 + GUARD_MS - 1.0));
    }

    #[test]
    fn a_never_opened_core_honours_a_dismiss() {
        // The `store_value(0.0_f64)` sentinel the three old clocks started from: nothing
        // is open, so nothing is being protected, and a stray dismiss must not be swallowed.
        let d = DialogsCore::default();
        assert_eq!(d.open_dialog(), None);
        assert!(d.may_confirm(1.0));
    }

    #[test]
    fn opening_a_second_dialog_replaces_the_first_and_restamps() {
        // Why one clock can replace the three: the core makes two guarded modals being
        // open at once unrepresentable, which is what the separate clocks were relying on
        // by luck.
        let mut d = DialogsCore::default();
        d.open(Dialog::Commit, 1_000.0);
        d.open(Dialog::Reset, 9_000.0);
        assert_eq!(d.open_dialog(), Some(Dialog::Reset));
        assert!(!d.may_confirm(9_000.0 + GUARD_MS - 1.0));
    }

    #[test]
    fn closing_a_dialog_that_is_no_longer_the_open_one_is_a_no_op() {
        let mut d = DialogsCore::default();
        d.open(Dialog::Commit, 1_000.0);
        d.open(Dialog::Confirm, 2_000.0);
        d.close(Dialog::Commit);
        assert_eq!(
            d.open_dialog(),
            Some(Dialog::Confirm),
            "a straggler close must not clear the dialog that replaced it"
        );
        d.close(Dialog::Confirm);
        assert_eq!(d.open_dialog(), None);
    }

    #[test]
    fn a_settled_clone_success_updates_everything_and_shows_the_mode_screen() {
        let s = clone_settlement(Ok("descriptor"));
        assert_eq!(
            s,
            CloneSettlement {
                clear_busy: true,
                close_dialog: true,
                clear_url: true,
                bump_epoch: true,
                mode_screen_for: Some("descriptor"),
                alert: None,
            }
        );
    }

    #[test]
    fn a_settled_clone_error_still_bumps_the_epoch() {
        // THE #260 regression test: the server may have finished the clone and
        // moved its current selection even though this client's response died
        // (tunnel drop, timeout). Reverting the error-arm bump resurrects the
        // exact reported failure — clone completed, graph never updates.
        let s = clone_settlement::<()>(Err("timed out".into()));
        assert!(s.bump_epoch);
    }

    #[test]
    fn a_settled_clone_error_keeps_the_dialog_and_url_for_a_retry() {
        let s = clone_settlement::<()>(Err("boom".into()));
        assert!(!s.close_dialog);
        assert!(!s.clear_url);
        assert!(s.clear_busy);
        assert_eq!(s.mode_screen_for, None);
        let alert = s.alert.expect("an error must reach the user");
        assert!(alert.contains("boom"));
        // The alert must warn about the completed-anyway case, or the natural
        // reaction to it (retry) silently duplicates the clone (#264).
        assert!(alert.contains("duplicate"));
    }

    #[test]
    fn both_arms_bump_the_epoch_never_only_the_success_one() {
        // Documents that the bump is arm-independent by construction. The
        // pre-#260 code bumped only on success, which is precisely the shape
        // that dropped a completed-but-lost clone on the floor. Asserted as
        // two trues, not equality — equality would also pass with both false.
        assert!(clone_settlement(Ok(())).bump_epoch);
        assert!(clone_settlement::<()>(Err(String::new())).bump_epoch);
    }

    #[test]
    fn a_dialog_with_a_clone_in_flight_refuses_dismissal_regardless_of_the_guard() {
        assert!(!clone_dialog_may_dismiss(true, true));
        assert!(!clone_dialog_may_dismiss(true, false));
    }

    #[test]
    fn an_idle_dialog_defers_to_the_ghost_click_guard() {
        assert!(clone_dialog_may_dismiss(false, true));
        assert!(!clone_dialog_may_dismiss(false, false));
    }

    #[test]
    fn draft_keys_are_distinct_per_repository() {
        // The scoping acceptance criterion of #226, at the key level: two
        // repositories can never share a draft slot.
        let a = commit_draft_key("5e1a4510-aaaa");
        let b = commit_draft_key("f9f44ccb-bbbb");
        assert_ne!(a, b);
        assert!(a.contains("5e1a4510-aaaa"));
        assert!(a.starts_with("gv-commit-draft:"));
    }

    #[test]
    fn reobserving_the_same_repository_keeps_the_live_signal() {
        // The clobber trap: the scope is re-observed on EVERY epoch reload
        // (Refresh, settled writes, drift), almost always with the same repo.
        // Reseeding then would overwrite keystrokes typed since the last
        // persist tick.
        assert_eq!(
            draft_scope_action(Some("same"), Some("same")),
            DraftScopeAction::KeepSignal
        );
        assert_eq!(draft_scope_action(None, None), DraftScopeAction::KeepSignal);
    }

    #[test]
    fn a_changed_or_first_repository_seeds_from_storage() {
        assert_eq!(
            draft_scope_action(None, Some("first")),
            DraftScopeAction::SeedFromStorage
        );
        assert_eq!(
            draft_scope_action(Some("old"), Some("new")),
            DraftScopeAction::SeedFromStorage
        );
        // Losing the repo entirely (degraded frame) also reseeds — to blank,
        // since a None scope persists nothing.
        assert_eq!(
            draft_scope_action(Some("old"), None),
            DraftScopeAction::SeedFromStorage
        );
    }
}
