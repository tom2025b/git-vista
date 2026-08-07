//! The in-flight operations registry — framework-free (M1.11 D1).
//!
//! This is where an operation *lives* between the click that asked for it and the terminal
//! event that resolves it. Before M1.11 it lived nowhere: `dialogs/confirm.rs` cleared the
//! dialog and spawned a fire-and-forget future, so closing a panel mid-write left the user
//! with no record that anything was happening (acceptance criterion 2).
//!
//! The lifecycle types are the protocol's own — [`OperationState`], [`OperationStage`],
//! [`IdempotencyKey`], [`OperationId`] — not parallel client copies, so an SSE
//! [`ProgressEvent`](git_vista_protocol::operation::ProgressEvent) maps straight in.

use git_vista_protocol::dto::{
    FetchError, FetchFailureKind, FetchSuccess, PullError, PullFailureKind, PullSuccess,
};
use git_vista_protocol::operation::{
    IdempotencyKey, OperationId, OperationStage, OperationState, TransferPhase, TransferProgress,
};
use git_vista_protocol::plan::{GenerationToken, MergeStrategy};

use crate::features::core_traits::{Applied, Invalidate, InvalidateScope, RequestKey};
use crate::features::operations::kind::OperationKind;

/// How many settled operations are kept for display.
///
/// Bounded on purpose: the list exists so a failure is visible and dismissible, not as a
/// session-long audit log — the Activity feed is the durable record. An unbounded list
/// would grow for the life of the tab.
pub const MAX_RECENT: usize = 8;

/// An operation the client has started and not yet seen resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// This client's name for the user action (ADR 0020). Stable across retries.
    pub key: IdempotencyKey,
    /// The server's handle, once the write response has been read. `None` between the
    /// dispatch and the response — a real window, and the reason progress for an unbound
    /// id is refused rather than invented.
    pub id: Option<OperationId>,
    pub kind: OperationKind,
    pub state: OperationState,
    pub stage: OperationStage,
    /// The last object-transfer report this operation has produced
    /// (M2.20f, #232) — the client-side mirror of the server's
    /// `OperationStatus::progress` and `ProgressEvent::progress`. `None`
    /// for every operation that transfers nothing, and for a fetch/pull
    /// that has not yet reached its first phase; never synthesized, so a
    /// caller that wants a percentage falls back to naming the phase
    /// rather than inventing one.
    pub progress: Option<TransferProgress>,
    /// Whether a cancel has been asked for and the server accepted the
    /// request (M2.20f, #232). Set by [`OperationsCore::request_cancel`],
    /// never by [`OperationsCore::settle`] — ADR 0043: cancelling only
    /// sets a latch and never terminalises the record itself, so this
    /// stays `true` on an entry that remains, honestly, in flight until
    /// the real terminal event arrives.
    pub cancel_requested: bool,
}

/// What the client needs from a terminal record: whether it worked, what git said, and the
/// generation observed *after* execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub state: OperationState,
    pub message: Option<String>,
    pub generation: Option<GenerationToken>,
}

impl Settlement {
    /// Build a settlement from a record's terminal fields, or `None` if it has not
    /// finished. The guard is the point: `GET /api/operations/{id}` answers with a full
    /// record whether or not the operation is over.
    pub fn from_terminal(
        state: OperationState,
        message: Option<String>,
        generation: Option<GenerationToken>,
    ) -> Option<Self> {
        state.is_terminal().then_some(Self {
            state,
            message,
            generation,
        })
    }
}

/// The follow-up an outcome invites, if any.
///
/// git's safe `branch -d` refuses an unmerged branch with "not fully merged". Dead-ending
/// on that error would be worse than useless — the user asked to delete a branch and the
/// tool knows exactly how — so the confirm modal re-opens offering `-D`. That rule lived
/// inside `dialogs/confirm.rs`'s Delete arm, which made it a dialog decision; it is an
/// operations decision, and stating it here makes it testable (design spec D4).
pub fn escalation(kind: &OperationKind, message: &str) -> Option<OperationKind> {
    match kind {
        OperationKind::Delete { branch, .. } if message.contains("not fully merged") => {
            Some(OperationKind::ForceDelete {
                branch: branch.clone(),
            })
        }
        _ => None,
    }
}

/// A settled [`OperationKind::Fetch`]/[`OperationKind::Pull`]'s `message` as a short human
/// line, instead of the typed JSON it actually is.
///
/// `Settlement::message` is `OperationStatus::message` verbatim (`signals.rs`'s
/// `subscribe()` reads `record.message.clone()` straight into it), and for these two kinds
/// that field is the un-rewrapped [`FetchSuccess`]/[`FetchError`]/[`PullSuccess`]/
/// [`PullError`] body, never git's prose alone. `middleware::api_contract` only rewraps a
/// response whose *whole HTTP status* is 4xx/5xx; `GET /api/operations/{id}` and the SSE
/// `result` event both answer `200` with the record embedded as data, so the typed JSON
/// inside `message` reaches this function exactly as the planner stored it. Left unrendered,
/// the settled card in `view.rs` would show that JSON verbatim at the user — the same
/// failure mode #316 fixed for the commit/reset `alert()` calls, on a channel #316's own
/// `split_error_response` never reaches (that function unwraps the `ApiError` envelope a
/// *direct* non-2xx response carries; a settled record's `message` was never wrapped in
/// one).
///
/// Each kind tries its own success shape first, then its own error shape — the two never
/// collide (`FetchSuccess`/`PullSuccess` have no `kind` field; `FetchError`/`PullError`
/// require one), so which is tried first is not load-bearing. A kind that is neither `Fetch`
/// nor `Pull`, or a `message` that parses as neither shape, comes back unmodified: never
/// worse than the raw string it replaces.
pub fn fetch_or_pull_summary(kind: &OperationKind, message: &str) -> String {
    match kind {
        OperationKind::Fetch { .. } => {
            fetch_summary(message).unwrap_or_else(|| message.to_string())
        }
        OperationKind::Pull { .. } => pull_summary(message).unwrap_or_else(|| message.to_string()),
        _ => message.to_string(),
    }
}

/// `Some` line for a `message` that parses as [`FetchSuccess`] or [`FetchError`]; `None` for
/// anything else, so the caller's fallback stays the one place that decides what "could not
/// parse" shows.
fn fetch_summary(message: &str) -> Option<String> {
    if let Ok(ok) = serde_json::from_str::<FetchSuccess>(message) {
        return Some(ok.message);
    }
    let err = serde_json::from_str::<FetchError>(message).ok()?;
    Some(match err.kind {
        // The other four kinds already read as a complete sentence — the server builds
        // them that way (`planner::fetch::exec_fetch`/`cancelled_response`). Cancellation
        // is the one outcome worth a leading tag, so a glance at the strip finds it
        // without reading the whole line.
        FetchFailureKind::Cancelled => format!("Fetch cancelled — {}", err.message),
        // `CredentialHelperBlocked` joins this group rather than earning a tag:
        // the server's message for it already names the sandbox exclusion and
        // what it means, so a leading tag would only shorten the room the
        // sentence needs. Added here because the protocol gained the variant
        // while this match still listed the original four — the match is
        // exhaustive by design so that a new failure kind cannot reach the UI
        // as a silently-dropped case, and this is that guard doing its job.
        FetchFailureKind::AuthenticationFailed
        | FetchFailureKind::CredentialHelperBlocked
        | FetchFailureKind::RemoteUnreachable
        | FetchFailureKind::RemoteRejected
        | FetchFailureKind::Other => err.message,
    })
}

