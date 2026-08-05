//! The reactive wrapper over the operations core — wasm only (M1.11, #64).
//!
//! Everything decidable is decided in [`super::core`], on the host, under test. This file
//! holds only what genuinely needs Leptos: reading the live epoch out of a signal, and
//! keeping the pending intent in a `StoredValue` that survives the closures that write it.

use std::cell::Cell;
use std::rc::Rc;

use leptos::{
    spawn_local, store_value, RwSignal, SignalGetUntracked, SignalUpdate, SignalWith,
    SignalWithUntracked, StoredValue,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use git_vista_protocol::operation::{
    IdempotencyKey, OperationId, OperationState, OperationStatus, ProgressEvent, PROGRESS_EVENT,
    RESULT_EVENT,
};
use git_vista_protocol::PROTOCOL_VERSION;

use crate::api::{self, WriteReceipt};
use crate::features::core_traits::{RequestKey, RequestTarget};
use crate::features::dialogs::core::{Dialog, ErrorNotice};
use crate::features::dialogs::signals::Dialogs;
use crate::features::graph::core::GraphCore;
use crate::features::operations::core::{
    escalation, latest_wins, remote_op_kind, resume_decision, InFlightRemoteOp, IntentSeq,
    OperationsCore, PendingIntent, ResumeDecision, Settled, Settlement,
};
use crate::features::operations::kind::OperationKind;
use crate::features::shell::signals::Shell;
use crate::prefs;

/// Mint the next value of a click-order sequence.
///
/// Call this **synchronously inside the event handler**, before any `await`. That is the
/// whole point: the sequence must record when the user acted, and a value taken after the
/// pre-check resolves would record when the network answered instead.
///
/// Free-standing because `picker.rs` orders its *own* messages with a second, unrelated
/// [`IntentSeq`]; [`Operations::next_seq`] is the same rule applied to the one this
/// feature owns.
pub fn next_seq(intent_seq: StoredValue<IntentSeq>) -> u64 {
    // `try_update_value` returns `None` only when the owning scope is already disposed, in
    // which case the continuation cannot write anything either. Sequence 0 is the reserved
    // "no intent" value, so falling back to it makes such an intent lose every comparison
    // rather than spuriously win one.
    intent_seq.try_update_value(|s| s.next()).unwrap_or(0)
}

/// The reactive handle every feature uses to start and watch a write.
///
/// `Copy`, like the rest of the app's signal bundles, so it drops into a closure without
/// ceremony. The core it wraps is created in `App` — **above** `graph_canvas` — which is
/// what makes acceptance criterion 2 true: an epoch bump rebuilds the canvas and every
/// overlay inside it, but the operations core is not inside it, so an in-flight write
/// survives a panel change, a repository re-read, and the dialog that started it.
#[derive(Clone, Copy)]
pub struct Operations {
    core: RwSignal<OperationsCore>,
    /// Where a settled write's invalidation lands (M1.11 D3, Task 6): `on_invalidate`
    /// skips the epoch bump when the settlement's generation matches what the graph
    /// already has, so a write that didn't move the repository doesn't force a re-read.
    graph: RwSignal<GraphCore>,
    /// Mints the click-order sequence for branch operations. A `StoredValue`, not a
    /// signal: minting is bookkeeping done inside an event handler, and nothing renders
    /// from it.
    intent_seq: StoredValue<IntentSeq>,
    /// The newest branch-operation intent that has actually opened its confirm dialog. A
    /// menu item's `fetch_head_branch()` pre-check resolves in network order, so each
    /// continuation compares against this before committing and a straggler from an
    /// earlier click is dropped instead of reopening its dialog over the one the user is
    /// looking at.
    ///
    /// Moved here from the `Overlays` bundle in Task 8 (M1.11, #64), which also moved it
    /// above `graph_canvas`. `Overlays` reset both on every epoch bump and argued that was
    /// correct; it is equally correct not to, and for the same reason the old comment
    /// gave — an intent raised before the bump fails [`RequestKey::is_current`] anyway,
    /// and sequences only ever increase, so a surviving record can never out-rank a newer
    /// tap.
    pending_intent: StoredValue<Option<PendingIntent>>,
    /// Where this feature's own failures become words on screen (#316) —
    /// see [`ErrorSink`] for why it is installed after construction rather
    /// than passed to [`Operations::new`].
    error_sink: StoredValue<Option<ErrorSink>>,
}

/// The handles needed to raise the app's error modal, held so a failure
/// discovered inside a `spawn_local` continuation — where there is no view,
/// no props and no return value — can still be reported to the user.
///
/// Installed by [`Operations::install_error_sink`] rather than taken by
/// [`Operations::new`] because of the order `App` has to build things in: the
/// operations registry is created *early*, above `graph_canvas`, so an
/// in-flight write survives the epoch bump its own completion causes, while
/// [`Shell`] and [`Dialogs`] are created much later, alongside the overlays
/// they own. Nothing can hold both at construction time without moving one of
/// them, and moving the registry down is precisely the bug M1.11 fixed.
///
/// `None` until installed, and that case is handled rather than assumed away:
/// a failure raised before wiring goes to the browser console instead of
/// vanishing.
#[derive(Clone, Copy)]
struct ErrorSink {
    shell: Shell,
    dialogs: Dialogs,
}

impl Operations {
    pub fn new(core: RwSignal<OperationsCore>, graph: RwSignal<GraphCore>) -> Self {
        Self {
            core,
            graph,
            intent_seq: store_value(IntentSeq::default()),
            pending_intent: store_value(None::<PendingIntent>),
            error_sink: store_value(None::<ErrorSink>),
        }
    }

    /// Wire this feature's failure reports into the app's error modal (#316).
    ///
    /// Call once from `App`, after [`Shell`] and [`Dialogs`] exist. Until it
    /// is called, [`Operations::cancel`]'s refusals reach the console and not
    /// the user — see [`ErrorSink`] for why the two-step is unavoidable.
    pub fn install_error_sink(&self, shell: Shell, dialogs: Dialogs) {
        self.error_sink
            .set_value(Some(ErrorSink { shell, dialogs }));
    }

    /// Mint the next click-order sequence for a branch operation.
    ///
    /// Call this **synchronously inside the event handler**, before any `await` — see
    /// [`next_seq`], which is where the rule and the disposed-scope fallback live.
    pub fn next_seq(&self) -> u64 {
        next_seq(self.intent_seq)
    }

    /// Stamp a request with the repository state it was raised against.
    ///
    /// `generation` is `None` here: the branch-operation endpoints predate M1.10 and do
    /// not report one, so these intents are fenced by epoch alone — which is exactly the
    /// case [`RequestKey::is_current`] documents as correct for pre-generation endpoints.
    pub fn request_key(&self, target: RequestTarget) -> RequestKey {
        RequestKey {
            epoch: self.graph.get_untracked().epoch(),
            generation: None,
            target,
        }
    }

    /// Whether a resolved pre-check may still open its dialog; records it if so.
    ///
    /// Two independent reasons to drop a continuation, and both matter:
    ///
    /// * the repository moved while the pre-check was in flight (a Refresh, a repo switch,
    ///   a drift reload), so the answer describes a repository the user is no longer
    ///   looking at;
    /// * a later tap already owns the dialog, so committing now would replace what the
    ///   user is looking at with something they asked for *earlier*.
    pub fn admit_intent(&self, intent: &PendingIntent) -> bool {
        if !intent
            .key
            .is_current(self.graph.get_untracked().epoch(), None)
        {
            return false;
        }
        let wins = self
            .pending_intent
            .try_with_value(|current| latest_wins(current.as_ref(), intent))
            .unwrap_or(false);
        if !wins {
            return false;
        }
        self.pending_intent.set_value(Some(intent.clone()));
        true
    }

    /// The in-flight and recently-settled registry, for views that render it.
    pub fn core(&self) -> RwSignal<OperationsCore> {
        self.core
    }

    /// Start one write.
    ///
    /// The operation is admitted **before** the request goes out, under the key the request
    /// will carry, so it is represented for its whole flight rather than only after the
    /// response lands. `dialogs/confirm.rs` used to clear the dialog and then `spawn_local`
    /// a future nothing held a reference to — which is why closing a panel mid-write left
    /// no trace that anything was happening.
    ///
    /// # Why this fires the write and then looks its id up separately
    ///
    /// The obvious arrangement — `send(...).await`, then bind, persist and
    /// subscribe — is what this used to do, and it silently disabled every
    /// live feature built on top of it. A tracked write's response is
    /// withheld until the operation is **already terminal**:
    /// `planner::plan_and_execute_tracked` ends with
    /// `record.wait_terminal().await`
    /// (`crates/git-vista-server/src/planner.rs:204`), so the
    /// `x-git-vista-operation` header only ever arrives after there is
    /// nothing left to watch. Awaiting it meant the Cancel button appeared
    /// at the moment cancelling became impossible, the progress stream was
    /// subscribed to after the transfer had ended, and the `localStorage`
    /// entry a reload recovers from was written after the reload window had
    /// closed. Three dead features, one `await`.
    ///
    /// So the write goes out on its own task and is **not** awaited here,
    /// while a second task asks the server what id it minted for the key
    /// this client already holds — `api::resolve_operation_id`, over
    /// `GET /api/operations/by-key/{key}`. The server knows the id from the
    /// instant `admit` returns (`operations::note_minted`, `planner.rs:142`),
    /// which is the whole flight earlier than the response.
    ///
    /// # The two tasks, and the one thing they must not both do
    ///
    /// Either task can be the one that learns the id first, and on a fast
    /// operation the response genuinely wins. Both therefore go through
    /// [`bind_and_watch`], which binds, persists and subscribes **once** and
    /// records that it did in a shared [`DispatchBinding`]. That cell is the
    /// whole concurrency argument: wasm is single-threaded and
    /// [`bind_and_watch`] contains no `await`, so its check-then-set cannot
    /// interleave, and `subscribe` therefore cannot run twice for one
    /// operation.
    ///
    /// A terminal event that lands *while* the resolver is still polling is
    /// not lost, and not because of anything this client does: the progress
    /// stream's first event is always the current snapshot, so a subscriber
    /// that arrives after the operation finished is answered immediately with
    /// the terminal record rather than waiting for a transition that already
    /// happened (`handlers/operations.rs::operation_events`, and its doc
    /// comment saying exactly that).
    ///
    /// # Degrading rather than breaking
    ///
    /// A server that has no `by-key` route answers `404` forever, the
    /// resolver spends its budget and returns `None`, and the write settles
    /// from its own response through the same [`bind_and_watch`] call the old
    /// code made — identical behaviour to before this existed, minus the live
    /// half. Nothing in the write path depends on the lookup succeeding.
    pub fn dispatch(&self, kind: OperationKind) {
        let key = api::new_idempotency_key();
        let core = self.core;
        let graph = self.graph;
        // A rejected admission means this exact key already names a different operation —
        // impossible for a freshly minted key, but refusing beats sending a request the
        // registry cannot account for.
        match core.try_update(|c| c.admit(key.clone(), kind.clone())) {
            Some(Ok(_)) => {}
            _ => return,
        }
        // Shared by the two tasks below. An `Rc<Cell<_>>` rather than a
        // `StoredValue`: this is per-dispatch bookkeeping with no reactive
        // reader, and it must outlive the reactive owner that started the
        // write — a confirm dialog that closes the instant it dispatches
        // disposes that owner, and a disposed `StoredValue` would start
        // silently refusing the writes this state machine depends on.
        let binding = Rc::new(Cell::new(DispatchBinding::Pending));

        // The write itself, fired and deliberately not awaited by anything
        // that needs to act during the operation.
        let sent = kind.clone();
        let write_binding = Rc::clone(&binding);
        let write_key = key.clone();
        spawn_local(async move {
            match send(&sent, write_key.clone()).await {
                // Either a refusal that never left the client (Visualize mode, a body
                // that would not serialize) or a transport failure after both of
                // `send_write_with_key`'s attempts.
                Err(reason) => {
                    // Only settle locally if nothing has bound a real id. If the
                    // resolver already found one, the operation demonstrably reached
                    // `admit` and is running server-side, and this file's standing
                    // rule applies: a transport failure is not an outcome. Settling
                    // it here would mark a live fetch `Failed` and — worse — would
                    // clobber the bound id, since `bind_id` overwrites rather than
                    // refuses (`core::OperationsCore::bind_id`).
                    //
                    // What happens instead, stated plainly because it is a
                    // deliberate behaviour change: the entry stays in flight,
                    // showing the truth, and its outcome arrives on the progress
                    // stream this task never touches — or, if that connection died
                    // too, on the next boot, because the resolver already wrote the
                    // `localStorage` entry `resume_inflight_remote_op` reads. Before
                    // the id could be bound early, neither recovery existed and the
                    // only option was to guess `Failed`.
                    if write_binding.get() == DispatchBinding::Pending {
                        write_binding.set(DispatchBinding::Closed);
                        settle_locally(core, graph, &write_key, reason, false);
                    }
                }
                Ok(receipt) => match receipt.operation.clone() {
                    // Operation-tracked: bind the server's handle, then read the
                    // record off the stream. The stream is what carries the
                    // post-execution generation, which the write response does not.
                    // A no-op when the resolver got here first — it subscribed to
                    // that same stream, and the terminal record is on it.
                    Some(id) => {
                        bind_and_watch(core, graph, &write_binding, &write_key, &sent, id);
                    }
                    // Not operation-tracked (`select`, `rescan`, `clone`,
                    // `delete-clone` never reach the server's planner). The HTTP
                    // answer is the whole outcome — and the resolver can never have
                    // bound anything for such a write, because a key that never
                    // reaches `admit` is never in the registry `lookup_by_key`
                    // reads.
                    None => {
                        if write_binding.get() == DispatchBinding::Pending {
                            write_binding.set(DispatchBinding::Closed);
                            settle_locally(core, graph, &write_key, receipt.message, receipt.ok);
                        }
                    }
                },
            }
        });

        // The id lookup, racing the write it describes — and expected to win.
        spawn_local(async move {
            // Stop polling the moment the write's own response has made the
            // question moot, so a 50ms delete does not leave a loop running
            // against a key nobody is waiting on.
            let watcher = Rc::clone(&binding);
            let stop = move || watcher.get() != DispatchBinding::Pending;
            // `None` is the ordinary degraded answer, not a failure: the
            // budget ran out, or the write already settled. Either way the
            // response task owns the outcome and there is nothing to report.
            let Some(id) = api::resolve_operation_id(key.clone(), stop).await else {
                return;
            };
            bind_and_watch(core, graph, &binding, &key, &kind, id);
        });
    }

    /// Acknowledge a settled outcome, removing it from the recent list.
    pub fn dismiss(&self, id: &OperationId) {
        let id = id.clone();
        self.core.update(|c| {
            c.dismiss(&id);
        });
    }

    /// The follow-up a settled operation invites, if any — today only an unmerged delete
    /// offering a force delete. Returns it once and dismisses the entry, so re-rendering
    /// cannot re-offer the same escalation twice.
    pub fn take_escalation(&self) -> Option<OperationKind> {
        let found = self.core.with_untracked(|c| {
            c.recent().find_map(|s: &Settled| {
                s.outcome
                    .message
                    .as_deref()
                    .and_then(|m| escalation(&s.kind, m))
                    .map(|next| (s.id.clone(), next))
            })
        });
        let (id, next) = found?;
        self.core.update(|c| {
            c.dismiss(&id);
        });
        Some(next)
    }

    /// How many writes are in flight, for a view that wants to say so.
    pub fn in_flight_count(&self) -> usize {
        self.core.with(|c| c.in_flight().count())
    }

    /// Ask the server to cancel an in-flight Fetch/Pull (#232, ADR 0043/0044).
    ///
    /// `POST /api/operations/{id}/cancel` only sets a cancellation latch — it
    /// never terminalises the record itself, only the pipeline may do that,
    /// and only after it has observed what actually happened to the
    /// repository (ADR 0043). So this call has exactly one honest effect: on
    /// `202 Requested`, flip `InFlight::cancel_requested` so the row can show
    /// "cancelling…" instead of removing the row or marking it done. The
    /// operation's real resolution still arrives, unchanged, through the
    /// existing [`subscribe`]/[`commit_settlement`] path once the executor
    /// observes the kill and the terminal event carries `Cancelled`.
    ///
    /// Every other outcome — `AlreadyFinished`, `NotCancellable`, `Unknown`
    /// (an evicted or never-admitted id), or a transport failure — changes
    /// nothing client-side. A transport failure is not an outcome, the same
    /// rule [`subscribe`]'s own `on_error` arm and this file's `send` already
    /// hold for every other write: the operation may still be running, and
    /// its real resolution is still coming through the stream this call
    /// never touches.
    ///
    /// # Changing nothing is not the same as saying nothing
    ///
    /// This method used to be a single `if let Ok(Requested) = …` with no
    /// `else`, which made all four of those outcomes indistinguishable from a
    /// button that does not work. Tapping Cancel on a dropped SSH tunnel
    /// produced no row change, no modal, no console line — nothing, forever.
    /// That is the exact failure #316 exists to end: a write that fails must
    /// say so in the app's own modal, in words.
    ///
    /// So every arm is matched, and the four non-`Requested` ones report
    /// through [`ErrorSink`] — the same `dialogs.open(Dialog::Error)` +
    /// `shell.open_error(...)` pair the context menu uses for its own
    /// refusals. What none of them do is change the operation's state: the
    /// row stays in flight, because it *is* in flight, and its real
    /// resolution still arrives on the stream.
    pub fn cancel(&self, id: &OperationId) {
        let core = self.core;
        let sink = self.error_sink;
        let id = id.clone();
        spawn_local(async move {
            match api::cancel_operation_request(&id).await {
                // The latch is set. The only client-side effect there is
                // honest ground for: the row reads "cancelling…" until the
                // pipeline says what it managed to do.
                Ok(api::CancelOutcome::Requested) => {
                    let _ = core.try_update(|c| c.request_cancel(&id));
                }
                // 409. The operation finished between the button rendering and
                // the tap landing — a race the UI cannot close, only explain.
                Ok(api::CancelOutcome::AlreadyFinished) => report_error(
                    sink,
                    "Couldn't cancel",
                    "This operation had already finished by the time the cancel \
                     reached the server, so there was nothing left to stop. Its \
                     result is on its way."
                        .to_string(),
                ),
                // 409. `planner::honours_cancellation` said no. The menu is
                // meant to keep the button from being offered at all here
                // (`OperationKind::is_cancellable`), so reaching this means the
                // two disagree — which the user should hear about rather than
                // experience as a dead button.
                Ok(api::CancelOutcome::NotCancellable) => report_error(
                    sink,
                    "Couldn't cancel",
                    "The server will not cancel this kind of operation — it has no \
                     cancellation point to stop at. Wait for it to finish."
                        .to_string(),
                ),
                // 404. The id was read off this operation's own bind, so this
                // is close to impossible mid-session; say so plainly rather
                // than swallowing it, because if it ever does happen the
                // client's idea of what is running has diverged from the
                // server's and that is worth seeing.
                Ok(api::CancelOutcome::Unknown) => report_error(
                    sink,
                    "Couldn't cancel",
                    "The server has no record of this operation, so there was nothing \
                     to cancel. It may have finished long enough ago to be forgotten — \
                     refresh to see what the repository actually looks like."
                        .to_string(),
                ),
                // Offline refusal, Visualize refusal, timeout, network error, or
                // an unexpected status. `api::cancel_operation_request` has
                // already unwrapped each of these into prose meant for a human
                // (#316), so it is shown verbatim rather than re-worded here.
                Err(reason) => report_error(sink, "Couldn't cancel", reason),
            }
        });
    }

    /// Resume watching a Fetch/Pull that was still in flight when the tab
    /// reloaded or was suspended and resumed (#232, M2.20f). Call once, at
    /// boot, immediately after [`Operations::new`] — see
    /// [`resume_inflight_remote_op`] for what it actually does; this is
    /// just the method-shaped door into it, matching every other action
    /// this bundle exposes.
    pub fn resume_from_storage(&self) {
        resume_inflight_remote_op(self.core, self.graph);
    }
}

/// Which of a dispatch's two tasks has claimed the operation's outcome
/// (M2.20f, #232).
///
/// [`Operations::dispatch`] runs the write and the `by-key` id lookup
/// concurrently, and either can finish first. This is the one fact they
/// coordinate on, and it exists to enforce exactly two rules: `subscribe`
/// runs at most once per operation, and a write that has already been settled
/// locally is never re-bound to a server id afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchBinding {
    /// No id yet. The resolver is still polling, and the write's own response
    /// has not arrived.
    Pending,
    /// A server id is bound, persisted where applicable, and its progress
    /// stream is subscribed to — all exactly once.
    Bound,
    /// The operation was settled without ever having a server id: a refusal
    /// that never left the client, a transport failure with nothing bound, or
    /// a write the planner never tracked.
    Closed,
}

