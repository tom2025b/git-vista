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
    /// Milliseconds (a `js_sys::Date::now()` reading in the browser, an arbitrary
    /// monotonic-enough scale in tests). `0.0` is the never-opened sentinel the old
    /// `store_value(0.0_f64)` used, and it works the same way here: any realistic `now_ms`
    /// is far beyond the guard window, so a stale core never suppresses a real dismiss.
    opened_at: f64,
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
        self.opened_at = now_ms;
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
    /// cannot shift the boundary by one millisecond.
    pub fn may_confirm(&self, now_ms: f64) -> bool {
        now_ms - self.opened_at > DIALOG_GUARD_MS
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
}