/// `Some` line for a `message` that parses as [`PullSuccess`] or [`PullError`]; `None` for
/// anything else, mirroring [`fetch_summary`].
fn pull_summary(message: &str) -> Option<String> {
    if let Ok(ok) = serde_json::from_str::<PullSuccess>(message) {
        return Some(ok.message);
    }
    let err = serde_json::from_str::<PullError>(message).ok()?;
    Some(match err.kind {
        // ADR 0044 §5's table is the source for these two tags: `Conflict` and
        // `ConflictLeftInProgress` are the one distinction in this vocabulary that asks
        // opposite things of the user ("choose again — nothing was lost" vs "the working
        // tree needs a human"), so each gets a leading tag rather than living only in the
        // trailing sentence git's own words already carry.
        PullFailureKind::Conflict => {
            format!(
                "Pull hit a conflict — the tree was restored. {}",
                err.message
            )
        }
        PullFailureKind::ConflictLeftInProgress => format!(
            "Pull hit a conflict and needs attention — the working tree is \
             mid-integration. {}",
            err.message
        ),
        // The fetch half's own cancellation tag, for the same scannability reason.
        PullFailureKind::Cancelled => format!("Pull cancelled — {}", err.message),
        PullFailureKind::StrategyRequired
        | PullFailureKind::AuthenticationFailed
        | PullFailureKind::RemoteUnreachable
        | PullFailureKind::RemoteRejected
        | PullFailureKind::NoSuchRemoteBranch
        | PullFailureKind::Other => err.message,
    })
}

/// The word for a [`TransferPhase`], matching the vocabulary `TransferPhase`'s own doc
/// comment (`operation.rs`) documents git as printing, lower-cased to match this strip's
/// existing stage text (`stage_text` in `view.rs`) rather than shouting git's capitalised
/// log lines at the user.
fn phase_word(phase: TransferPhase) -> &'static str {
    match phase {
        TransferPhase::Enumerating => "enumerating objects",
        TransferPhase::Counting => "counting objects",
        TransferPhase::Compressing => "compressing objects",
        TransferPhase::Receiving => "receiving objects",
        TransferPhase::Writing => "writing objects",
        TransferPhase::Resolving => "resolving deltas",
    }
}

/// Turns one [`TransferProgress`] report into the text a user reads (M2.20g, #232) — the
/// concrete answer to the issue's acceptance criterion "show live progress ... rather than a
/// spinner with no detail". Pure on purpose: this crate has no wasm test harness at all, so
/// anything the view needs to show about a transfer's progress has to be derivable, and
/// tested, as a plain function of data rather than asserted against rendered markup.
///
/// Degrades exactly the way [`TransferProgress`]'s own doc comment (`operation.rs`) requires
/// of every reader of the type: never fabricate a number git did not print.
///
/// - `percent` present -> `"<phase> NN%"`. Preferred over the object counts whenever both are
///   present, because it is the more compact signal and the one phase that never carries a
///   percentage (`Enumerating`) also never carries a total, so there is no case where
///   choosing percent throws away a fraction the counts could have shown instead.
/// - `percent` absent, `objects`/`total_objects` both present -> `"<phase> a/b"` (no trailing
///   "objects" word — `phase_word` already names what is being counted, and for
///   [`TransferPhase::Resolving`] that word is "deltas", not "objects").
/// - `percent` absent, only `objects` present -> `"<phase> a"` — `Enumerating`'s own shape,
///   which has a running count and no total to pair it with.
/// - Nothing but the phase -> the phase name alone, never `"0/0"` or any other invented
///   figure.
///
/// Rendered beside, never instead of, `stage_text`'s own line (`view.rs`): the stage names
/// the pipeline step (`Executing`), this names the git-level detail inside it, and a fetch
/// spends its *entire* `Executing` stage moving through this one function's five phases.
pub fn progress_line(progress: &TransferProgress) -> String {
    let phase = phase_word(progress.phase);
    // No trailing "objects" word on the counted forms: `phase_word` already says what is
    // being counted ("enumerating objects", "resolving deltas", ...), so appending it again
    // would either repeat ("enumerating objects 12 objects") or lie ("resolving deltas 12/50
    // objects" when they are deltas, not objects).
    match (progress.percent, progress.objects, progress.total_objects) {
        (Some(pct), _, _) => format!("{phase} {pct}%"),
        (None, Some(done), Some(total)) => format!("{phase} {done}/{total}"),
        (None, Some(done), None) => format!("{phase} {done}"),
        (None, None, _) => phase.to_string(),
    }
}

/// An operation that has resolved, kept briefly so its outcome can be shown and dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub id: OperationId,
    pub kind: OperationKind,
    pub outcome: Settlement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationsRejection {
    /// ADR 0020's client-side mirror of the server's 409: one key names one operation.
    KeyBoundToDifferentOperation,
    /// No in-flight entry matches — never seen, or already resolved and forgotten.
    UnknownOperation,
    /// The terminal event arrived twice (a resumed stream replays it).
    AlreadySettled,
}

/// The registry. Every fallible transition validates before it mutates, so a rejection
/// leaves the core byte-identical (global constraint 4).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OperationsCore {
    in_flight: Vec<InFlight>,
    /// Newest first, capped at [`MAX_RECENT`].
    recent: Vec<Settled>,
}

impl OperationsCore {
    /// Register a user action. Re-admitting the same key with the same operation is the
    /// retry case and changes nothing; re-using it for a *different* operation is refused.
    pub fn admit(
        &mut self,
        key: IdempotencyKey,
        kind: OperationKind,
    ) -> Result<Applied, OperationsRejection> {
        if let Some(existing) = self.in_flight.iter().find(|e| e.key == key) {
            return if existing.kind == kind {
                Ok(Applied::NoChange)
            } else {
                Err(OperationsRejection::KeyBoundToDifferentOperation)
            };
        }
        self.in_flight.push(InFlight {
            key,
            id: None,
            kind,
            state: OperationState::Accepted,
            stage: OperationStage::Queued,
            progress: None,
            cancel_requested: false,
        });
        Ok(Applied::Committed)
    }

    /// Attach the server's handle, read from the write response's `x-git-vista-operation`.
    pub fn bind_id(
        &mut self,
        key: &IdempotencyKey,
        id: OperationId,
    ) -> Result<Applied, OperationsRejection> {
        let entry = self
            .in_flight
            .iter_mut()
            .find(|e| &e.key == key)
            .ok_or(OperationsRejection::UnknownOperation)?;
        if entry.id.as_ref() == Some(&id) {
            return Ok(Applied::NoChange);
        }
        entry.id = Some(id);
        Ok(Applied::Committed)
    }