/// Attach `id` to the in-flight entry `key` names, remember it for a reload,
/// and start watching its progress stream — **once**, whichever of
/// [`Operations::dispatch`]'s two tasks gets here first.
///
/// **Contains no `await`, and must not grow one.** That is what makes the
/// check-then-set atomic: wasm is single-threaded, so a function with no
/// suspension point cannot interleave with the other task, and the `Pending`
/// test below therefore cannot be passed by both callers. Introduce an await
/// between the test and the `set` and the second caller subscribes to a
/// stream the first is already reading — two `EventSource`s against
/// `MAX_LIVE_STREAMS` for one operation, and two settlements racing into
/// `commit_settlement`.
fn bind_and_watch(
    core: RwSignal<OperationsCore>,
    graph: RwSignal<GraphCore>,
    binding: &Cell<DispatchBinding>,
    key: &IdempotencyKey,
    kind: &OperationKind,
    id: OperationId,
) {
    if binding.get() != DispatchBinding::Pending {
        return;
    }
    if core
        .try_update(|c| c.bind_id(key, id.clone()))
        .is_none_or(|r| r.is_err())
    {
        // The owning scope is gone, or the entry is no longer in flight
        // (already settled, already dismissed). Either way there is nothing
        // to watch and nothing the other task should try again.
        binding.set(DispatchBinding::Closed);
        return;
    }
    binding.set(DispatchBinding::Bound);
    // #232, M2.20f: persist a Fetch/Pull's identity so a reload or Safari tab
    // suspend/resume can find it again — see `resume_inflight_remote_op`. A
    // no-op for every other kind. Reached from the resolver it is finally
    // written *during* the transfer, which is the only time a reload can
    // happen mid-operation and therefore the only time the entry is worth
    // anything.
    persist_if_remote_op(kind, &id);
    subscribe(core, graph, id, STREAM_REATTACH_MAX_ATTEMPTS);
}

