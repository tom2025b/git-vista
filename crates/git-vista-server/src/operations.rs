//! The operation registry (M1.08, #61): identity, lifecycle and replay for
//! every mutation.
//!
//! Before this module a write request was anonymous. It had no name, no
//! recorded state, and no existence outside the TCP connection carrying it — so
//! a dropped tunnel cancelled the git command mid-flight, a retry was
//! indistinguishable from a fresh intent, and a lost response made the outcome
//! unknowable. This module gives each mutation the three things that fixes:
//!
//! 1. **A name.** The client sends an [`IdempotencyKey`] for one *user action*;
//!    the server mints an [`OperationId`] for the accepted operation and returns
//!    it in the [`OPERATION_HEADER`](git_vista_protocol::OPERATION_HEADER).
//! 2. **A lifecycle.** `Accepted → Running → Succeeded | Failed`, observable
//!    live through a [`watch`] channel (the SSE stream) and after the fact
//!    through `GET /api/operations/{id}`.
//! 3. **A replayable result.** The recorded `(status, message)` is returned
//!    verbatim to a retry carrying the same key, which plans nothing, takes no
//!    guard, and runs no git.
//!
//! ## The one invariant
//!
//! **A key binds to an operation, not just to a name.** The record stores the
//! plan's `operation_hash`, and a key presented with a *different* operation is
//! refused with 409 rather than answered with someone else's result. Without
//! that, an idempotency key is a way to get the wrong answer confidently.
//!
//! ## Shape of the state
//!
//! One [`watch::Sender<OperationStatus>`] per record is the whole of the mutable
//! state: the current snapshot *is* the watch value, so "read the status", "wait
//! for the terminal result" and "stream progress" are three views of one datum
//! that cannot disagree. Watch coalesces intermediate values under load, which
//! is exactly right here — a progress stream owes the client the *latest* state,
//! and the terminal state is never coalesced away because nothing follows it.
//!
//! The map is guarded by a `std` mutex that is **never held across an await**
//! (the same discipline as [`crate::coordinator`]): every entry point clones the
//! `Arc` out and drops the guard before awaiting anything.
//!
//! ## Bounded in memory, durable on disk
//!
//! Newest [`MAX_RECORDS`] records, [`RECORD_TTL_SECS`] TTL, and **a record that
//! is not terminal is never evicted** — dropping a live record would strand the
//! request awaiting it. [`crate::durable`] (M1.09, #62) persists every record to
//! SQLite and [`rehydrate`] reloads them at startup, so a restart no longer
//! forgets a *finished* operation's outcome — only a running one, which no
//! process can meaningfully resume across a restart anyway (see that module's
//! docs). The staleness gate still covers the case a client re-POSTs before
//! reconciling: the generation has moved, and it is told so.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use axum::http::StatusCode;
use tokio::sync::watch;

use git_vista_protocol::{
    GenerationToken, GitOperation, IdempotencyKey, OperationHash, OperationId, OperationStage,
    OperationState, OperationStatus, RecoveryStrategy, RepositoryToken, TransferProgress,
    UnixSeconds, WorktreeToken,
};

/// How many records the registry keeps. Bounded because the client chooses the
/// key and every distinct key is an entry; terminal records past this count are
/// evicted oldest-first.
pub(crate) const MAX_RECORDS: usize = 256;

/// How long a terminal record stays replayable. Long enough to cover a tunnel
/// outage and a user coming back to the tab; short enough that the registry is
/// not a log.
pub(crate) const RECORD_TTL_SECS: i64 = 3600;