    /// Record one progress event.
    ///
    /// `progress` is compared too (M2.20f, #232), not just `state`/`stage`:
    /// a fetch sits at `OperationStage::Executing` for its whole transfer,
    /// so state/stage alone would report every percent tick as
    /// [`Applied::NoChange`] and a live progress bar would never move.
    pub fn observe(
        &mut self,
        id: &OperationId,
        state: OperationState,
        stage: OperationStage,
        progress: Option<TransferProgress>,
    ) -> Result<Applied, OperationsRejection> {
        let entry = self
            .in_flight
            .iter_mut()
            .find(|e| e.id.as_ref() == Some(id))
            .ok_or(OperationsRejection::UnknownOperation)?;
        if entry.state == state && entry.stage == stage && entry.progress == progress {
            return Ok(Applied::NoChange);
        }
        entry.state = state;
        entry.stage = stage;
        entry.progress = progress;
        Ok(Applied::Committed)
    }

    /// Record that a cancel request for `id` was accepted by the server
    /// (M2.20f, #232) — sets the client-side flag the status strip renders
    /// as "cancelling…", without touching `state`/`stage`/`progress` and
    /// without removing the entry from the in-flight list.
    ///
    /// **Never terminalises the entry.** ADR 0043 is explicit that
    /// `POST /api/operations/{id}/cancel` "never terminalises the record
    /// itself: only the pipeline may do that, and only after it has
    /// observed what actually happened to the repository" — so this
    /// method's whole job is recording that the *ask* landed, not
    /// resolving anything. The real outcome still arrives, later, through
    /// [`OperationsCore::settle`], exactly as for every other operation.
    ///
    /// Idempotent: a second cancel of the same operation (a retried
    /// request, or a second tap before the first is acknowledged visually)
    /// reports [`Applied::NoChange`] rather than an error.
    pub fn request_cancel(&mut self, id: &OperationId) -> Result<Applied, OperationsRejection> {
        let entry = self
            .in_flight
            .iter_mut()
            .find(|e| e.id.as_ref() == Some(id))
            .ok_or(OperationsRejection::UnknownOperation)?;
        if entry.cancel_requested {
            return Ok(Applied::NoChange);
        }
        entry.cancel_requested = true;
        Ok(Applied::Committed)
    }

    /// Resolve an operation and publish what the rest of the app must reconcile against.
    ///
    /// The returned [`Invalidate`] carries the post-execution generation, so a feature
    /// holding server state can compare it with what it already has and re-read only when
    /// the repository actually moved (design spec D3).
    pub fn settle(
        &mut self,
        id: &OperationId,
        outcome: Settlement,
    ) -> Result<Invalidate, OperationsRejection> {
        let Some(index) = self
            .in_flight
            .iter()
            .position(|e| e.id.as_ref() == Some(id))
        else {
            // Distinguish "never heard of it" from "the stream replayed the terminal
            // event": only the second is expected, and it must be a no-op rather than a
            // second invalidation.
            return Err(if self.recent.iter().any(|s| &s.id == id) {
                OperationsRejection::AlreadySettled
            } else {
                OperationsRejection::UnknownOperation
            });
        };
        let entry = self.in_flight.remove(index);
        let invalidate = Invalidate {
            generation: outcome.generation.clone(),
            // A write can move refs, the working tree and the journal at once, so nothing
            // narrower than `Everything` is honest here. The generation is what stops that
            // from meaning "re-read blindly".
            scope: InvalidateScope::Everything,
        };
        self.recent.insert(
            0,
            Settled {
                id: id.clone(),
                kind: entry.kind,
                outcome,
            },
        );
        self.recent.truncate(MAX_RECENT);
        Ok(invalidate)
    }

    /// Drop a settled entry the user has acknowledged.
    pub fn dismiss(&mut self, id: &OperationId) -> Applied {
        match self.recent.iter().position(|s| &s.id == id) {
            Some(index) => {
                self.recent.remove(index);
                Applied::Committed
            }
            None => Applied::NoChange,
        }
    }

    pub fn in_flight(&self) -> impl Iterator<Item = &InFlight> {
        self.in_flight.iter()
    }

    /// Settled operations, newest first.
    pub fn recent(&self) -> impl Iterator<Item = &Settled> {
        self.recent.iter()
    }
}

/// The subset of an in-flight Fetch/Pull's identity persisted across a
/// reload or Safari tab suspend/resume (#232, M2.20f) — the client-side
/// half of the "reconnect, don't lose track" acceptance criterion.
/// Deliberately its own struct rather than `#[derive(Serialize,
/// Deserialize)]` on all of [`OperationKind`]: most variants
/// (`Undo(Undoable)` among them) have no business gaining a wire shape
/// just because two of their siblings now need one to survive
/// `localStorage`.
///
/// `branch`/`strategy` are `None` together for a Fetch and `Some` together
/// for a Pull — never mixed, because `OperationKind::Pull` cannot be
/// constructed before the strategy picker supplies both at once (ADR
/// 0044). [`remote_op_kind`] treats any other pairing as corrupt storage,
/// not a third operation kind.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InFlightRemoteOp {
    pub id: String,
    pub remote: String,
    pub branch: Option<String>,
    pub strategy: Option<MergeStrategy>,
}

/// Rebuild the [`OperationKind`] a persisted [`InFlightRemoteOp`] names, or
/// `None` for a shape [`InFlightRemoteOp`] itself cannot enforce but this
/// app would never have written: a `branch` with no `strategy`, or a
/// `strategy` with no `branch`. Pull always carries both together (ADR
/// 0044), so either mismatch can only be storage that was hand-edited or
/// left behind by a different client version — never a third operation
/// kind, and never trusted.
pub fn remote_op_kind(entry: &InFlightRemoteOp) -> Option<OperationKind> {
    match (&entry.branch, entry.strategy) {
        (None, None) => Some(OperationKind::Fetch {
            remote: entry.remote.clone(),
        }),
        (Some(branch), Some(strategy)) => Some(OperationKind::Pull {
            remote: entry.remote.clone(),
            branch: branch.clone(),
            strategy,
        }),
        _ => None,
    }
}

/// What a boot-time reconnect should do with a resumed operation's live
/// status (#232) — the pure half of `resume_inflight_remote_op`'s decision
/// (`features::operations::signals`), so "terminal means settle, not-yet-
/// terminal means keep watching" is a tested fact rather than an inline
/// branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDecision {
    /// The operation already reached a terminal state while the tab was
    /// away or reloading — replay the settlement once, immediately,
    /// rather than opening a stream that would answer with the same
    /// terminal `result` event anyway.
    Settle,
    /// Still running — resubscribe to its SSE stream and keep watching
    /// live, the same as a freshly dispatched operation.
    Subscribe,
}

/// Whether a resumed operation's status should be replayed as an
/// immediate settlement or watched live. A pure wrapper over
/// [`OperationState::is_terminal`], named and tested on its own so the
/// reconnect path's central branch is a checked fact — #232's own
/// acceptance criterion: reconnect to "current or terminal state", never
/// lose track.
pub fn resume_decision(state: OperationState) -> ResumeDecision {
    if state.is_terminal() {
        ResumeDecision::Settle
    } else {
        ResumeDecision::Subscribe
    }
}

