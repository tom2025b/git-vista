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

/// Whether the Open-URL dialog may be dismissed right now (#260, #278).
///
/// A clone in flight pins the dialog open: dismissing it doesn't cancel the
/// request, it just makes the app *look* idle while a clone is still running —
/// the "acted like it worked" half of #260. `guard_allows` is the existing
/// iOS ghost-click verdict ([`DialogsCore::may_confirm`]); this composes with
/// it rather than replacing it.
///
/// **`checking` re-opens the exit during the #278 polling phase** (review
/// finding). The pin was sized against a worst case of `2×CLONE_TIMEOUT_MS`
/// (~19 minutes); #278's poll budget stacks up to ~120 iterations of
/// interval-plus-two-deadlines on top of that, pushing the worst case past an
/// hour. Pinning someone inside a modal that long is its own version of
/// "the app looks broken" — the very complaint #260 exists to fix.
///
/// Re-opening it *specifically* during polling is safe in a way it is not
/// during the POST: the poll is a read-only `GET /api/clone-status/{key}`
/// loop, so abandoning it cannot leave a half-done mutation, and the
/// authoritative record lives server-side under the retained key either way.
/// The clone's real outcome remains discoverable in the repository picker,
/// which is exactly what the exhausted-poll copy already tells the user to
/// check. What is still pinned is the window where the `POST` itself is in
/// flight and its fate genuinely unknown to everyone.
pub fn clone_dialog_may_dismiss(cloning: bool, checking: bool, guard_allows: bool) -> bool {
    (!cloning || checking) && guard_allows
}

/// Whether a definite (non-2xx) `POST /api/clone` response still leaves the
/// outcome open enough to poll `GET /api/clone-status/{key}` (#278) for a
/// later answer, rather than reporting the response itself as final.
///
/// Every other non-2xx clone response is already **terminal**:
/// `handlers/clone.rs::admit_clone` either ran the clone to completion and
/// recorded its real result, or replayed one already recorded, so polling
/// would only ever reach the exact conclusion the response already carries.
/// The one exception is `409 Conflict` with the "already in progress"
/// message — `admit_clone`'s `Err((CONFLICT, ...))` arm for an attempt that
/// is still *running*, not yet resolved. That response is a snapshot of a
/// clone in flight, not its outcome — and this client's own single in-flight
/// retry (`send_write_with_key`, #216/#218) is exactly what can produce it:
/// the first `POST`'s response was lost, the second raced in while the first
/// was still executing server-side.
///
/// Matched on the message text, not the bare status code: the same `409`
/// also answers a genuinely different, and genuinely terminal, refusal — a
/// key reused for a different clone URL (`admit_clone`'s key-collision
/// check) — which must NOT be polled, since polling that one would just
/// replay the same refusal forever.
pub fn clone_response_should_poll(status: u16, message: &str) -> bool {
    status == 409 && message.contains("already in progress")
}

/// One `GET /api/clone-status/{key}` poll's outcome — the pure decision
/// input for [`clone_poll_step`]. Generic over the descriptor type `T` for
/// the same reason [`CloneSettlement`] is: this module carries no wasm/HTTP
/// dependency, so the caller (`api.rs`, wasm-only) supplies the real
/// `RepositoryDescriptor` while the tests below use a plain string.
///
/// Shaped directly from `handlers/clone.rs::CloneStatusResponse` (checked
/// against that enum, not guessed) plus the two cases that response can
/// never itself represent: the key not existing at all (`404`), and the poll
/// request itself failing before any server answer was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClonePollOutcome<T> {
    /// `{"state": "running"}` — no result yet.
    Running,
    /// `{"state": "succeeded", "descriptor": ...}` — the descriptor a lost
    /// `POST /api/clone` response would have carried.
    Succeeded(T),
    /// `{"state": "failed", "message": ...}` — the same message the original
    /// response would have carried.
    Failed(String),
    /// `404` — the key was never admitted, was evicted past its TTL, or the
    /// original `POST` never reached the server at all. Indistinguishable
    /// from here; see [`clone_poll_step`]'s doc comment for why that's
    /// treated as retryable rather than a hard stop.
    Unknown,
    /// The poll attempt itself failed at the network level — no server
    /// answer was read at all. Distinct from every variant above, all three
    /// of which mean a response *was* read.
    PollError(String),
}

/// What to do after one poll: settle the whole `clone_request` call, or wait
/// and try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClonePollStep<T> {
    Resolved(Result<T, String>),
    KeepPolling,
}

