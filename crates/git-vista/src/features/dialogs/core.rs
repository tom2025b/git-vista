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
    /// A write-failure notice (#316) — `Shell::error_notice`. Dismiss-only:
    /// an error is never "confirmed", so it carries one OK button and the
    /// same backdrop-dismiss ghost-click guard as every other modal here.
    Error,
    /// The pull merge/rebase strategy picker (#232, ADR 0044). Deliberately
    /// its own variant rather than a reuse of [`Dialog::Confirm`]: the
    /// picker's content — `Dialogs::pull_target`/`Dialogs::confirm_strategy`
    /// — cannot be represented as a `PendingOp` until a strategy is chosen
    /// (`MergeStrategy` derives no `Default` and has no "unset" arm,
    /// `crates/git-vista-protocol/src/plan.rs:307-316`), so it cannot share
    /// `Shell::confirm_op`, the signal every other `Dialog::Confirm` opener
    /// writes to. It still shares this guard, same as every other modal.
    PullStrategy,
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
    status == 409 && message.contains(git_vista_protocol::CLONE_IN_PROGRESS_SENTINEL)
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

// ---------------------------------------------------------------------------
// The tiered discard/delete confirmation (M2.18b, #220)
// ---------------------------------------------------------------------------
//
// # Why two ceremonies, and why the difference is structural
//
// `DeleteUntrackedPaths` (`git clean -f`) is the **first operation in this
// app with no way back of any kind**. The content it removes was never
// written to git's object database, so there is no blob to find, no reflog
// entry, no journal event to replay — `planner.rs`'s own response text says
// so in as many words ("That content was never stored in git, so there is no
// way to bring it back"), and a server-side regression test greps that text
// for "undo", "restore" and "recover" to keep it that way.
//
// `DiscardTrackedPaths` (`git checkout --`) is a weaker claim: content that
// was *staged* before it ran is still reachable in the object database until
// the next `git gc`. The backend tags both `RecoveryStrategy::Irrecoverable`
// but spells that difference out in words rather than letting one tag imply
// the same story for both.
//
// So the UI difference is not a label swap. Delete demands **two deliberate
// taps in sequence** — the confirm button is inert until an explicit arm
// control is pressed — while discard is a single tap on a modal that already
// lists every affected path. #220 rules out a type-to-confirm field for the
// reason `dialogs/mod.rs` documents: a void `<input>` panics Leptos' CSR
// node-walk on iOS WebKit, which is why this whole modal is textarea-or-
// nothing.
//
// # The one place this deviates from #220's written bullets
//
// #220 says "`DeleteUntracked`'s confirmation … uses `danger: true` styling;
// `DiscardTracked`'s does not". Both are `danger: true` here. `danger: false`
// renders the confirm button **green** (`dialogs/confirm.rs`'s
// `confirm_style`), which reads as "safe" — and discarding a worktree-only
// edit destroys its only copy just as permanently as the delete does. Saying
// "safe" in colour while saying "gone" in words is the same overclaim the
// backend's wording test exists to prevent, so the asymmetry is carried by
// the ceremony, the title and the body instead of by the button colour.

/// Which of the two working-tree operations a confirmation is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeAction {
    /// `git checkout -- <paths>` — recoverable only for staged content, and
    /// only until the next `git gc`.
    DiscardTracked,
    /// `git clean -f -- <paths>` — permanent, full stop.
    DeleteUntracked,
}

/// The extra, deliberate step the delete ceremony demands before its confirm
/// button does anything. `None` on [`ConfirmPrompt`] means a single-tap
/// confirmation — which is the whole visible difference between the tiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmStep {
    /// The arm control's own label — states which step it is, and flips once
    /// pressed so the two taps are distinguishable without colour.
    pub label: &'static str,
    /// Maps to `aria-pressed`: this is a toggle, and a screen-reader user
    /// needs to hear that step one has landed.
    pub pressed: bool,
}

/// Everything the error modal (#316) renders for one write failure.
///
/// The `body` is the server's `error.message` — already unwrapped from the
/// wire envelope by [`split_error_response`], never the raw JSON — and the
/// `title` names the action that failed ("Couldn't create branch").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorNotice {
    pub title: &'static str,
    pub body: String,
}