/// Put a failure this feature discovered in front of the user, in the app's
/// own modal (#316).
///
/// The `dialogs.open(Dialog::Error)` call is the ghost-click guard and comes
/// first, then the notice — the ordering `Shell::open_error`'s own doc comment
/// requires and every other opener in the app follows.
///
/// With no sink installed the message goes to the console instead. That is a
/// real degradation and is deliberately not silent: a failure with nowhere to
/// go must still leave a trace, since "nothing happened at all" is the bug
/// this function exists to stop.
fn report_error(sink: StoredValue<Option<ErrorSink>>, title: &'static str, body: String) {
    // `try_with_value`, not `with_value`: this runs from a `spawn_local`
    // continuation that can outlive the scope which stored the sink.
    match sink.try_with_value(|s| *s).flatten() {
        Some(ErrorSink { shell, dialogs }) => {
            dialogs.open(Dialog::Error);
            shell.open_error(ErrorNotice { title, body });
        }
        None => {
            web_sys::console::error_1(&format!("git-vista: {title}: {body}").into());
        }
    }
}

/// One arm per `api.rs` write function — the mapping that made moving `PendingOp` here a
/// rename rather than a redesign.
async fn send(kind: &OperationKind, key: IdempotencyKey) -> Result<WriteReceipt, String> {
    match kind {
        OperationKind::Merge { branch, .. } => {
            api::branch_op_request("/api/merge", branch, key).await
        }
        OperationKind::Push { branch } => api::branch_op_request("/api/push", branch, key).await,
        OperationKind::Checkout { branch, .. } => {
            api::branch_op_request("/api/checkout", branch, key).await
        }
        OperationKind::Delete { branch, .. } => {
            api::branch_op_request("/api/delete-branch", branch, key).await
        }
        OperationKind::ForceDelete { branch } => {
            api::branch_op_request("/api/force-delete-branch", branch, key).await
        }
        OperationKind::Rebase { .. } => api::rebase_request(key).await,
        OperationKind::Fetch { remote } => api::fetch_request(remote, key).await,
        OperationKind::Pull {
            remote,
            branch,
            strategy,
        } => api::pull_request(remote, branch, *strategy, key).await,
        OperationKind::Undo(u) => api::undo_request(&u.action, key).await,
        // Two arms, not one parameterised by a bool — mirroring the two
        // separate `GitOperation` variants and the two separate endpoints
        // behind them (#71, M2.18a/#219, wired by M2.18b/#220).
        OperationKind::DiscardTrackedPaths { paths } => {
            api::discard_tracked_paths_request(paths.clone(), key).await
        }
        OperationKind::DeleteUntrackedPaths { paths } => {
            api::delete_untracked_paths_request(paths.clone(), key).await
        }
    }
}