/// Mints the monotone sequence that orders user intents by *click* time.
///
/// The graph epoch cannot do this job. Two menu taps land in the same epoch — nothing
/// invalidates the graph between them — so ordering by epoch would leave every tie to the
/// incoming write, which is precisely the network-order bug being fixed. A counter that
/// advances once per user action is the only thing that records what the user did last.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IntentSeq(u64);

impl IntentSeq {
    /// Mint the next sequence. Called synchronously at click time, before any `await`.
    pub fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Whether a result stamped `seq` may overwrite the one currently shown, stamped
/// `shown_seq`.
///
/// The same ordering rule as [`latest_wins`], for the places that display a *result* rather
/// than raise an operation — the repo picker's one status line, written by both the
/// delete-clone handler and the Rescan button (`picker.rs`). Without a stamp the line shows
/// whichever request answered last, which after a quick Delete-then-Rescan is the wrong one.
pub fn result_is_newest(shown_seq: u64, seq: u64) -> bool {
    seq >= shown_seq
}

/// A user intent that has been raised but whose pre-check has not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingIntent {
    /// Click order, from [`IntentSeq::next`]. Minted before the `spawn_local`, so it
    /// records when the user *acted*, not when the network *answered*.
    pub seq: u64,
    /// Which repository state the intent was raised against, so a repository switch or a
    /// generation bump can strand it even when it is the newest intent.
    pub key: RequestKey,
    pub kind: OperationKind,
}

/// Whether `incoming` may replace `current`.
///
/// Fixes the `menu.rs` race (design spec §3): every branch item does a live
/// `fetch_head_branch()` pre-check and today writes `confirm_op` unconditionally in its
/// continuation (`menu.rs:352-363,378-389,422-433,540-548`), so dialogs open in *network*
/// order rather than *click* order — tap Checkout then Merge, and a slow Checkout pre-check
/// reopens the Checkout dialog over the Merge one the user is looking at.
///
/// This is only half the gate. A caller must also check
/// [`RequestKey::is_current`](crate::features::core_traits::RequestKey::is_current), which
/// strands an intent whose repository moved underneath it. Sequence answers "did the user
/// ask for something newer?"; the key answers "is what they asked for still meaningful?".
///
/// Ties go to `incoming`: sequences are unique in practice, and admitting the later of two
/// equal values keeps the function total without a special case.
pub fn latest_wins(current: Option<&PendingIntent>, incoming: &PendingIntent) -> bool {
    match current {
        None => true,
        Some(cur) => incoming.seq >= cur.seq,
    }
}

#[cfg(test)]
mod core_tests {
    use super::*;
    use git_vista_protocol::operation::OperationStage;

    fn key(s: &str) -> IdempotencyKey {
        IdempotencyKey::new(s).expect("valid idempotency key")
    }

    fn id(s: &str) -> OperationId {
        OperationId::new(s).expect("valid operation id")
    }

    fn merge() -> OperationKind {
        OperationKind::Merge {
            branch: "feature".into(),
            into: Some("main".into()),
        }
    }

    fn succeeded(generation: &str) -> Settlement {
        Settlement {
            state: OperationState::Succeeded,
            message: None,
            generation: Some(GenerationToken::new(generation).expect("valid generation")),
        }
    }