/// What a write failure's response body means for the UI: the words to show
/// the user (never raw JSON, never a request id) and the id to log for
/// server-side correlation. Mirrors `api.rs::response_error`'s
/// envelope-or-raw-body fallback, but keeps the two audiences separate
/// (#316) instead of concatenating them into one string the way that
/// helper's 9 existing call sites do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFacingError {
    pub message: String,
    pub request_id: Option<String>,
}

/// Parse a non-2xx `/api/*` body as the `ApiError` envelope every route
/// sends, and split it into what the user should see and what should only
/// reach the console. Falls back to the raw body for anything that isn't
/// the envelope (a route that predates it, or a body reshaped by something
/// in front of the server) — same fallback `response_error` already relies
/// on, so an unparseable body still reaches the user rather than vanishing.
pub fn split_error_response(status: u16, body: &str) -> UserFacingError {
    match serde_json::from_str::<git_vista_protocol::ApiError>(body) {
        Ok(err) => UserFacingError {
            message: err.error.message,
            request_id: Some(err.request_id.as_str().to_string()),
        },
        Err(_) if body.trim().is_empty() => UserFacingError {
            message: format!("HTTP {status}"),
            request_id: None,
        },
        // Valid JSON that is not the envelope — a reverse proxy's own error
        // shape, say. Echoing it would be #316 wearing a different body: JSON
        // a user cannot act on. The status line is the honest fallback.
        Err(_) if serde_json::from_str::<serde_json::Value>(body).is_ok() => UserFacingError {
            message: format!("HTTP {status}"),
            request_id: None,
        },
        Err(_) => UserFacingError {
            message: body.to_string(),
            request_id: None,
        },
    }
}

/// Whether `name` can be sent to `/api/branch` as typed, and the fix to
/// offer when it can't. Git's own ref-name grammar is large (no `~^:?*[`,
/// no `..`, no leading `-`, can't end in `.lock`, ...) and reimplementing
/// all of it here would duplicate git's own validation without ever being
/// as correct as git's — `exec_create_branch`'s own doc comment
/// (planner.rs:2138-2139) is explicit that git is the source of truth and
/// its stderr is forwarded verbatim on any other rejection. This checks
/// only the single most common typo a name-entry prompt actually produces:
/// a space (#316's own repro was literally "test branch"). Every other
/// invalid name still round-trips to the server and comes back through the
/// unwrapped error path above.
pub fn branch_name_space_fix(name: &str) -> Option<String> {
    name.contains(' ').then(|| name.replace(' ', "-"))
}

// ---------------------------------------------------------------------------
// The pull strategy picker (#232, M2.20f, ADR 0044)
// ---------------------------------------------------------------------------

/// The `{remote, branch}` a pull's strategy is being chosen for.
///
/// Resolved live on click, exactly like `rebase_item`'s
/// `fetch_head_branch()` pre-check in `menu.rs` — never taken from the
/// possibly-stale graph. Holding only this pair (never a `MergeStrategy`) is
/// the point: it is the one piece of picker state that exists *before* the
/// user has decided anything, so it is the only piece a pre-decision struct
/// may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullTarget {
    pub remote: String,
    pub branch: String,
}

/// Whether the pull picker's Pull button may run yet.
///
/// Literally `strategy.is_some()` — but named and host-tested so ADR 0044's
/// "no pre-selected option" acceptance criterion is a checked fact rather
/// than a convention a future edit could quietly break. `MergeStrategy`
/// derives no `Default` and carries no sentinel "not yet chosen" variant on
/// purpose (`crates/git-vista-protocol/src/plan.rs:307-316`); this function,
/// not the type, is where "nothing chosen yet" is represented, and only ever
/// as `None` — the same posture the type itself takes at the wire layer
/// (`PullRequest::strategy` has no `#[serde(default)]`,
/// `crates/git-vista-protocol/src/dto.rs:415`).
pub fn pull_confirm_enabled(strategy: Option<git_vista_protocol::plan::MergeStrategy>) -> bool {
    strategy.is_some()
}