/// Persist a just-bound Fetch/Pull's identity to `localStorage` (#232,
/// M2.20f), so a reload or Safari tab suspend/resume can find it again on
/// boot — see [`resume_inflight_remote_op`]. Every other operation kind is
/// a no-op: only Fetch and Pull carry the "reconnect, don't lose track"
/// acceptance criterion, and persisting one that settles in milliseconds
/// (a delete, a checkout) would just leave stale storage behind for no
/// reader to ever consult.
fn persist_if_remote_op(kind: &OperationKind, id: &OperationId) {
    let entry = match kind {
        OperationKind::Fetch { remote } => InFlightRemoteOp {
            id: id.as_str().to_string(),
            remote: remote.clone(),
            branch: None,
            strategy: None,
        },
        OperationKind::Pull {
            remote,
            branch,
            strategy,
        } => InFlightRemoteOp {
            id: id.as_str().to_string(),
            remote: remote.clone(),
            branch: Some(branch.clone()),
            strategy: Some(*strategy),
        },
        _ => return,
    };
    prefs::store_inflight_remote_op(&entry);
}

/// Settle an operation from the HTTP answer alone, for the paths that have no record on
/// the server to read.
fn settle_locally(
    core: RwSignal<OperationsCore>,
    graph: RwSignal<GraphCore>,
    key: &IdempotencyKey,
    message: String,
    ok: bool,
) {
    // `settle` is keyed by operation id, so an operation that never got one needs a
    // client-side handle. The idempotency key is already unique per user action.
    let Ok(id) = OperationId::new(key.as_str()) else {
        return;
    };
    if core
        .try_update(|c| c.bind_id(key, id.clone()))
        .is_none_or(|r| r.is_err())
    {
        return;
    }
    let outcome = Settlement {
        state: if ok {
            OperationState::Succeeded
        } else {
            OperationState::Failed
        },
        message: Some(message),
        generation: None,
    };
    commit_settlement(core, graph, &id, outcome);
}