/// How many progress streams may be open at once, process-wide. A client that
/// opens streams and abandons them must not be able to exhaust the server; the
/// cap is generous for real use (one stream per in-flight operation) and hard.
pub(crate) const MAX_LIVE_STREAMS: usize = 32;

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// One operation's live state: the authoritative [`OperationStatus`] snapshot,
/// published through a watch channel so readers, waiters and the progress
/// stream all observe the same value.
pub(crate) struct Record {
    /// The key this record was admitted under, so eviction can drop both index
    /// entries without scanning the key map.
    key: IdempotencyKey,
    status: watch::Sender<OperationStatus>,
    /// The cancellation latch (M2.20c, #229): `false` until an operator asks
    /// for this operation to stop, then `true` forever.
    ///
    /// A separate `watch` rather than a field on the snapshot, deliberately.
    /// The snapshot is what a *client* observes and what `durable` persists;
    /// "someone asked for this to stop" is an instruction to the running
    /// pipeline, not a fact about the operation's outcome — the outcome is
    /// the terminal record the pipeline then writes, which is the only thing
    /// that can honestly say whether the cancel arrived in time. Keeping the
    /// two apart is what stops a record from reading "cancelled" while the
    /// fetch it names actually completed.
    ///
    /// Latching (never reset to `false`) closes the race where a cancel
    /// arrives microseconds before the executor takes its receiver: the
    /// executor's first read already sees `true` and never spawns.
    cancel: watch::Sender<bool>,
}

impl Record {
    /// The current snapshot.
    pub(crate) fn status(&self) -> OperationStatus {
        self.status.borrow().clone()
    }

    /// This operation's server-minted id.
    pub(crate) fn id(&self) -> OperationId {
        self.status.borrow().id.clone()
    }

    /// A receiver for the progress stream. The current value counts as seen, so
    /// a subscriber's first `changed()` reports a real transition — the stream
    /// handler sends the current snapshot itself before looping.
    pub(crate) fn subscribe(&self) -> watch::Receiver<OperationStatus> {
        self.status.subscribe()
    }

    /// Await the terminal state and return the recorded response, verbatim.
    ///
    /// This is what both the request that *owns* an operation and every retry
    /// carrying the same key await, so a duplicate returns byte-identical bytes
    /// to the original. Dropping this future (the client disconnected) cancels
    /// only the waiting — the operation itself runs in a detached task and is
    /// unaffected, which is the whole point of the detached-run model.
    pub(crate) async fn wait_terminal(&self) -> (StatusCode, String) {
        let mut rx = self.subscribe();
        // The sender lives in the registry map for at least as long as the
        // record, so `wait_for` can only fail if the record was dropped while
        // live — which eviction refuses to do. Treated as a 500 rather than
        // unwrapped: a stranded waiter must still answer its client.
        let recorded = match rx.wait_for(|s| s.is_terminal()).await {
            Ok(snapshot) => replay(&snapshot),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The server lost track of this operation. Refresh and check the \
                 repository before retrying."
                    .to_string(),
            ),
        };
        // Bound above rather than returned directly: the `Ref` guard `wait_for`
        // yields borrows `rx`, and a tail expression would keep it alive past
        // the receiver's own drop.
        recorded
    }

    /// Ask this operation to stop (M2.20c, #229).
    ///
    /// Returns `false` — and changes nothing — when the record is already
    /// terminal, because there is nothing left to cancel and answering "ok"
    /// would tell an operator their cancel took effect on an operation that
    /// had already finished. Returns `true` when the latch moved *or* was
    /// already set: a repeated cancel of a still-running operation is
    /// idempotent, not an error.
    ///
    /// Setting the latch is the *whole* of this function. It never touches
    /// the status snapshot: only the pipeline may terminalise a record, and
    /// only after it has observed what actually happened to the repository.
    pub(crate) fn request_cancel(&self) -> bool {
        if self.status.borrow().is_terminal() {
            return false;
        }
        self.cancel.send_replace(true);
        true
    }

    /// A receiver for the cancellation latch, for the executor to select on.
    pub(crate) fn cancel_signal(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    /// Publish an object-transfer report (M2.20c, #229). A no-op once
    /// terminal, and a no-op when nothing changed, so a fetch reporting the
    /// same percentage twice does not wake every subscriber twice.
    fn set_progress(&self, progress: TransferProgress) {
        self.status.send_if_modified(|s| {
            if s.is_terminal() || s.progress == Some(progress) {
                return false;
            }
            s.progress = Some(progress);
            true
        });
    }

    /// Publish a stage change. A no-op once terminal, so a late stage report
    /// from a racing task can never resurrect a finished record.
    fn set_stage(&self, stage: OperationStage) {
        self.status.send_if_modified(|s| {
            if s.is_terminal() || s.stage == stage {
                return false;
            }
            s.stage = stage;
            if s.state == OperationState::Accepted {
                s.state = OperationState::Running;
            }
            true
        });
    }
}