/// Everything `dialogs/confirm.rs` renders for one confirmation.
///
/// The first five fields are exactly the `(title, body, confirm_label,
/// danger, enabled)` tuple that view's match has always produced; the last
/// two are M2.18b's addition, and are what let one modal host two different
/// ceremonies without the branch operations noticing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub title: &'static str,
    pub body: String,
    pub confirm_label: &'static str,
    pub danger: bool,
    /// Whether the confirm button may actually run the operation right now.
    pub enabled: bool,
    /// The second deliberate step, for the ceremony that has one.
    pub arm: Option<ArmStep>,
    /// Why the confirm button is inert — `None` exactly when `enabled`.
    ///
    /// Rendered as a **visible line** as well as folded into the button's
    /// `aria-label` (via `graph::core::disabled_menu_item_copy`), because
    /// #65's finding was that a `title`-only reason never surfaces on a tap
    /// and is never announced.
    pub blocked_reason: Option<&'static str>,
}

impl ConfirmPrompt {
    /// A single-tap confirmation with no extra step and nothing blocking it
    /// beyond `enabled` — the shape every branch/undo arm of
    /// `confirm_modal_view` has always had. Its `enabled: false` cases (a
    /// merge into itself, a detached HEAD) carry their reason in the body
    /// text, which is why `blocked_reason` stays `None` for them rather than
    /// duplicating it.
    pub fn plain(
        title: &'static str,
        body: String,
        confirm_label: &'static str,
        danger: bool,
        enabled: bool,
    ) -> Self {
        Self {
            title,
            body,
            confirm_label,
            danger,
            enabled,
            arm: None,
            blocked_reason: None,
        }
    }
}

/// How many paths a confirmation body lists before it summarises the rest.
///
/// A body long enough to scroll past the confirm button is its own hazard on
/// an iPad; twelve fits the modal at the sizes this app is used at. Whatever
/// is cut is still *counted* — see [`path_list`].
pub const PATH_LIST_LIMIT: usize = 12;

/// The 44x44 floor (#65) as an inline-style prefix.
///
/// This modal is inline-styled end to end — `dialogs/mod.rs` records why (the
/// iPad-proven recipe), and that is also why these controls do not appear in
/// `features::a11y::audit`'s stylesheet census, which can only see CSS
/// selectors. Naming the declaration once here is what makes the floor
/// host-checkable at all.
pub const TOUCH_TARGET_STYLE: &str = "min-height:44px; min-width:44px; ";

/// Render `paths` as a bulleted block, capped at `limit`, always stating the
/// full count so a truncated list can never understate what is about to
/// happen.
pub fn path_list(paths: &[String], limit: usize) -> String {
    let shown: Vec<String> = paths.iter().take(limit).map(|p| format!("• {p}")).collect();
    let hidden = paths.len().saturating_sub(shown.len());
    if hidden == 0 {
        shown.join("\n")
    } else {
        format!("{}\n• …and {hidden} more", shown.join("\n"))
    }
}