/// Apply a settlement and act on the invalidation it publishes.
fn commit_settlement(
    core: RwSignal<OperationsCore>,
    graph: RwSignal<GraphCore>,
    id: &OperationId,
    outcome: Settlement,
) {
    let id = id.clone();
    let cleared_id = id.clone();
    let published = core
        .try_update(move |c| c.settle(&id, outcome).ok())
        .flatten();
    // #232, M2.20f: a settled Fetch/Pull is no longer "in flight to resume"
    // — clear the boot-time reconnect entry the moment it settles for
    // real. A no-op for every other kind (nothing was ever stored for
    // them) and for a replayed terminal event (`published` is `None`
    // then, exactly the case the comment below already names).
    if published.is_some() {
        prefs::clear_inflight_remote_op_if_matches(cleared_id.as_str());
    }
    // A replayed terminal event settles nothing and publishes nothing, so a reconnected
    // stream cannot run `on_invalidate` twice. When it DOES publish, `on_invalidate` is
    // the D3 payoff: a settlement carrying the generation the graph already has skips the
    // epoch bump entirely, instead of the old unconditional re-read after every write.
    if let Some(inv) = published {
        let _ = graph.try_update(|g| g.on_invalidate(&inv));
    }
}

/// Follow one operation's progress stream to its terminal event.
///
/// The protocol version rides in the **query string**, not a header: `EventSource` cannot
/// set request headers, which is exactly why ADR 0020 gave this one route a query-string
/// negotiation path (the server allows it for this path alone).
/// How many times a lost progress stream is re-established before the client
/// admits, out loud, that it does not know the outcome (#232 follow-up).
///
/// Bounded for a reason the review found the hard way. `on_error` used to just
/// `close()` the stream and leave the entry in flight — harmless while nothing
/// read that list, and *fatal* the moment `menu.rs`'s `remote_op_running` gate
/// started keying off it: one dropped tunnel and Fetch **and** Pull were
/// disabled for the rest of the session, with no way back short of a reload.
/// This deployment is an iPad over an SSH tunnel, so that is not an exotic
/// case, it is Tuesday.
const STREAM_REATTACH_MAX_ATTEMPTS: u32 = 6;