/// The recorded response of a terminal record. A record that is terminal always
/// has both fields; the fallback keeps this total rather than panicking on a
/// shape that would be a bug elsewhere.
fn replay(snapshot: &OperationStatus) -> (StatusCode, String) {
    let status = snapshot
        .status
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let message = snapshot
        .message
        .clone()
        .unwrap_or_else(|| "The operation finished without recording a result.".to_string());
    (status, message)
}

// ---------------------------------------------------------------------------
// The handle the running pipeline holds
// ---------------------------------------------------------------------------

/// The write side of one record, held by the detached task running the
/// pipeline. Exactly one exists per operation, so there is one writer and the
/// lifecycle cannot be advanced from two places.
///
/// **Dropping it without [`finish`](OperationHandle::finish) terminalises the
/// record as a failure.** That is deliberate and load-bearing: if the pipeline
/// panics or the task is aborted, every waiter would otherwise hang forever on
/// a record that will never move again.
pub(crate) struct OperationHandle {
    record: Arc<Record>,
    finished: bool,
}

impl OperationHandle {
    /// What [`finish`](Self::finish) would record, computed without
    /// publishing it.
    ///
    /// **Exists so a caller can persist the terminal state durably before
    /// anyone can observe the operation is done — see issue #158.**
    /// `finish` publishes through a `watch` channel, which is exactly what
    /// unblocks every `wait_terminal` waiter, including the request that owns
    /// this operation. If the durable-journal write for the terminal state
    /// happens *after* `finish`, a waiter can resume on another worker thread
    /// and act on "the operation is done" — including, in the lifecycle
    /// tests, immediately calling `crate::durable::recover()` — before that
    /// write has landed. `recover()` cannot tell "hasn't been journaled yet"
    /// from "orphaned by a crash" and force-fails whatever it finds
    /// non-terminal, so the still-mid-flight row gets marked `Failed` even
    /// though the operation genuinely succeeded. Computing the terminal value
    /// here, persisting it, and only then calling `finish` closes that
    /// window: nothing can observe "done" before the durable write is real.
    pub(crate) fn terminal_status(
        &self,
        status: StatusCode,
        message: &str,
        generation: Option<GenerationToken>,
    ) -> OperationStatus {
        let mut s = self.record.status();
        apply_terminal(&mut s, status, message.to_string(), generation);
        s
    }

    /// Record the terminal result: the response to replay, plus the
    /// post-execution generation.
    ///
    /// The state follows the status code — a 2xx is `Succeeded`, anything else
    /// `Failed` — because a refusal *is* an outcome the client asked for, not an
    /// error in recording it.
    ///
    /// `recovery` is not a parameter here: the planner reports it through
    /// [`note_recovery`] the moment the plan exists, so a record carries "how
    /// would I undo this" even while the operation is still running.
    pub(crate) fn finish(
        mut self,
        status: StatusCode,
        message: String,
        generation: Option<GenerationToken>,
    ) {
        self.finished = true;
        self.record
            .status
            .send_modify(|s| apply_terminal(s, status, message, generation));
    }
}