/// The pure decision behind #278's poll loop: given one poll's outcome and
/// how many attempts have been spent against the bounded budget, decide
/// whether to settle the original `clone_request` call or wait and poll
/// again.
///
/// Only [`ClonePollOutcome::Succeeded`]/[`ClonePollOutcome::Failed`] are
/// treated as a real answer — the server said so definitively. Every other
/// variant is retried within budget:
///
/// - `Running` is retried by definition — that's the whole point of polling.
/// - `Unknown` (404) is retried too, not treated as an immediate give-up: a
///   lost `POST /api/clone` response does not guarantee the request ever
///   reached the server's `admit_clone` at all (`with_deadline`'s losing
///   future is dropped client-side, but does not abort the browser's
///   in-flight fetch — see `api.rs::with_deadline`'s doc comment), so the
///   record this key names may simply not exist *yet*, over a tunnel slow
///   enough that the original request is still arriving.
/// - `PollError` is retried too — it is exactly the flaky-tunnel condition
///   this whole feature exists to ride out; one lost poll must not give up
///   on an attempt that is still running server-side.
///
/// `lost_reason` is folded into the final message only once the budget is
/// exhausted, so the user sees why polling started in the first place, not
/// just that it gave up.
pub fn clone_poll_step<T>(
    outcome: ClonePollOutcome<T>,
    attempts_made: u32,
    max_attempts: u32,
    lost_reason: &str,
) -> ClonePollStep<T> {
    match outcome {
        ClonePollOutcome::Succeeded(descriptor) => ClonePollStep::Resolved(Ok(descriptor)),
        ClonePollOutcome::Failed(message) => ClonePollStep::Resolved(Err(message)),
        ClonePollOutcome::Running | ClonePollOutcome::Unknown | ClonePollOutcome::PollError(_) => {
            if attempts_made >= max_attempts {
                ClonePollStep::Resolved(Err(clone_poll_exhausted_message(lost_reason)))
            } else {
                ClonePollStep::KeepPolling
            }
        }
    }
}

/// The message shown when the poll budget runs out without ever reading a
/// definitive `Succeeded`/`Failed` from the server.
fn clone_poll_exhausted_message(lost_reason: &str) -> String {
    // Detail only, deliberately unframed (review finding): every string this
    // returns reaches the user through `clone_settlement`'s `Err` arm, which
    // is the single place that adds the "Couldn't clone:" heading — exactly
    // as it already does for every other error `clone_request` can produce.
    // Framing here too produced the heading twice in one alert.
    format!(
        "{lost_reason}\n\nPolled the server for the outcome but never got a definitive \
         answer. The clone may still be running — check the repository picker in a few \
         minutes before retrying; a retry now can create a duplicate clone."
    )
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
///
/// Losing the scope (`new` is `None`) also keeps the signal. A `None` frame
/// is not only genuine degraded mode — it is what an *errored* seed reload
/// looks like, and over this deployment's flaky tunnel a Refresh during a
/// drop is routine. Blanking the open dialog's textarea on that would lose
/// exactly the work this feature exists to protect. Freezing instead means
/// the scope keeps its last-known repository, per-keystroke persistence
/// continues under that key, and recovery to the same repository is then the
/// same-scope no-op above — nothing clobbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftScopeAction {
    /// Same repository as before, or the repository is momentarily unknown —
    /// leave the live signal (and the last-known scope) alone.
    KeepSignal,
    /// A different repository (or the first one seen) — replace the signal
    /// with that repository's persisted draft, or blank if none.
    SeedFromStorage,
}