/// How long to wait before each re-attach. Six attempts at two seconds rides
/// out a blip without holding a stuck row on screen for a minute.
const STREAM_REATTACH_INTERVAL_MS: u64 = 2_000;

/// What a user is told when the re-attach budget runs out. Deliberately does
/// not claim the operation failed — it claims *we lost track of it*, which is
/// the only honest thing left to say, and points at the check that resolves it.
/// Same posture as `clone_poll_exhausted_message` takes for #278's poll.
const STREAM_LOST_MESSAGE: &str = "Lost contact with the server while this was running, and \
                                   couldn't get it back. It may well have finished — check the \
                                   graph before running it again, or you can end up doing it \
                                   twice.";

/// Re-establish a progress stream that dropped, or — once the budget is gone —
/// settle the entry honestly so it stops blocking the menu (#232 follow-up).
///
/// `GET /api/operations/{id}` first rather than blindly reopening the stream:
/// the operation may have finished *during* the outage, in which case there is
/// no stream left to join and the terminal record is the answer. That is the
/// same status-then-decide shape [`resume_inflight_remote_op`] uses on boot,
/// and `resume_decision` is the pure, host-tested half both share.
///
/// `budget` is what stops a permanently-dead tunnel from looping forever: it
/// rides *through* the re-subscription, so a stream that reconnects and dies
/// again resumes counting down rather than starting fresh.
fn reattach_after_stream_loss(
    core: RwSignal<OperationsCore>,
    graph: RwSignal<GraphCore>,
    id: OperationId,
    budget: u32,
) {
    spawn_local(async move {
        for _ in 0..STREAM_REATTACH_MAX_ATTEMPTS {
            api::sleep_ms(STREAM_REATTACH_INTERVAL_MS).await;
            // Still unreachable — that is not an outcome either, so keep trying
            // within budget rather than inventing a verdict.
            let Ok(status) = api::fetch_operation_status(&id).await else {
                continue;
            };
            let _ =
                core.try_update(|c| c.observe(&id, status.state, status.stage, status.progress));
            match resume_decision(status.state) {
                ResumeDecision::Settle => {
                    if let Some(outcome) = Settlement::from_terminal(
                        status.state,
                        status.message.clone(),
                        status.generation,
                    ) {
                        commit_settlement(core, graph, &id, outcome);
                    }
                    return;
                }
                ResumeDecision::Subscribe => {
                    subscribe(core, graph, id.clone(), budget);
                    return;
                }
            }
        }
        // Budget gone. The entry cannot stay in flight: `settle` is the only
        // thing that removes it, the menu gate reads that list, and a row that
        // can never leave it is a permanent lockout. So say what is true —
        // contact was lost, the outcome is unknown — and let the user act on it.
        commit_settlement(
            core,
            graph,
            &id,
            Settlement {
                state: OperationState::Failed,
                message: Some(STREAM_LOST_MESSAGE.to_string()),
                generation: None,
            },
        );
    });
}