/// The one place that knows what "finished" means for an [`OperationStatus`]:
/// shared by [`OperationHandle::finish`] (which publishes it) and
/// [`OperationHandle::terminal_status`] (which only computes it), so the two
/// can never drift apart.
fn apply_terminal(
    s: &mut OperationStatus,
    status: StatusCode,
    message: String,
    generation: Option<GenerationToken>,
) {
    s.state = if status.is_success() {
        OperationState::Succeeded
    } else {
        OperationState::Failed
    };
    s.stage = OperationStage::Finished;
    s.status = Some(status.as_u16());
    s.message = Some(message);
    s.generation = generation;
    s.ended_at = Some(UnixSeconds(crate::activity::now_secs()));
}

impl Drop for OperationHandle {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The pipeline panicked or its task was aborted. Terminalise, so every
        // waiter gets an answer instead of hanging on a record nothing will
        // ever move again.
        self.record.status.send_modify(|s| {
            if s.is_terminal() {
                return;
            }
            s.state = OperationState::Failed;
            s.stage = OperationStage::Finished;
            s.status = Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
            s.message = Some(
                "The operation stopped without finishing. Check the repository \
                 before retrying."
                    .to_string(),
            );
            s.ended_at = Some(UnixSeconds(crate::activity::now_secs()));
        });
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// What [`admit`] decided about a request's idempotency key.
pub(crate) enum Admission {
    /// A new operation. This request owns it and must run the pipeline.
    Fresh(OperationHandle, Arc<Record>),
    /// The key names an operation already admitted for the *same* operation:
    /// await its result rather than running anything.
    Existing(Arc<Record>),
    /// The key was reused for a *different* operation. Refused, because
    /// replaying a result computed for something else is worse than failing.
    Conflict,
}

/// The process-wide registry. The `std` mutex guards only the maps and is never
/// held across an await.
struct Registry {
    by_key: HashMap<IdempotencyKey, Arc<Record>>,
    by_id: HashMap<OperationId, Arc<Record>>,
    /// Insertion order, for oldest-first eviction. Ids only — the maps own the
    /// records.
    order: VecDeque<OperationId>,
}

static REGISTRY: OnceLock<StdMutex<Registry>> = OnceLock::new();

fn registry() -> &'static StdMutex<Registry> {
    REGISTRY.get_or_init(|| {
        StdMutex::new(Registry {
            by_key: HashMap::new(),
            by_id: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Admit an operation under `key`, or resolve it to an existing record.
///
/// Admission is a single critical section over the maps, so two concurrent
/// requests carrying the same key cannot both be admitted: the loser sees the
/// winner's record and awaits it.
pub(crate) fn admit(
    key: &IdempotencyKey,
    operation: &GitOperation,
    operation_hash: &OperationHash,
    repository: RepositoryToken,
    worktree: WorktreeToken,
) -> Admission {
    let now = crate::activity::now_secs();
    let mut reg = registry().lock().expect("operation registry lock");
    evict(&mut reg, now);

    if let Some(existing) = reg.by_key.get(key) {
        let existing = Arc::clone(existing);
        return if &existing.status.borrow().operation_hash == operation_hash {
            Admission::Existing(existing)
        } else {
            Admission::Conflict
        };
    }

    let id = mint_id();
    let (status, _) = watch::channel(OperationStatus {
        id: id.clone(),
        state: OperationState::Accepted,
        stage: OperationStage::Queued,
        operation: operation.clone(),
        operation_hash: operation_hash.clone(),
        repository,
        worktree,
        accepted_at: UnixSeconds(now),
        ended_at: None,
        status: None,
        message: None,
        generation: None,
        recovery: None,
        progress: None,
    });
    let (cancel, _) = watch::channel(false);
    let record = Arc::new(Record {
        key: key.clone(),
        status,
        cancel,
    });

    reg.by_key.insert(key.clone(), Arc::clone(&record));
    reg.by_id.insert(id.clone(), Arc::clone(&record));
    reg.order.push_back(id);

    Admission::Fresh(
        OperationHandle {
            record: Arc::clone(&record),
            finished: false,
        },
        record,
    )
}

/// Look one operation up by the id the server handed out.
pub(crate) fn lookup(id: &OperationId) -> Option<Arc<Record>> {
    let reg = registry().lock().expect("operation registry lock");
    reg.by_id.get(id).map(Arc::clone)
}

/// Repopulate the registry from journal rows read at startup (M1.09, #62).
///
/// Called once, before the server accepts requests, with the durable layer's
/// already-closed-out records — every entry here is terminal, because
/// [`crate::durable::recover`] resolved anything a prior process left running
/// into a `Failed` record before returning. This function only ever *adds*
/// entries to an empty registry, so it does not need `admit`'s duplicate/
/// conflict logic.
pub(crate) fn rehydrate(records: Vec<(IdempotencyKey, OperationStatus)>) {
    let mut reg = registry().lock().expect("operation registry lock");
    for (key, status) in records {
        let id = status.id.clone();
        let (status_tx, _) = watch::channel(status);
        // Every rehydrated record is already terminal (see this function's
        // doc), so its latch can never be observed by a pipeline — but it is
        // built unset rather than skipped so `Record` has one shape.
        let (cancel, _) = watch::channel(false);
        let record = Arc::new(Record {
            key: key.clone(),
            status: status_tx,
            cancel,
        });
        reg.by_key.insert(key, Arc::clone(&record));
        reg.by_id.insert(id.clone(), record);
        reg.order.push_back(id);
    }
}

/// Drop terminal records that are past the TTL, then oldest-first until the map
/// is within [`MAX_RECORDS`].
///
/// **A record that is not terminal is never dropped**, at any size or age: a
/// request is awaiting it, and evicting it would strand that request and let a
/// retry start a second git command. An all-live registry therefore grows past
/// the cap rather than break that promise — bounded in practice by the guard,
/// which lets one mutation per repository run at a time.
fn evict(reg: &mut Registry, now: i64) {
    let mut kept: VecDeque<OperationId> = VecDeque::with_capacity(reg.order.len());
    let mut over = reg.order.len().saturating_sub(MAX_RECORDS);

    while let Some(id) = reg.order.pop_front() {
        // Copy out what the decision needs, so the map borrow ends here and the
        // removal below is free to take a mutable one.
        let Some((key, terminal, ended_at)) = reg.by_id.get(&id).map(|record| {
            let snapshot = record.status.borrow();
            (
                record.key.clone(),
                snapshot.is_terminal(),
                snapshot.ended_at,
            )
        }) else {
            continue; // already gone
        };
        let expired = ended_at.is_some_and(|ended| now.saturating_sub(ended.0) > RECORD_TTL_SECS);
        if terminal && (expired || over > 0) {
            if !expired {
                over -= 1;
            }
            reg.by_id.remove(&id);
            reg.by_key.remove(&key);
        } else {
            kept.push_back(id);
        }
    }
    reg.order = kept;
}

/// Mint an unguessable operation id.
///
/// Random rather than sequential: any session-authenticated client can fetch
/// any id it knows, so a countable id would hand out other operations' records
/// for free. 128 bits from the OS CSPRNG, hex-encoded — the same source and
/// shape as the session secrets.
fn mint_id() -> OperationId {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG (getrandom) unavailable");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    OperationId::new(hex).expect("hex is token-shaped and 32 characters")
}

// ---------------------------------------------------------------------------
// Per-request context
// ---------------------------------------------------------------------------

/// What the idempotency middleware puts in scope for one request: the key the
/// client sent, and a slot the planner writes the minted id back into so the
/// middleware can stamp it onto the response.
struct RequestContext {
    key: IdempotencyKey,
    minted: Arc<StdMutex<Option<OperationId>>>,
}

tokio::task_local! {
    /// The current request's idempotency context.
    ///
    /// A task-local rather than a handler argument because the requirement has
    /// to hold at the *chokepoint*: fifteen write handlers all call
    /// `planner::plan_and_execute`, and threading a parameter through every one
    /// of them would make "did we remember the key here?" a per-handler
    /// question again — exactly what M1.06b removed. The read is at the one
    /// place a mutation can begin, so a new handler inherits the requirement
    /// without doing anything.
    static REQUEST: RequestContext;
}

/// Run `f` with `key` in scope as the current request's idempotency key.
/// Whatever id the planner mints lands in `minted` for the caller to read once
/// the future resolves.
pub(crate) async fn with_key<F: Future>(
    key: IdempotencyKey,
    minted: Arc<StdMutex<Option<OperationId>>>,
    f: F,
) -> F::Output {
    REQUEST.scope(RequestContext { key, minted }, f).await
}

/// The idempotency key of the request this task is serving, if any.
///
/// `None` means the request carried no key (refused at the chokepoint) or this
/// is not a request task at all — a test driving the planner directly, or the
/// detached pipeline task, which deliberately does not inherit the scope.
pub(crate) fn current_key() -> Option<IdempotencyKey> {
    REQUEST.try_with(|ctx| ctx.key.clone()).ok()
}

/// Record the id minted for this request, for the response header.
pub(crate) fn note_minted(id: &OperationId) {
    let _ = REQUEST.try_with(|ctx| {
        if let Ok(mut slot) = ctx.minted.lock() {
            *slot = Some(id.clone());
        }
    });
}

// ---------------------------------------------------------------------------
// Progress reporting from inside the pipeline
// ---------------------------------------------------------------------------

tokio::task_local! {
    /// The record the *currently running pipeline* reports stages to.
    ///
    /// Set by the detached task around the pipeline, so `plan_and_execute_in`
    /// and everything under it can report progress without carrying a sink
    /// through five signatures. Absent when the planner is driven directly (the
    /// test suites), which makes [`stage`] a no-op there.
    static PIPELINE: Arc<Record>;
}

/// Run `f` with `record` as the pipeline's progress sink.
pub(crate) async fn with_progress<F: Future>(record: Arc<Record>, f: F) -> F::Output {
    PIPELINE.scope(record, f).await
}

/// Report that the pipeline has reached `stage`. A no-op outside a tracked
/// operation, so the planner can report unconditionally.
pub(crate) fn stage(stage: OperationStage) {
    let _ = PIPELINE.try_with(|record| record.set_stage(stage));
}

/// The id of the operation this task's pipeline is running under, if any.
///
/// `None` means the planner is being driven outside a tracked operation — the
/// contract and coordination suites, which call `plan_and_execute_in`
/// directly. Production always has one: `plan_and_execute` →
/// `plan_and_execute_tracked` runs the pipeline inside [`with_progress`].
///
/// Exists so the guarded region of the pipeline can name the recovery ref it
/// must write **before** a destructive command runs (`refs/git-vista/recovery/
/// <operation id>`; see `planner::pin_recovery`). Read from the task-local
/// rather than threaded through `plan_and_execute_in` → `submit_plan` →
/// `execute` for the same reason [`stage`] is: the requirement belongs at the
/// chokepoint, not in five signatures.
pub(crate) fn current_operation_id() -> Option<OperationId> {
    PIPELINE.try_with(|record| record.id()).ok()
}

/// Report an object-transfer step from inside the pipeline (M2.20c, #229). A
/// no-op outside a tracked operation, so the executor can report
/// unconditionally — exactly like [`stage`] above.
pub(crate) fn progress(progress: TransferProgress) {
    let _ = PIPELINE.try_with(|record| record.set_progress(progress));
}

/// The cancellation latch of the operation this pipeline is running, if it is
/// running under one (M2.20c, #229).
///
/// `None` outside a tracked operation — the contract/coordination suites
/// drive the planner directly — which the executor must read as *"nobody can
/// cancel this"*, never as *"cancelled"*. Fail-safe in the right direction:
/// an untracked run completes normally instead of refusing to start.
pub(crate) fn cancel_signal() -> Option<watch::Receiver<bool>> {
    PIPELINE.try_with(|record| record.cancel_signal()).ok()
}

/// Record how the pre-operation state could be recovered, from the plan the
/// pipeline just built. Reported as soon as the plan exists rather than at the
/// end, so a client watching a *running* destructive operation can already see
/// its way back. A no-op outside a tracked operation.
pub(crate) fn note_recovery(recovery: &RecoveryStrategy) {
    let _ = PIPELINE.try_with(|record| {
        record.status.send_if_modified(|s| {
            if s.is_terminal() || s.recovery.as_ref() == Some(recovery) {
                return false;
            }
            s.recovery = Some(recovery.clone());
            true
        });
    });
}

// ---------------------------------------------------------------------------
// Stream admission
// ---------------------------------------------------------------------------

/// One open progress stream. Counts against [`MAX_LIVE_STREAMS`] until dropped,
/// including when the client disconnects (axum drops the response body's stream
/// and this along with it).
pub(crate) struct StreamPermit;

static LIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);

impl StreamPermit {
    /// Take a permit, or `None` when the process is already at its cap.
    pub(crate) fn acquire() -> Option<StreamPermit> {
        let taken = LIVE_STREAMS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_LIVE_STREAMS).then_some(n + 1)
        });
        taken.ok().map(|_| StreamPermit)
    }
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        LIVE_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{CommitMessage, GitOperation};

    fn op(message: &str) -> GitOperation {
        GitOperation::CommitOnHead {
            message: CommitMessage::new(message).unwrap(),
            allow_empty: false,
        }
    }

    fn hash(seed: char) -> OperationHash {
        OperationHash::new(seed.to_string().repeat(64)).unwrap()
    }

    fn tokens() -> (RepositoryToken, WorktreeToken) {
        (
            RepositoryToken::new("a".repeat(64)).unwrap(),
            WorktreeToken::new("b".repeat(64)).unwrap(),
        )
    }

    /// Keys are process-global state shared with every other test in this
    /// binary, so each test mints its own.
    fn key(name: &str) -> IdempotencyKey {
        IdempotencyKey::new(format!("test-{name}")).unwrap()
    }

    fn admit_op(k: &IdempotencyKey, o: &GitOperation, h: &OperationHash) -> Admission {
        let (repo, worktree) = tokens();
        admit(k, o, h, repo, worktree)
    }

    #[test]
    fn a_new_key_is_admitted_and_gets_an_id() {
        let k = key("new-key");
        let record = match admit_op(&k, &op("first"), &hash('a')) {
            Admission::Fresh(handle, record) => {
                assert_eq!(record.status().state, OperationState::Accepted);
                assert_eq!(record.status().stage, OperationStage::Queued);
                handle.finish(StatusCode::OK, "done".into(), None);
                record
            }
            _ => panic!("a fresh key must be admitted"),
        };
        assert_eq!(lookup(&record.id()).map(|r| r.id()), Some(record.id()));
    }

    #[test]
    fn the_same_key_with_the_same_operation_resolves_to_the_first_record() {
        let k = key("same-op");
        let Admission::Fresh(handle, first) = admit_op(&k, &op("once"), &hash('b')) else {
            panic!("first admission");
        };
        handle.finish(StatusCode::OK, "the original answer".into(), None);

        match admit_op(&k, &op("once"), &hash('b')) {
            Admission::Existing(second) => assert_eq!(second.id(), first.id()),
            _ => panic!("a repeated key must resolve to the existing record"),
        }
    }

    /// The load-bearing safety property: a key must never answer with a result
    /// computed for a different operation.
    #[test]
    fn the_same_key_with_a_different_operation_is_a_conflict() {
        let k = key("different-op");
        let Admission::Fresh(handle, _) = admit_op(&k, &op("one"), &hash('c')) else {
            panic!("first admission");
        };
        handle.finish(StatusCode::OK, "done".into(), None);

        assert!(matches!(
            admit_op(&k, &op("something else entirely"), &hash('d')),
            Admission::Conflict
        ));
    }

    #[tokio::test]
    async fn a_terminal_record_replays_the_recorded_response_verbatim() {
        let k = key("replay");
        let Admission::Fresh(handle, record) = admit_op(&k, &op("replayed"), &hash('e')) else {
            panic!("admission");
        };
        handle.finish(
            StatusCode::CONFLICT,
            "Refused: the repository moved.".into(),
            None,
        );

        let (status, body) = record.wait_terminal().await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, "Refused: the repository moved.");
    }