pub fn draft_scope_action(old: Option<&str>, new: Option<&str>) -> DraftScopeAction {
    if new.is_none() || old == new {
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
        // Still POSTing: fate genuinely unknown to everyone, stay pinned.
        assert!(!clone_dialog_may_dismiss(true, false, true));
        assert!(!clone_dialog_may_dismiss(true, false, false));
    }

    #[test]
    fn an_idle_dialog_defers_to_the_ghost_click_guard() {
        assert!(clone_dialog_may_dismiss(false, false, true));
        assert!(!clone_dialog_may_dismiss(false, false, false));
    }

    /// #278 review finding: the poll phase re-opens the exit. Polling is a
    /// read-only GET loop whose budget can run to roughly an hour on top of
    /// the POST's own worst case — pinning someone in a modal that long is
    /// the same "app looks broken" complaint #260 exists to fix, and
    /// abandoning a read cannot strand a half-done mutation.
    #[test]
    fn the_polling_phase_can_be_dismissed_but_still_respects_the_ghost_click_guard() {
        assert!(clone_dialog_may_dismiss(true, true, true));
        // The iOS ghost-click guard still has the final say — re-opening the
        // exit must not also re-open the stray-tap hole it was closing.
        assert!(!clone_dialog_may_dismiss(true, true, false));
    }

    #[test]
    fn a_still_in_progress_conflict_is_pollable() {
        assert!(clone_response_should_poll(
            409,
            "A clone for this request is already in progress. Wait for it to finish \
             before retrying."
        ));
    }

    #[test]
    fn a_different_url_conflict_is_not_pollable() {
        // The other 409: a key collision with a genuinely different clone
        // URL. Polling this one would just replay the same refusal forever.
        assert!(!clone_response_should_poll(
            409,
            "That idempotency key was already used for a different clone URL. \
             Retry with a fresh key."
        ));
    }

    #[test]
    fn a_bad_url_or_server_error_is_not_pollable() {
        // Both already terminal, already answered — polling would learn
        // nothing new.
        assert!(!clone_response_should_poll(
            400,
            "fatal: repository not found"
        ));
        assert!(!clone_response_should_poll(500, "Couldn't run git: boom"));
        assert!(!clone_response_should_poll(
            504,
            "The clone did not finish within 10 minutes and was stopped."
        ));
    }

    #[test]
    fn a_succeeded_poll_resolves_ok_with_the_descriptor() {
        let step = clone_poll_step(
            ClonePollOutcome::Succeeded("descriptor"),
            1,
            120,
            "timed out",
        );
        assert_eq!(step, ClonePollStep::Resolved(Ok("descriptor")));
    }

    #[test]
    fn a_failed_poll_resolves_err_with_the_recorded_message() {
        let step = clone_poll_step::<&str>(
            ClonePollOutcome::Failed("fatal: repository not found".into()),
            1,
            120,
            "timed out",
        );
        assert_eq!(
            step,
            ClonePollStep::Resolved(Err("fatal: repository not found".into()))
        );
    }

    #[test]
    fn a_running_poll_under_budget_keeps_polling() {
        let step = clone_poll_step::<&str>(ClonePollOutcome::Running, 1, 120, "timed out");
        assert_eq!(step, ClonePollStep::KeepPolling);
    }

    #[test]
    fn an_unknown_key_under_budget_keeps_polling_rather_than_giving_up_immediately() {
        // The record may simply not exist YET — the original POST could
        // still be arriving over a slow tunnel. See the doc comment on why
        // this is not treated as an instant "no such attempt".
        let step = clone_poll_step::<&str>(ClonePollOutcome::Unknown, 1, 120, "timed out");
        assert_eq!(step, ClonePollStep::KeepPolling);
    }

    #[test]
    fn a_poll_error_under_budget_keeps_polling() {
        // The flaky-tunnel condition this feature exists to ride out — one
        // lost poll must not give up on an attempt that is still running.
        let step = clone_poll_step::<&str>(
            ClonePollOutcome::PollError("network error".into()),
            1,
            120,
            "timed out",
        );
        assert_eq!(step, ClonePollStep::KeepPolling);
    }

    #[test]
    fn a_running_poll_at_the_budget_gives_up_with_the_lost_reason_folded_in() {
        let step = clone_poll_step::<&str>(
            ClonePollOutcome::Running,
            120,
            120,
            "The server did not answer within 60 seconds.",
        );
        match step {
            ClonePollStep::Resolved(Err(message)) => {
                assert!(message.contains("The server did not answer within 60 seconds."));
                assert!(message.contains("may still be running"));
                assert!(message.contains("duplicate"));
            }
            other => panic!("expected a resolved give-up, got {other:?}"),
        }
    }

    #[test]
    fn one_attempt_short_of_budget_still_keeps_polling() {
        // Off-by-one guard: the budget is exhausted only once attempts_made
        // reaches max_attempts, never one short of it.
        let step = clone_poll_step::<&str>(ClonePollOutcome::Running, 119, 120, "timed out");
        assert_eq!(step, ClonePollStep::KeepPolling);
    }

    #[test]
    fn an_unknown_key_at_the_budget_also_gives_up() {
        let step = clone_poll_step::<&str>(ClonePollOutcome::Unknown, 5, 5, "network error");
        assert!(matches!(step, ClonePollStep::Resolved(Err(_))));
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
    fn losing_the_scope_freezes_the_draft_instead_of_blanking_it() {
        // A None frame is what an ERRORED seed reload looks like (Refresh
        // during a tunnel drop), not only genuine degraded mode. Reseeding
        // here would set the open dialog's textarea to blank mid-draft.
        // Keeping the signal — and, by the caller's early return, the
        // last-known scope — means typing during the outage still persists
        // under the last-known repository, and recovery to that repository
        // is the same-scope no-op.
        assert_eq!(
            draft_scope_action(Some("old"), None),
            DraftScopeAction::KeepSignal
        );
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
    }
}