fn subscribe(
    core: RwSignal<OperationsCore>,
    graph: RwSignal<GraphCore>,
    id: OperationId,
    reattach_budget: u32,
) {
    let url = format!(
        "/api/operations/{}/events?protocol={}",
        id.as_str(),
        PROTOCOL_VERSION
    );
    let Ok(source) = web_sys::EventSource::new(&url) else {
        return;
    };

    // `progress` carries the small `ProgressEvent`; `result` carries the full record. The
    // first event is always the current snapshot, so an operation that already finished
    // answers immediately with `result`.
    let on_progress =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let Some(text) = e.data().as_string() else {
                return;
            };
            let Ok(ev) = serde_json::from_str::<ProgressEvent>(&text) else {
                return;
            };
            let _ = core.try_update(|c| c.observe(&ev.id, ev.state, ev.stage, ev.progress));
        });

    let closing = source.clone();
    let on_result =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            // Close first: the server ends the stream after the terminal event, and the
            // client must not hold a connection open against `MAX_LIVE_STREAMS`.
            closing.close();
            let Some(text) = e.data().as_string() else {
                return;
            };
            let Ok(record) = serde_json::from_str::<OperationStatus>(&text) else {
                return;
            };
            let Some(outcome) =
                Settlement::from_terminal(record.state, record.message.clone(), record.generation)
            else {
                return;
            };
            commit_settlement(core, graph, &record.id, outcome);
        });

    // A transport failure is not an outcome — the operation may well have run. But
    // "not an outcome" cannot mean "nothing happens": this used to close the stream
    // and walk away, which left the entry in flight forever, and once `menu.rs`'s
    // `remote_op_running` gate started reading that list, one dropped tunnel disabled
    // Fetch and Pull for the whole session. So close the dead socket and hand off to
    // [`reattach_after_stream_loss`], which re-reads the record, rejoins the stream if
    // there is still one to join, and — only once its budget is spent — settles the
    // entry with the honest "we lost track of this" message rather than a guess.
    let on_error_source = source.clone();
    let reattach_id = id.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        on_error_source.close();
        if reattach_budget > 0 {
            reattach_after_stream_loss(core, graph, reattach_id.clone(), reattach_budget - 1);
        } else {
            // Budget exhausted upstream. Same reasoning as the exhausted arm in
            // `reattach_after_stream_loss`: the row must not be able to outlive
            // every recovery path, because the menu gate reads it.
            commit_settlement(
                core,
                graph,
                &reattach_id,
                Settlement {
                    state: OperationState::Failed,
                    message: Some(STREAM_LOST_MESSAGE.to_string()),
                    generation: None,
                },
            );
        }
    });

    let _ = source
        .add_event_listener_with_callback(PROGRESS_EVENT, on_progress.as_ref().unchecked_ref());
    let _ =
        source.add_event_listener_with_callback(RESULT_EVENT, on_result.as_ref().unchecked_ref());
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    // The listeners must outlive this function, and the stream they serve is bounded four
    // ways by the server (terminal close, 15 s heartbeat, 30-minute cap, MAX_LIVE_STREAMS).
    // One closure trio per user-confirmed write is a bounded cost, not a growing one.
    on_progress.forget();
    on_result.forget();
    on_error.forget();
}