    /// A waiter must never hang because the pipeline died: dropping the handle
    /// without finishing terminalises the record.
    #[tokio::test]
    async fn dropping_the_handle_unfinished_fails_the_record() {
        let k = key("dropped");
        let Admission::Fresh(handle, record) = admit_op(&k, &op("abandoned"), &hash('f')) else {
            panic!("admission");
        };
        drop(handle);

        let (status, body) = record.wait_terminal().await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("stopped without finishing"), "{body}");
        assert_eq!(record.status().state, OperationState::Failed);
    }

    #[test]
    fn stages_move_the_state_to_running_and_stop_at_terminal() {
        let k = key("stages");
        let Admission::Fresh(handle, record) = admit_op(&k, &op("staged"), &hash('a')) else {
            panic!("admission");
        };
        record.set_stage(OperationStage::Planning);
        assert_eq!(record.status().state, OperationState::Running);
        assert_eq!(record.status().stage, OperationStage::Planning);

        handle.finish(StatusCode::OK, "done".into(), None);
        record.set_stage(OperationStage::Executing);
        assert_eq!(
            record.status().stage,
            OperationStage::Finished,
            "a terminal record must not be moved by a late stage report"
        );
    }

    /// Eviction may drop terminal records, but never a live one — a live record
    /// has a request awaiting it.
    #[test]
    fn eviction_never_drops_a_live_record() {
        let live_key = key("evict-live");
        let Admission::Fresh(handle, live) = admit_op(&live_key, &op("live"), &hash('b')) else {
            panic!("admission");
        };

        // Overflow the cap with terminal records.
        for n in 0..(MAX_RECORDS + 8) {
            let k = IdempotencyKey::new(format!("test-evict-filler-{n}")).unwrap();
            if let Admission::Fresh(h, _) = admit_op(&k, &op("filler"), &hash('c')) {
                h.finish(StatusCode::OK, "done".into(), None);
            }
        }

        assert!(
            lookup(&live.id()).is_some(),
            "a record still running must survive any amount of pressure"
        );
        handle.finish(StatusCode::OK, "done".into(), None);
    }

    #[test]
    fn minted_ids_are_unique_and_token_shaped() {
        let a = mint_id();
        let b = mint_id();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_alphanumeric()));
    }

    #[tokio::test]
    async fn the_key_is_only_visible_inside_its_scope() {
        assert!(current_key().is_none());
        let slot = Arc::new(StdMutex::new(None));
        let seen = with_key(key("scoped"), Arc::clone(&slot), async { current_key() }).await;
        assert_eq!(seen, Some(key("scoped")));
        assert!(current_key().is_none(), "the scope must not leak");
    }

    #[tokio::test]
    async fn the_minted_id_reaches_the_slot_for_the_response_header() {
        let slot: Arc<StdMutex<Option<OperationId>>> = Arc::new(StdMutex::new(None));
        let id = mint_id();
        with_key(key("minted"), Arc::clone(&slot), async {
            note_minted(&id);
        })
        .await;
        assert_eq!(slot.lock().unwrap().as_ref(), Some(&id));
    }

    #[test]
    fn stream_permits_are_capped_and_released_on_drop() {
        let permits: Vec<_> = (0..MAX_LIVE_STREAMS)
            .map(|_| StreamPermit::acquire().expect("under the cap"))
            .collect();
        assert!(
            StreamPermit::acquire().is_none(),
            "the cap must be hard, not advisory"
        );
        drop(permits);
        assert!(
            StreamPermit::acquire().is_some(),
            "dropping a stream must free its permit"
        );
    }
}