    /// An admitted operation whose server id is already bound — the state every test
    /// that cares about progress or settlement starts from.
    fn running() -> OperationsCore {
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).expect("first admit accepted");
        c.bind_id(&key("k1"), id("op-1")).expect("bind accepted");
        c
    }

    #[test]
    fn an_admitted_operation_is_in_flight() {
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).expect("first admit accepted");
        assert_eq!(c.in_flight().count(), 1);
    }

    #[test]
    fn readmitting_the_same_key_is_a_noop_not_a_second_operation() {
        // ADR 0020: a key is minted per USER ACTION and reused across network retries. A
        // retry must never become a second operation.
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).unwrap();
        let applied = c.admit(key("k1"), merge()).expect("retry accepted");
        assert_eq!(applied, Applied::NoChange);
        assert_eq!(c.in_flight().count(), 1, "a retry is the same operation");
    }

    #[test]
    fn reusing_a_key_with_a_different_operation_is_refused() {
        // Mirrors the server's own 409 (ADR 0020): a key alone is a footgun.
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).unwrap();
        let err = c
            .admit(
                key("k1"),
                OperationKind::Delete {
                    branch: "other".into(),
                    current: None,
                },
            )
            .unwrap_err();
        assert_eq!(err, OperationsRejection::KeyBoundToDifferentOperation);
        assert_eq!(
            c.in_flight().count(),
            1,
            "the refused admit changed nothing"
        );
    }

    #[test]
    fn settling_yields_the_post_execution_generation_as_an_invalidation() {
        // The criterion-4 datum: reconcile against the generation the server observed
        // AFTER execution, instead of blindly re-reading everything.
        let mut c = running();
        let inv = c
            .settle(&id("op-1"), succeeded("77"))
            .expect("settle accepted");
        assert_eq!(inv.generation.as_ref().map(|g| g.as_str()), Some("77"));
        assert_eq!(inv.scope, InvalidateScope::Everything);
        assert_eq!(
            c.in_flight().count(),
            0,
            "a settled operation is no longer in flight"
        );
    }

    #[test]
    fn settling_an_unknown_id_is_refused_and_changes_nothing() {
        let mut c = running();
        let err = c.settle(&id("nope"), succeeded("77")).unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
        assert_eq!(c.in_flight().count(), 1);
    }

    #[test]
    fn settling_twice_is_refused_so_a_reconnected_stream_cannot_double_apply() {
        // A resumed SSE stream replays the terminal event. Applying it twice must not
        // publish a second invalidation — which would bump the graph epoch again and
        // re-read the whole repository for nothing.
        let mut c = running();
        c.settle(&id("op-1"), succeeded("77")).unwrap();
        let err = c.settle(&id("op-1"), succeeded("77")).unwrap_err();
        assert_eq!(err, OperationsRejection::AlreadySettled);
    }

    #[test]
    fn an_operation_survives_observation_of_every_stage() {
        // Criterion 2 in core form: nothing about a panel appears here, so nothing a panel
        // does can drop this state.
        let mut c = running();
        for stage in [
            OperationStage::Queued,
            OperationStage::Planning,
            OperationStage::Waiting,
            OperationStage::Checking,
            OperationStage::Executing,
        ] {
            c.observe(&id("op-1"), OperationState::Running, stage, None)
                .expect("stage accepted");
        }
        assert_eq!(c.in_flight().count(), 1);
        let live = c.in_flight().next().unwrap();
        assert_eq!(live.stage, OperationStage::Executing);
        assert_eq!(live.state, OperationState::Running);
    }

    #[test]
    fn observing_the_same_stage_twice_reports_no_change() {
        // The stream heartbeats and can repeat; a repeat is not a transition.
        let mut c = running();
        c.observe(
            &id("op-1"),
            OperationState::Running,
            OperationStage::Planning,
            None,
        )
        .unwrap();
        let applied = c
            .observe(
                &id("op-1"),
                OperationState::Running,
                OperationStage::Planning,
                None,
            )
            .unwrap();
        assert_eq!(applied, Applied::NoChange);
    }

    #[test]
    fn a_failed_operation_stays_visible_after_it_settles() {
        // Task 5 replaces the native `window.alert()` failure path with reactive state.
        // That only works if a failure survives settlement instead of vanishing.
        let mut c = running();
        c.settle(
            &id("op-1"),
            Settlement {
                state: OperationState::Failed,
                message: Some("not fully merged".into()),
                generation: None,
            },
        )
        .expect("a failure is an outcome, so it settles");
        assert_eq!(c.in_flight().count(), 0);
        let settled: Vec<_> = c.recent().collect();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].outcome.state, OperationState::Failed);
        assert_eq!(
            settled[0].outcome.message.as_deref(),
            Some("not fully merged")
        );
    }

    #[test]
    fn a_settled_entry_can_be_dismissed() {
        let mut c = running();
        c.settle(&id("op-1"), succeeded("77")).unwrap();
        assert_eq!(c.dismiss(&id("op-1")), Applied::Committed);
        assert_eq!(c.recent().count(), 0);
        assert_eq!(
            c.dismiss(&id("op-1")),
            Applied::NoChange,
            "dismissing twice is harmless"
        );
    }

    #[test]
    fn the_settled_list_is_bounded_so_a_long_session_cannot_grow_without_limit() {
        let mut c = OperationsCore::default();
        for n in 0..(MAX_RECENT + 3) {
            let k = key(&format!("k{n}"));
            let i = id(&format!("op-{n}"));
            c.admit(k.clone(), merge()).unwrap();
            c.bind_id(&k, i.clone()).unwrap();
            c.settle(&i, succeeded("77")).unwrap();
        }
        assert_eq!(c.recent().count(), MAX_RECENT);
        assert_eq!(
            c.recent().next().unwrap().id.as_str(),
            format!("op-{}", MAX_RECENT + 2),
            "the newest settlement is first; the oldest were dropped"
        );
    }

    #[test]
    fn an_unmerged_delete_escalates_to_a_force_delete_of_the_same_branch() {
        let refused = OperationKind::Delete {
            branch: "feature".into(),
            current: Some("main".into()),
        };
        assert_eq!(
            escalation(&refused, "error: the branch 'feature' is not fully merged"),
            Some(OperationKind::ForceDelete {
                branch: "feature".into()
            })
        );
    }

    #[test]
    fn nothing_else_escalates() {
        // A delete that failed for another reason is a dead end on purpose: offering `-D`
        // for, say, a locked ref would suggest force is the answer when it is not.
        let delete = OperationKind::Delete {
            branch: "feature".into(),
            current: None,
        };
        assert_eq!(escalation(&delete, "permission denied"), None);
        assert_eq!(
            escalation(&merge(), "error: the branch is not fully merged"),
            None,
            "only a delete escalates, whatever the message says"
        );
    }

    #[test]
    fn binding_an_id_to_an_unknown_key_is_refused() {
        let mut c = OperationsCore::default();
        let err = c.bind_id(&key("k1"), id("op-1")).unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
    }

    #[test]
    fn progress_for_an_operation_whose_id_is_not_yet_bound_is_refused() {
        // The dispatch writes, the response binds the id, and only then can the stream
        // say anything. An event arriving before the bind names an operation this client
        // cannot yet match, and must not invent one.
        let mut c = OperationsCore::default();
        c.admit(key("k1"), merge()).unwrap();
        let err = c
            .observe(
                &id("op-1"),
                OperationState::Running,
                OperationStage::Planning,
                None,
            )
            .unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
    }

    #[test]
    fn a_settlement_is_built_only_from_a_terminal_record() {
        // `GET /api/operations/{id}` answers with a full record whether or not it has
        // finished. Reconciling from a non-terminal one would record an outcome that has
        // not happened.
        assert!(Settlement::from_terminal(OperationState::Running, None, None).is_none());
        let s = Settlement::from_terminal(
            OperationState::Succeeded,
            Some("Fast-forward".into()),
            GenerationToken::new("9").ok(),
        )
        .expect("a terminal record settles");
        assert_eq!(s.state, OperationState::Succeeded);
        assert_eq!(s.generation.as_ref().map(|g| g.as_str()), Some("9"));
    }

    // -----------------------------------------------------------------
    // #232 (M2.20f): the boot-time reconnect for an in-flight Fetch/Pull
    // -----------------------------------------------------------------

    #[test]
    fn a_terminal_state_resumes_by_settling() {
        assert_eq!(
            resume_decision(OperationState::Succeeded),
            ResumeDecision::Settle
        );
        assert_eq!(
            resume_decision(OperationState::Failed),
            ResumeDecision::Settle
        );
    }

    #[test]
    fn a_non_terminal_state_resumes_by_subscribing() {
        assert_eq!(
            resume_decision(OperationState::Accepted),
            ResumeDecision::Subscribe
        );
        assert_eq!(
            resume_decision(OperationState::Running),
            ResumeDecision::Subscribe
        );
    }

    #[test]
    fn an_inflight_remote_op_round_trips_through_json() {
        let fetch = InFlightRemoteOp {
            id: "op-1".to_string(),
            remote: "origin".to_string(),
            branch: None,
            strategy: None,
        };
        let json = serde_json::to_string(&fetch).expect("serializes");
        let back: InFlightRemoteOp = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(fetch, back);

        let pull = InFlightRemoteOp {
            id: "op-2".to_string(),
            remote: "origin".to_string(),
            branch: Some("main".to_string()),
            strategy: Some(MergeStrategy::Rebase),
        };
        let json = serde_json::to_string(&pull).expect("serializes");
        let back: InFlightRemoteOp = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(pull, back);
    }

    #[test]
    fn malformed_inflight_remote_op_json_refuses_to_deserialize() {
        // A mutation check on the round trip above: dropping a required field
        // must not silently produce a default-ish value nobody asked for.
        assert!(serde_json::from_str::<InFlightRemoteOp>("{\"id\":\"op-1\"}").is_err());
    }

    #[test]
    fn remote_op_kind_rebuilds_fetch_from_a_persisted_entry_with_no_branch() {
        let entry = InFlightRemoteOp {
            id: "op-1".into(),
            remote: "origin".into(),
            branch: None,
            strategy: None,
        };
        assert_eq!(
            remote_op_kind(&entry),
            Some(OperationKind::Fetch {
                remote: "origin".into()
            })
        );
    }

    #[test]
    fn remote_op_kind_rebuilds_pull_from_a_persisted_entry_with_branch_and_strategy() {
        let entry = InFlightRemoteOp {
            id: "op-2".into(),
            remote: "origin".into(),
            branch: Some("main".into()),
            strategy: Some(MergeStrategy::Rebase),
        };
        assert_eq!(
            remote_op_kind(&entry),
            Some(OperationKind::Pull {
                remote: "origin".into(),
                branch: "main".into(),
                strategy: MergeStrategy::Rebase,
            })
        );
    }

    #[test]
    fn remote_op_kind_refuses_a_branch_with_no_strategy_as_corrupt() {
        // Pull always carries both together (ADR 0044) — a branch alone can
        // only be storage that was hand-edited or written by a different
        // client version, never a shape this app itself would produce.
        let entry = InFlightRemoteOp {
            id: "op-3".into(),
            remote: "origin".into(),
            branch: Some("main".into()),
            strategy: None,
        };
        assert_eq!(remote_op_kind(&entry), None);
    }

    #[test]
    fn remote_op_kind_refuses_a_strategy_with_no_branch_as_corrupt() {
        let entry = InFlightRemoteOp {
            id: "op-4".into(),
            remote: "origin".into(),
            branch: None,
            strategy: Some(MergeStrategy::Merge),
        };
        assert_eq!(remote_op_kind(&entry), None);
    }
}