/// Resume watching a Fetch/Pull that was still in flight when the tab
/// reloaded or was suspended and resumed (#232, M2.20f — the PWA-reload /
/// Safari-suspend-resume acceptance criterion). Called once, from
/// `App()`, via [`Operations::resume_from_storage`], immediately after
/// [`Operations::new`].
///
/// Reuses the exact primitives a fresh [`Operations::dispatch`] uses
/// rather than adding new core-mutating surface for this one path: `admit`
/// under a key synthesised from the operation id itself (`IdempotencyKey`
/// and `OperationId` share the same token-shape validator,
/// `require_token`), `bind_id` to attach the real handle, an `observe` of
/// the live status, and then either an immediate [`commit_settlement`]
/// (finished while the tab was away) or [`subscribe`] (still running) —
/// the choice made by [`resume_decision`], the pure, host-tested half of
/// this.
fn resume_inflight_remote_op(core: RwSignal<OperationsCore>, graph: RwSignal<GraphCore>) {
    let Some(entry) = prefs::load_inflight_remote_op() else {
        return;
    };
    let Ok(id) = OperationId::new(entry.id.as_str()) else {
        // Corrupt or foreign storage content — never trusted, never acted on.
        prefs::clear_inflight_remote_op_if_matches(entry.id.as_str());
        return;
    };
    let Some(kind) = remote_op_kind(&entry) else {
        prefs::clear_inflight_remote_op_if_matches(entry.id.as_str());
        return;
    };
    let Ok(key) = IdempotencyKey::new(id.as_str()) else {
        return;
    };
    spawn_local(async move {
        // A transport failure is not an outcome (the same rule `subscribe`'s own
        // `on_error` arm holds) — leave storage and core state untouched; the
        // next boot tries again.
        let Ok(status) = api::fetch_operation_status(&id).await else {
            return;
        };
        if core
            .try_update(|c| c.admit(key.clone(), kind.clone()))
            .is_none_or(|r| r.is_err())
        {
            return;
        }
        if core
            .try_update(|c| c.bind_id(&key, id.clone()))
            .is_none_or(|r| r.is_err())
        {
            return;
        }
        let _ = core.try_update(|c| c.observe(&id, status.state, status.stage, status.progress));
        match resume_decision(status.state) {
            ResumeDecision::Settle => {
                if let Some(outcome) = Settlement::from_terminal(
                    status.state,
                    status.message.clone(),
                    status.generation,
                ) {
                    commit_settlement(core, graph, &id, outcome);
                }
            }
            ResumeDecision::Subscribe => subscribe(core, graph, id, STREAM_REATTACH_MAX_ATTEMPTS),
        }
    });
}