/// The title, body, button copy and ceremony for one working-tree
/// confirmation.
///
/// `armed` is the live state of [`ArmStep`]'s toggle. It is consulted **only**
/// by [`WorktreeAction::DeleteUntracked`]: a discard is a single-tap
/// confirmation and must not become gated on a control it never shows.
pub fn worktree_confirm(action: WorktreeAction, paths: &[String], armed: bool) -> ConfirmPrompt {
    let count = paths.len();
    let s = if count == 1 { "" } else { "s" };
    let list = path_list(paths, PATH_LIST_LIMIT);
    match action {
        WorktreeAction::DiscardTracked => ConfirmPrompt {
            title: "Discard changes to tracked files",
            body: format!(
                "Discard uncommitted changes to {count} tracked file{s}?\n\n{list}\n\n\
                 Each one goes back to its checked-out version. Content you staged \
                 before this runs is recoverable from git's object database, and only \
                 until the next git gc — a change you never staged has no other copy."
            ),
            confirm_label: "Discard",
            danger: true,
            enabled: count > 0,
            arm: None,
            blocked_reason: (count == 0).then_some("No tracked changes to discard."),
        },
        WorktreeAction::DeleteUntracked => ConfirmPrompt {
            title: "Permanently delete untracked files",
            body: format!(
                "Delete {count} untracked file{s} from the working tree?\n\n{list}\n\n\
                 This content was never stored in git, so once these files are gone \
                 nothing in this repository — and nothing in git-vista — holds a copy \
                 of them. This is permanent."
            ),
            confirm_label: "Delete Permanently",
            danger: true,
            enabled: count > 0 && armed,
            arm: (count > 0).then_some(ArmStep {
                label: if armed {
                    "Step 1 of 2 done — the permanent delete is enabled"
                } else {
                    "Step 1 of 2 — I understand this is permanent"
                },
                pressed: armed,
            }),
            blocked_reason: if count == 0 {
                Some("No untracked files to delete.")
            } else if !armed {
                Some("Complete step 1 first — this delete is permanent.")
            } else {
                None
            },
        },
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

    /// The pull picker (#232) is a new `Dialog` variant, not a special case:
    /// it must consult the same guard as every other modal, with no
    /// exhaustive match anywhere in this core to have missed.
    #[test]
    fn the_pull_picker_shares_the_same_guard_as_every_other_dialog() {
        let mut d = DialogsCore::default();
        d.open(Dialog::PullStrategy, 1_000.0);
        assert_eq!(d.open_dialog(), Some(Dialog::PullStrategy));
        assert!(!d.may_confirm(1_000.0 + GUARD_MS - 1.0));
        assert!(d.may_confirm(1_000.0 + GUARD_MS + 1.0));
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
        // #289: built around the shared sentinel, not a hand-copied string —
        // otherwise this test only proves the matcher agrees with its own
        // private copy of the server's wording, not with the server.
        let message = format!(
            "A clone for this request is {}. Wait for it to finish before retrying.",
            git_vista_protocol::CLONE_IN_PROGRESS_SENTINEL
        );
        assert!(clone_response_should_poll(409, &message));
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

    // -----------------------------------------------------------------
    // The tiered discard/delete confirmation (M2.18b, #220)
    // -----------------------------------------------------------------

    fn paths(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("scratch/file{i}.txt")).collect()
    }

    /// Every string this module puts on screen for one action, so a
    /// vocabulary check cannot pass by only looking at the easy field.
    fn all_copy(c: &ConfirmPrompt) -> String {
        let mut s = format!("{} {} {}", c.title, c.body, c.confirm_label);
        if let Some(a) = &c.arm {
            s.push(' ');
            s.push_str(a.label);
        }
        if let Some(r) = c.blocked_reason {
            s.push(' ');
            s.push_str(r);
        }
        s.to_lowercase()
    }

    const FORBIDDEN: [&str; 3] = ["undo", "restore", "recover"];

    /// The regression this repo's server side already holds itself to
    /// (`delete_untracked_paths_text_never_sounds_recoverable` in
    /// `planner/contract_suite.rs`), applied to the words the *user actually
    /// reads* — the response body it greps is not what the confirmation
    /// dialog shows, so holding the same line here needs its own test.
    ///
    /// The second half is the paired positive that proves the grep can fire:
    /// the discard copy is *allowed* the qualified word, and does contain it.
    /// Without that, an empty or renamed field would let the first half pass
    /// while saying nothing.
    #[test]
    fn the_delete_copy_never_sounds_recoverable_but_the_grep_still_works() {
        for armed in [false, true] {
            let delete = worktree_confirm(WorktreeAction::DeleteUntracked, &paths(2), armed);
            let text = all_copy(&delete);
            for word in FORBIDDEN {
                assert!(
                    !text.contains(word),
                    "delete copy must not sound recoverable (found {word:?}, armed={armed}): {text}"
                );
            }
            assert!(text.contains("permanent"), "{text}");
        }
        // Paired positive: the same grep over the discard copy DOES hit, so
        // the assertions above are capable of failing.
        let discard = worktree_confirm(WorktreeAction::DiscardTracked, &paths(2), false);
        let discard_text = all_copy(&discard);
        assert!(
            discard_text.contains("recover"),
            "the discard copy is meant to state the qualified recovery story: {discard_text}"
        );
        // …and only the *qualified* claim, never a blanket one — the two
        // qualifiers the server's own text is tested for.
        assert!(discard_text.contains("staged"), "{discard_text}");
        assert!(discard_text.contains("git gc"), "{discard_text}");
    }

    /// The tiering is a different *ceremony*, not the same modal with a
    /// different label: delete needs a second deliberate step, discard does
    /// not, and `armed` must not leak across into the discard arm.
    #[test]
    fn delete_needs_two_taps_and_discard_needs_one() {
        let p = paths(3);

        let delete_unarmed = worktree_confirm(WorktreeAction::DeleteUntracked, &p, false);
        assert!(!delete_unarmed.enabled, "an unarmed delete must be inert");
        assert_eq!(
            delete_unarmed.arm,
            Some(ArmStep {
                label: "Step 1 of 2 — I understand this is permanent",
                pressed: false,
            })
        );
        assert!(delete_unarmed.blocked_reason.is_some());

        let delete_armed = worktree_confirm(WorktreeAction::DeleteUntracked, &p, true);
        assert!(delete_armed.enabled, "arming is what makes it live");
        assert!(delete_armed.arm.is_some_and(|a| a.pressed));
        assert_eq!(delete_armed.blocked_reason, None);

        // The discard arm ignores `armed` entirely — a single flag wired to
        // gate both would make one of these two assertions fail.
        for armed in [false, true] {
            let discard = worktree_confirm(WorktreeAction::DiscardTracked, &p, armed);
            assert!(discard.enabled, "a discard is a single-tap confirmation");
            assert_eq!(discard.arm, None);
            assert_eq!(discard.blocked_reason, None);
        }
    }

    /// The anti-cosmetic assertion: if a future edit collapsed these into one
    /// prompt with a swapped verb, this is what would catch it.
    #[test]
    fn the_two_confirmations_differ_in_more_than_their_label() {
        let p = paths(2);
        let discard = worktree_confirm(WorktreeAction::DiscardTracked, &p, true);
        let delete = worktree_confirm(WorktreeAction::DeleteUntracked, &p, true);
        assert_ne!(discard.title, delete.title);
        assert_ne!(discard.body, delete.body);
        assert_ne!(discard.confirm_label, delete.confirm_label);
        assert_ne!(
            discard.arm.is_some(),
            delete.arm.is_some(),
            "the ceremony itself must differ, not only the wording"
        );
    }

    /// Both are `danger: true` — the deviation from #220's literal bullet,
    /// pinned so it is a recorded decision rather than a drift. `danger:
    /// false` paints the confirm button green, and a green button on an
    /// operation that destroys an unstaged edit's only copy says "safe"
    /// when the body says "gone".
    #[test]
    fn both_confirmations_are_styled_destructive() {
        let p = paths(1);
        assert!(worktree_confirm(WorktreeAction::DiscardTracked, &p, false).danger);
        assert!(worktree_confirm(WorktreeAction::DeleteUntracked, &p, false).danger);
    }

    /// #220: no blind "discard all" wording — the body names the files.
    #[test]
    fn the_body_lists_the_exact_paths() {
        let p = vec!["src/a.rs".to_string(), "docs/b.md".to_string()];
        for action in [
            WorktreeAction::DiscardTracked,
            WorktreeAction::DeleteUntracked,
        ] {
            let body = worktree_confirm(action, &p, true).body;
            assert!(body.contains("src/a.rs"), "{body}");
            assert!(body.contains("docs/b.md"), "{body}");
        }
    }

    /// #71 close-out (M2.18): pins `DiscardTracked`'s confirmation copy to
    /// its exact literal wording — not a `.contains` spot-check but the
    /// whole `title`/`body`/`confirm_label` triple — mirroring the
    /// delete-side honesty tests above (`the_delete_copy_never_sounds_
    /// recoverable_but_the_grep_still_works`) with the stricter version:
    /// where those guard *what the words must never say*, this guards *what
    /// the words currently ARE*, so a silent rewording of the discard
    /// ceremony — the one confirmation in this pair with no second arm step
    /// to catch a slip — fails loudly here instead of drifting unnoticed.
    #[test]
    fn discard_tracked_confirmation_copy_is_pinned() {
        let p = vec!["a.txt".to_string(), "sub/b.txt".to_string()];
        let c = worktree_confirm(WorktreeAction::DiscardTracked, &p, false);
        assert_eq!(c.title, "Discard changes to tracked files");
        assert_eq!(
            c.body,
            "Discard uncommitted changes to 2 tracked files?\n\n\
             • a.txt\n\
             • sub/b.txt\n\n\
             Each one goes back to its checked-out version. Content you staged \
             before this runs is recoverable from git's object database, and only \
             until the next git gc — a change you never staged has no other copy."
        );
        assert_eq!(c.confirm_label, "Discard");
        assert!(c.danger);
        assert_eq!(c.arm, None);
    }

    /// A list longer than the cap is summarised, never silently shortened:
    /// the count in the first line still covers every path, and the overflow
    /// is stated. The paired assertion pins that something really was cut —
    /// otherwise "the count is right" would hold trivially.
    #[test]
    fn a_truncated_list_still_states_the_full_count() {
        let p = paths(PATH_LIST_LIMIT + 5);
        let body = worktree_confirm(WorktreeAction::DeleteUntracked, &p, true).body;
        assert!(
            body.contains(&format!("Delete {} untracked files", p.len())),
            "{body}"
        );
        assert!(body.contains("…and 5 more"), "{body}");
        let last = p.last().unwrap();
        assert!(
            !body.contains(last.as_str()),
            "the fixture must actually overflow the cap, or this test proves nothing: {body}"
        );
    }

    #[test]
    fn path_list_shows_everything_when_it_fits() {
        let p = paths(3);
        let listed = path_list(&p, PATH_LIST_LIMIT);
        assert_eq!(listed.lines().count(), 3);
        assert!(!listed.contains("more"), "{listed}");
    }

    /// The empty case is reachable in production, not hypothetical: the menu
    /// builds its path list from a status snapshot, and the worktree can go
    /// clean between that read and the tap.
    #[test]
    fn an_empty_selection_can_never_be_confirmed() {
        for action in [
            WorktreeAction::DiscardTracked,
            WorktreeAction::DeleteUntracked,
        ] {
            for armed in [false, true] {
                let c = worktree_confirm(action, &[], armed);
                assert!(!c.enabled, "{:?} armed={armed}", c.title);
                assert!(c.blocked_reason.is_some(), "{:?}", c.title);
                // Nothing to arm when there is nothing to delete.
                assert_eq!(c.arm, None);
            }
        }
    }

    /// Every reachable `blocked_reason`, pinned to its exact wording and to
    /// the action it belongs to.
    ///
    /// `an_empty_selection_can_never_be_confirmed` above only asks whether a
    /// reason is *present*, which a swap between the two arms' strings
    /// satisfies just as well as the correct assignment does — verified by
    /// mutation: giving `DiscardTracked` the delete arm's "No untracked files
    /// to delete." left all 37 tests in this module green while the user was
    /// being told the wrong thing about the wrong operation. That is exactly
    /// the copy-paste this file's two near-identical arms invite, and this is
    /// the test that refuses it.
    ///
    /// The exact-string pins catch the swap; the vocabulary assertions below
    /// them survive a rewording, so a future edit that legitimately rephrases
    /// both lines still cannot cross them over.
    #[test]
    fn each_blocked_reason_names_its_own_action() {
        // Nothing selected — the same reason whether or not the arm is set,
        // because there is nothing to arm.
        for armed in [false, true] {
            assert_eq!(
                worktree_confirm(WorktreeAction::DiscardTracked, &[], armed).blocked_reason,
                Some("No tracked changes to discard."),
                "armed={armed}"
            );
            assert_eq!(
                worktree_confirm(WorktreeAction::DeleteUntracked, &[], armed).blocked_reason,
                Some("No untracked files to delete."),
                "armed={armed}"
            );
        }
        // Files selected, step 1 not yet taken — the delete's own state, and
        // one the discard arm can never be in.
        assert_eq!(
            worktree_confirm(WorktreeAction::DeleteUntracked, &paths(2), false).blocked_reason,
            Some("Complete step 1 first — this delete is permanent.")
        );

        // Rewording-proof half: each reason speaks its own arm's verb and
        // never the other's. Note the nouns cannot be used for this —
        // "tracked" is a substring of "untracked", so a `contains("tracked")`
        // check would hold for both and prove nothing.
        let discard_empty = worktree_confirm(WorktreeAction::DiscardTracked, &[], false)
            .blocked_reason
            .expect("an empty discard is blocked");
        let delete_empty = worktree_confirm(WorktreeAction::DeleteUntracked, &[], false)
            .blocked_reason
            .expect("an empty delete is blocked");
        assert_ne!(discard_empty, delete_empty);
        assert!(discard_empty.contains("discard"), "{discard_empty}");
        assert!(!discard_empty.contains("delete"), "{discard_empty}");
        assert!(delete_empty.contains("delete"), "{delete_empty}");
        assert!(!delete_empty.contains("discard"), "{delete_empty}");
    }

    /// #65's 44x44 floor, on the one declaration the new controls use. These
    /// buttons are inline-styled, so `features::a11y::audit`'s stylesheet
    /// census cannot see them — this is the check that can.
    #[test]
    fn the_touch_target_style_declares_forty_four_on_both_axes() {
        assert!(TOUCH_TARGET_STYLE.contains("min-height:44px"));
        assert!(TOUCH_TARGET_STYLE.contains("min-width:44px"));
    }

    // `the_touch_target_style_declares_forty_four_on_both_axes` (ends line
    // 1124), before the module's closing `}` (line 1125).

    // -----------------------------------------------------------------
    // #316: unwrap the ApiError envelope; offer a space-fix before the
    // round-trip (branch names)
    // -----------------------------------------------------------------

    /// The exact repro from #316: `{"error":{"code":"bad_request",
    /// "message":"fatal: 'test branch' is not a valid branch name"},
    /// "request_id":"...","protocol":4}` reaching a native `alert()` verbatim.
    /// `split_error_response` must pull only the human message out — never the
    /// surrounding JSON — for a modal to ever show plain words.
    #[test]
    fn a_json_envelope_body_is_split_into_message_and_request_id() {
        let err = git_vista_protocol::ApiError::new(
            git_vista_protocol::ErrorCode::BadRequest,
            "fatal: 'test branch' is not a valid branch name",
            git_vista_protocol::RequestId::new("req-abc123"),
        );
        let body = serde_json::to_string(&err).unwrap();

        let parsed = split_error_response(400, &body);

        assert_eq!(
            parsed.message,
            "fatal: 'test branch' is not a valid branch name"
        );
        assert_eq!(parsed.request_id.as_deref(), Some("req-abc123"));
        // The #316 defect in one line: the message must never be (or contain)
        // the raw envelope this whole function exists to unwrap.
        assert!(
            !parsed.message.contains('{') && !parsed.message.contains("request_id"),
            "the split message must not leak the JSON envelope: {:?}",
            parsed.message
        );
    }

    /// The request id must be extractable, but never folded into the message
    /// string — #316 is explicit that it goes to the console, not the user. A
    /// caller that wants console output uses `request_id` separately; this
    /// pins that the two never get concatenated inside `split_error_response`
    /// itself (which is exactly what `response_error`'s existing `"{} (request
    /// {})"` formatting does, and deliberately why this is a new function
    /// rather than a reuse of that one).
    #[test]
    fn the_request_id_never_leaks_into_the_user_facing_message() {
        let err = git_vista_protocol::ApiError::new(
            git_vista_protocol::ErrorCode::GitFailed,
            "fatal: not a git repository",
            git_vista_protocol::RequestId::new("req-should-not-appear-in-message"),
        );
        let body = serde_json::to_string(&err).unwrap();
        let parsed = split_error_response(500, &body);
        assert_eq!(parsed.message, "fatal: not a git repository");
        assert!(
            !parsed.message.contains("req-should-not-appear-in-message"),
            "{:?}",
            parsed.message
        );
    }

    /// A body that isn't the envelope at all (a route that predates it, or
    /// something in front of the server reshaping the response) still reaches
    /// the user as its raw text — the same fallback `api.rs::response_error`
    /// already relies on for its 9 existing call sites.
    #[test]
    fn valid_json_that_is_not_the_envelope_never_reaches_the_user() {
        // #316 wearing a different body: a reverse proxy's own JSON error
        // shape must fall back to the status line, not be echoed as JSON.
        let got = split_error_response(502, r#"{"detail":"upstream refused"}"#);
        assert_eq!(got.message, "HTTP 502");
        assert_eq!(got.request_id, None);
    }

    #[test]
    fn an_unparseable_body_falls_back_to_the_raw_text() {
        let parsed = split_error_response(502, "Bad Gateway");
        assert_eq!(parsed.message, "Bad Gateway");
        assert_eq!(parsed.request_id, None);
    }

    /// An empty body (e.g. a bare non-2xx status with nothing behind it) falls
    /// back to naming the status, never an empty string a modal would render as
    /// blank.
    #[test]
    fn an_empty_body_falls_back_to_the_http_status() {
        let parsed = split_error_response(400, "");
        assert_eq!(parsed.message, "HTTP 400");
        assert_eq!(parsed.request_id, None);
    }

    /// A name with a space is exactly #316's own repro ("test branch") — offer
    /// the dash-joined fix rather than round-tripping to the server only to get
    /// git's own rejection back.
    #[test]
    fn a_name_with_a_space_offers_the_dash_joined_fix() {
        assert_eq!(
            branch_name_space_fix("test branch"),
            Some("test-branch".to_string())
        );
    }

    /// Every space in the name gets joined, not just the first — a name typed
    /// with multiple words must not come back half-fixed.
    #[test]
    fn every_space_in_the_name_is_joined() {
        assert_eq!(branch_name_space_fix("a b c"), Some("a-b-c".to_string()));
    }

    /// A name with no space is left alone — `None` means "nothing to offer",
    /// not "here is the same string back".
    #[test]
    fn a_name_without_a_space_is_left_alone() {
        assert_eq!(branch_name_space_fix("feature-x"), None);
        assert_eq!(branch_name_space_fix("release/1.0"), None);
    }

    /// Deliberately narrow, per the design's own scope note: this checks only
    /// for a space, not git's full ref-name grammar. A name with some other
    /// invalid character (no space) must NOT get a bogus "fix" offered — that
    /// would silently propose a name whose only problem wasn't actually its
    /// problem, and would round-trip to the server anyway (unfixably, since
    /// this fn only fixes spaces) for no reason.
    #[test]
    fn a_non_space_invalid_character_is_not_treated_as_fixable() {
        assert_eq!(branch_name_space_fix("bad~name"), None);
        assert_eq!(branch_name_space_fix("bad^name"), None);
    }

    // -----------------------------------------------------------------
    // #232 (M2.20f): the pull strategy picker's "no pre-selected default"
    // invariant, ADR 0044
    // -----------------------------------------------------------------

    /// The acceptance criterion itself, at the type boundary: nothing chosen
    /// yet must never read as enabled, or the Pull button would be runnable
    /// before ADR 0044's typed vocabulary can represent what strategy to
    /// send.
    #[test]
    fn the_pull_button_is_disabled_until_a_strategy_is_chosen() {
        assert!(!pull_confirm_enabled(None));
    }

    /// The other half: once *either* strategy is picked, the button enables
    /// — proving `None` is the only disabling value, not some accident of
    /// one particular variant.
    #[test]
    fn either_strategy_enables_the_pull_button_and_neither_is_favoured() {
        use git_vista_protocol::plan::MergeStrategy;
        assert!(pull_confirm_enabled(Some(MergeStrategy::Merge)));
        assert!(pull_confirm_enabled(Some(MergeStrategy::Rebase)));
    }

    // NOT HOST-TESTABLE — wasm-only, needs a wasm-bindgen-test harness (none
    // exists in this repo today, grep-confirmed) or this repo's own human-
    // testbed step (`./dev testbed`, per Git-Vista/CLAUDE.md "Definition of
    // done"):
    //   - menu.rs's `on_branch` closure (lines 274-301): the Err arm (293-298)
    //     wiring `split_error_response`'s output into `shell.open_error(...)`,
    //     and the proposed `web_sys::Window::confirm_with_message` space-fix
    //     pre-flight before `create_branch_request` is called (line 288).
    //     `mod menu;` is `#[cfg(target_arch = "wasm32")]`-gated in main.rs.
    //   - dialogs/commit.rs's `submit_commit` Err arm (line 103) — same gating.
    //   - api.rs's three unwrap sites (`create_branch_request` 929-945,
    //     `create_commit_request` 953-974, `reset_test_repo_request`
    //     1352-1364) — api.rs cannot compile on the host target AT ALL,
    //     because `gloo-net` is a `[target.'cfg(target_arch = "wasm32")'
    //     .dependencies]`-only crate dependency (Cargo.toml line 81).
    //   - the new `Overlay::Error`/`Dialog::Error`/`error_modal_view` render
    //     path — Leptos view code, wasm-only by construction.
}