#[cfg(test)]
mod intent_tests {
    use super::*;
    use crate::features::core_traits::{RequestKey, RequestTarget};

    /// An intent raised by the `seq`-th click of the session, against graph epoch 1.
    fn intent(seq: u64, branch: &str) -> PendingIntent {
        PendingIntent {
            seq,
            key: RequestKey {
                epoch: 1,
                generation: None,
                target: RequestTarget::Branch(branch.to_string()),
            },
            kind: OperationKind::Delete {
                branch: branch.to_string(),
                current: None,
            },
        }
    }

    #[test]
    fn a_slower_earlier_response_cannot_replace_a_newer_pending_intent() {
        // The menu.rs race (design spec §3): the user taps Merge, then Delete. Delete's
        // pre-check resolves first and opens the dialog. Merge's pre-check then resolves
        // and must NOT overwrite the dialog the user is looking at.
        let delete = intent(5, "other-branch");
        let stale_merge = intent(4, "feature");
        assert!(
            !latest_wins(Some(&delete), &stale_merge),
            "an intent from an earlier click must be dropped, not committed"
        );
    }

    #[test]
    fn a_newer_intent_replaces_an_older_pending_one() {
        let old = intent(4, "feature");
        let new = intent(5, "other-branch");
        assert!(latest_wins(Some(&old), &new));
    }

    #[test]
    fn the_first_intent_always_wins_when_nothing_is_pending() {
        assert!(latest_wins(None, &intent(1, "main")));
    }

    #[test]
    fn two_intents_with_the_same_sequence_resolve_to_the_incoming_one() {
        // Sequences are unique in practice; admitting the later of two equal values keeps
        // the function total rather than leaving an unreachable special case.
        let a = intent(5, "a");
        let b = intent(5, "b");
        assert!(latest_wins(Some(&a), &b));
    }

    #[test]
    fn intents_racing_within_one_epoch_are_still_ordered() {
        // The defect this whole task exists to fix. Both taps happen against the SAME graph
        // epoch — nothing invalidated the graph between them — so epoch comparison alone
        // would call them equal and let whichever response arrived last win. Only the click
        // sequence records what the user actually asked for most recently.
        let mut seq = IntentSeq::default();
        let first = PendingIntent {
            seq: seq.next(),
            ..intent(0, "checkout-target")
        };
        let second = PendingIntent {
            seq: seq.next(),
            ..intent(0, "merge-source")
        };
        assert_eq!(
            first.key.epoch, second.key.epoch,
            "the premise: one epoch spans both clicks"
        );
        assert!(latest_wins(Some(&first), &second), "the later tap commits");
        assert!(
            !latest_wins(Some(&second), &first),
            "and the earlier tap's straggling response is dropped"
        );
    }

    #[test]
    fn a_stale_result_cannot_overwrite_the_message_a_newer_action_already_showed() {
        // The picker bug: tap Delete, then Rescan. Rescan answers first and writes its
        // line; Delete's slower reply then replaces it, so the user reads the outcome of
        // the action they did NOT do most recently.
        let mut seq = IntentSeq::default();
        let delete = seq.next();
        let rescan = seq.next();
        assert!(
            result_is_newest(0, rescan),
            "nothing shown yet, so it shows"
        );
        assert!(
            !result_is_newest(rescan, delete),
            "the earlier action's reply must not overwrite the later action's line"
        );
    }

    #[test]
    fn intent_sequences_are_monotone_and_start_above_zero() {
        // Zero is reserved for "no intent has ever been raised", so the first mint must not
        // collide with the initial value of the counter a caller stores.
        let mut seq = IntentSeq::default();
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
        assert_eq!(seq.next(), 3);
    }
}

#[cfg(test)]
mod fetch_pull_summary_tests {
    use super::*;

    fn fetch_kind() -> OperationKind {
        OperationKind::Fetch {
            remote: "origin".into(),
        }
    }

    fn pull_kind() -> OperationKind {
        OperationKind::Pull {
            remote: "origin".into(),
            branch: "main".into(),
            strategy: git_vista_protocol::plan::MergeStrategy::Merge,
        }
    }

    fn fetch_error_body(kind: FetchFailureKind, message: &str) -> String {
        // Built from the server's own DTO and serialized the way the server serializes
        // it — not a hand-written JSON string that could agree with a client-side
        // assumption while disagreeing with the wire.
        serde_json::to_string(&FetchError {
            kind,
            message: message.to_string(),
            updated_refs: Vec::new(),
        })
        .unwrap()
    }

    fn pull_error_body(kind: PullFailureKind, message: &str, worktree_restored: bool) -> String {
        serde_json::to_string(&PullError {
            kind,
            message: message.to_string(),
            updated_refs: Vec::new(),
            worktree_restored,
        })
        .unwrap()
    }

    #[test]
    fn a_kind_that_is_neither_fetch_nor_pull_passes_the_raw_message_through() {
        let merge = OperationKind::Merge {
            branch: "feature".into(),
            into: Some("main".into()),
        };
        let raw = "{\"anything\":true}";
        assert_eq!(fetch_or_pull_summary(&merge, raw), raw);
    }

    #[test]
    fn a_fetch_success_body_reads_back_gits_own_words() {
        let success = FetchSuccess {
            remote: "origin".into(),
            message: "Fetched from \u{2018}origin\u{2019}: 3 remote-tracking refs updated.".into(),
            updated_refs: Vec::new(),
        };
        let json = serde_json::to_string(&success).unwrap();
        assert_eq!(fetch_or_pull_summary(&fetch_kind(), &json), success.message);
    }

    #[test]
    fn a_pull_success_body_reads_back_gits_own_words() {
        let success = PullSuccess {
            remote: "origin".into(),
            branch: "main".into(),
            strategy: git_vista_protocol::plan::MergeStrategy::Rebase,
            message: "Pulled \u{2018}main\u{2019} from \u{2018}origin\u{2019} into the \
                      checked-out branch (rebase strategy)."
                .into(),
            updated_refs: Vec::new(),
            advanced: true,
        };
        let json = serde_json::to_string(&success).unwrap();
        assert_eq!(fetch_or_pull_summary(&pull_kind(), &json), success.message);
    }

    #[test]
    fn every_passthrough_fetch_failure_kind_renders_gits_words_unmodified() {
        // Named-mutation test: strict equality, not `.contains`. A bug that moved one of
        // these arms into the `Cancelled` match arm would still produce text containing
        // "git said this" — only equality catches the added tag.
        for kind in [
            FetchFailureKind::AuthenticationFailed,
            FetchFailureKind::RemoteUnreachable,
            FetchFailureKind::RemoteRejected,
            FetchFailureKind::Other,
        ] {
            let body = fetch_error_body(kind, "git said this");
            let line = fetch_or_pull_summary(&fetch_kind(), &body);
            assert_eq!(
                line, "git said this",
                "{kind:?} must pass git's words through as-is"
            );
        }
    }

    #[test]
    fn a_cancelled_fetch_is_tagged_so_it_is_scannable() {
        // Named mutation: swap `Cancelled` for any passthrough kind above and this must
        // fail — the leading tag is the one thing this test exists to prove.
        let body = fetch_error_body(FetchFailureKind::Cancelled, "the fetch was cancelled");
        let line = fetch_or_pull_summary(&fetch_kind(), &body);
        assert_eq!(line, "Fetch cancelled — the fetch was cancelled");
    }

    #[test]
    fn every_passthrough_pull_failure_kind_renders_gits_words_unmodified() {
        for kind in [
            PullFailureKind::StrategyRequired,
            PullFailureKind::AuthenticationFailed,
            PullFailureKind::RemoteUnreachable,
            PullFailureKind::RemoteRejected,
            PullFailureKind::NoSuchRemoteBranch,
            PullFailureKind::Other,
        ] {
            let body = pull_error_body(kind, "git said this", true);
            let line = fetch_or_pull_summary(&pull_kind(), &body);
            assert_eq!(
                line, "git said this",
                "{kind:?} must pass git's words through as-is"
            );
        }
    }

    #[test]
    fn a_conflict_says_the_tree_was_restored_and_nothing_was_lost() {
        // Named mutation: swap `Conflict` for `ConflictLeftInProgress` (or vice versa) and
        // this must fail — ADR 0044 §5 states the two ask opposite things of the user, so
        // they must never share a rendering.
        let body = pull_error_body(PullFailureKind::Conflict, "git said this", true);
        let line = fetch_or_pull_summary(&pull_kind(), &body);
        assert_eq!(
            line,
            "Pull hit a conflict — the tree was restored. git said this"
        );
    }

    #[test]
    fn a_conflict_left_in_progress_says_the_tree_needs_attention() {
        let body = pull_error_body(
            PullFailureKind::ConflictLeftInProgress,
            "git said this",
            false,
        );
        let line = fetch_or_pull_summary(&pull_kind(), &body);
        assert_eq!(
            line,
            "Pull hit a conflict and needs attention — the working tree is \
             mid-integration. git said this"
        );
        assert_ne!(
            line,
            fetch_or_pull_summary(
                &pull_kind(),
                &pull_error_body(PullFailureKind::Conflict, "git said this", true)
            ),
            "conflict and conflict-left-in-progress must never render the same line"
        );
    }

    #[test]
    fn a_cancelled_pull_is_tagged_so_it_is_scannable() {
        let body = pull_error_body(PullFailureKind::Cancelled, "the pull was cancelled", true);
        let line = fetch_or_pull_summary(&pull_kind(), &body);
        assert_eq!(line, "Pull cancelled — the pull was cancelled");
    }

    #[test]
    fn a_message_that_is_not_json_falls_back_to_the_raw_string() {
        let raw = "not json at all";
        assert_eq!(fetch_or_pull_summary(&fetch_kind(), raw), raw);
        assert_eq!(fetch_or_pull_summary(&pull_kind(), raw), raw);
    }

    #[test]
    fn json_of_the_wrong_shape_falls_back_to_the_raw_string() {
        // Valid JSON, but neither FetchSuccess/FetchError nor PullSuccess/PullError — a
        // route that predates this typed contract, or a body reshaped in front of the
        // server. Must not panic, and must not be silently discarded.
        let raw = "{\"unexpected\":\"shape\"}";
        assert_eq!(fetch_or_pull_summary(&fetch_kind(), raw), raw);
        assert_eq!(fetch_or_pull_summary(&pull_kind(), raw), raw);
    }
}

#[cfg(test)]
mod progress_and_cancel_tests {
    use super::*;
    use git_vista_protocol::operation::TransferPhase;

    fn key(s: &str) -> IdempotencyKey {
        IdempotencyKey::new(s).expect("valid idempotency key")
    }

    fn id(s: &str) -> OperationId {
        OperationId::new(s).expect("valid operation id")
    }

    fn fetch_kind() -> OperationKind {
        OperationKind::Fetch {
            remote: "origin".into(),
        }
    }

    /// An admitted Fetch whose server id is already bound — the state every
    /// test in this module starts from, mirroring `core_tests::running()`.
    fn running() -> OperationsCore {
        let mut c = OperationsCore::default();
        c.admit(key("k1"), fetch_kind())
            .expect("first admit accepted");
        c.bind_id(&key("k1"), id("op-1")).expect("bind accepted");
        c
    }

    fn receiving(percent: u8) -> TransferProgress {
        TransferProgress {
            phase: TransferPhase::Receiving,
            percent: Some(percent),
            objects: None,
            total_objects: None,
        }
    }

    /// Mutation this catches: dropping the `entry.progress = progress;`
    /// assignment (or never adding the field), so a live percentage never
    /// reaches anything a view could render.
    #[test]
    fn observe_stores_transfer_progress_when_present() {
        let mut c = running();
        let progress = receiving(42);
        c.observe(
            &id("op-1"),
            OperationState::Running,
            OperationStage::Executing,
            Some(progress),
        )
        .expect("progress accepted");
        let live = c.in_flight().next().expect("still in flight");
        assert_eq!(live.progress, Some(progress));
    }

    /// Mutation this catches: leaving the `NoChange` guard as the old
    /// two-field `entry.state == state && entry.stage == stage` check. A
    /// fetch sits at `Executing` for its whole transfer (M2.20f, #232), so
    /// without comparing `progress` too, every percent tick after the first
    /// would be silently swallowed as "no change" and a live progress bar
    /// would never move.
    #[test]
    fn a_progress_only_change_is_committed_even_when_state_and_stage_repeat() {
        let mut c = running();
        c.observe(
            &id("op-1"),
            OperationState::Running,
            OperationStage::Executing,
            Some(receiving(10)),
        )
        .unwrap();
        let applied = c
            .observe(
                &id("op-1"),
                OperationState::Running,
                OperationStage::Executing,
                Some(receiving(55)),
            )
            .unwrap();
        assert_eq!(
            applied,
            Applied::Committed,
            "percent moved, so this must not report NoChange"
        );
        assert_eq!(c.in_flight().next().unwrap().progress, Some(receiving(55)));
    }

    /// The paired regression guard: truly identical progress must still
    /// report `NoChange`, so the heartbeat/stream-replay case this method's
    /// original two-field check protected stays protected once a third
    /// field is in the comparison.
    #[test]
    fn observing_identical_progress_twice_reports_no_change() {
        let mut c = running();
        c.observe(
            &id("op-1"),
            OperationState::Running,
            OperationStage::Executing,
            Some(receiving(10)),
        )
        .unwrap();
        let applied = c
            .observe(
                &id("op-1"),
                OperationState::Running,
                OperationStage::Executing,
                Some(receiving(10)),
            )
            .unwrap();
        assert_eq!(applied, Applied::NoChange);
    }

    /// Mutation this catches: `request_cancel` removing the entry (e.g. a
    /// copy-pasted `self.in_flight.remove(...)` from `settle`) instead of
    /// only flagging it — ADR 0043 requires the row to stay in-flight until
    /// the real terminal event arrives.
    #[test]
    fn request_cancel_sets_the_flag_without_removing_the_entry() {
        let mut c = running();
        let applied = c.request_cancel(&id("op-1")).expect("cancel accepted");
        assert_eq!(applied, Applied::Committed);
        let live = c.in_flight().next().expect("still in flight");
        assert!(live.cancel_requested);
        assert_eq!(
            c.in_flight().count(),
            1,
            "a cancel request never terminalises the entry (ADR 0043)"
        );
    }

    /// Mutation this catches: dropping the `if entry.cancel_requested {
    /// return Ok(Applied::NoChange); }` guard, which would make a retried
    /// cancel request read as a fresh transition every time.
    #[test]
    fn a_second_cancel_of_the_same_operation_is_a_noop() {
        let mut c = running();
        c.request_cancel(&id("op-1")).unwrap();
        let applied = c.request_cancel(&id("op-1")).unwrap();
        assert_eq!(
            applied,
            Applied::NoChange,
            "a retried or repeated cancel must not be a second transition"
        );
    }

    /// Mutation this catches: `request_cancel` using `.find(...).unwrap()`
    /// (panics) or silently returning `Ok` instead of the same
    /// `UnknownOperation` refusal every other by-id lookup here gives.
    #[test]
    fn cancelling_an_unknown_operation_is_refused() {
        let mut c = OperationsCore::default();
        let err = c.request_cancel(&id("nope")).unwrap_err();
        assert_eq!(err, OperationsRejection::UnknownOperation);
    }
}

#[cfg(test)]
mod progress_line_tests {
    use super::*;

    fn progress(
        phase: TransferPhase,
        percent: Option<u8>,
        objects: Option<u64>,
        total_objects: Option<u64>,
    ) -> TransferProgress {
        TransferProgress {
            phase,
            percent,
            objects,
            total_objects,
        }
    }

    /// The acceptance criterion in miniature: two different reports must read as two
    /// different lines, or a view rendering this function's output could stop moving while
    /// the underlying mechanism kept ticking and no test here would notice.
    #[test]
    fn two_different_percentages_render_two_different_lines() {
        let low = progress(TransferPhase::Receiving, Some(10), None, None);
        let high = progress(TransferPhase::Receiving, Some(90), None, None);
        assert_ne!(progress_line(&low), progress_line(&high));
        assert_eq!(progress_line(&low), "receiving objects 10%");
        assert_eq!(progress_line(&high), "receiving objects 90%");
    }

    /// A different phase at the same percentage must also render distinctly — otherwise a
    /// mutation that dropped `phase` from the format string entirely would still pass the
    /// percentage-only comparison above.
    #[test]
    fn two_different_phases_render_two_different_lines() {
        let receiving = progress(TransferPhase::Receiving, Some(50), None, None);
        let resolving = progress(TransferPhase::Resolving, Some(50), None, None);
        assert_ne!(progress_line(&receiving), progress_line(&resolving));
    }

    /// Percent is preferred over the object counts when both are present — never render a
    /// stale-looking `a/b` alongside a percentage that already says more.
    #[test]
    fn percent_wins_over_object_counts_when_both_are_present() {
        let line = progress_line(&progress(
            TransferPhase::Writing,
            Some(75),
            Some(300),
            Some(400),
        ));
        assert_eq!(line, "writing objects 75%");
    }

    /// No percentage but a full `(a/b)` pair falls back to the counts, exactly the
    /// `Counting`/`Compressing`/`Resolving` shape before git's first percentage line for a
    /// phase arrives.
    #[test]
    fn object_counts_render_when_percent_is_absent() {
        let line = progress_line(&progress(
            TransferPhase::Resolving,
            None,
            Some(12),
            Some(50),
        ));
        assert_eq!(line, "resolving deltas 12/50");
    }

    /// `Enumerating`'s own shape (`operation.rs`'s doc comment on `objects`): a running count
    /// with no total. Must never render as `"12/0"` or similar invented total.
    #[test]
    fn a_running_count_with_no_total_never_fabricates_a_denominator() {
        let line = progress_line(&progress(TransferPhase::Enumerating, None, Some(12), None));
        assert_eq!(line, "enumerating objects 12");
        assert!(!line.contains("0"), "no fabricated total, ever: {line}");
    }

    /// Nothing but the phase: the phase name alone, never a fabricated "0/0" or "0%" — the
    /// literal case `TransferProgress`'s own doc comment (`operation.rs`) exists to forbid.
    #[test]
    fn a_bare_phase_with_no_numbers_renders_as_the_phase_alone() {
        let line = progress_line(&progress(TransferPhase::Counting, None, None, None));
        assert_eq!(line, "counting objects");
        assert!(!line.contains('%') && !line.contains('/'));
    }

    /// A transfer that has started but reported no counts yet must still name its
    /// phase, never render as blank and never invent a number.
    ///
    /// **This replaces a test that proved nothing.** The previous version built a
    /// literal `None`, called `.map(progress_line)` on it, and asserted the result
    /// was `None` — which is a property of `Option::map` in the standard library,
    /// not of this module. `progress_line` was never invoked, so no mutation to it
    /// (or to `view.rs`) could turn that test red. It is exactly the shape this
    /// repo's standing caution names: structurally complete, semantically inert.
    ///
    /// The genuinely uncoverable half — that `view.rs` renders no fragment when
    /// `InFlight::progress` is `None` — lives in wasm-only code with no test
    /// harness in this crate at all (`features/operations/mod.rs` gates `view.rs`
    /// behind `cfg(target_arch = "wasm32")`). It is checked by hand on the device
    /// pass rather than pretended at here. Saying so is worth more than a green
    /// test that cannot fail.
    #[test]
    fn a_transfer_with_no_counts_yet_still_names_its_phase() {
        let bare = TransferProgress {
            phase: TransferPhase::Enumerating,
            percent: None,
            objects: None,
            total_objects: None,
        };
        let line = progress_line(&bare);
        assert!(
            !line.trim().is_empty(),
            "a phase-only progress must still say something: {line:?}"
        );
        // The no-fabrication rule TransferProgress's own doc comment states: with
        // nothing counted, there is no honest percentage and no honest denominator.
        assert!(!line.contains('%'), "invented a percentage: {line:?}");
        assert!(!line.contains('/'), "invented a denominator: {line:?}");
        assert!(!line.contains('0'), "invented a zero count: {line:?}");
    }
}
