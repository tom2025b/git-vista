//! The reactive wrapper over the operations core — wasm only (M1.11, #64).
//!
//! Everything decidable is decided in [`super::core`], on the host, under test. This file
//! holds only what genuinely needs Leptos: reading the live epoch out of a signal, and
//! keeping the pending intent in a `StoredValue` that survives the closures that write it.

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
use crate::features::graph::core::GraphCore;
use crate::features::operations::core::{
    escalation, latest_wins, IntentSeq, OperationsCore, PendingIntent, Settled, Settlement,
};
use crate::features::operations::kind::OperationKind;

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
}

impl Operations {
    pub fn new(core: RwSignal<OperationsCore>, graph: RwSignal<GraphCore>) -> Self {
        Self {
            core,
            graph,
            intent_seq: store_value(IntentSeq::default()),
            pending_intent: store_value(None::<PendingIntent>),
        }
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
        let sent = kind.clone();
        spawn_local(async move {
            match send(&sent, key.clone()).await {
                // The request never went out (Visualize mode, or a body that would not
                // serialize). Settle it locally so the refusal is visible state rather
                // than a silently dropped intent.
                Err(reason) => {
                    settle_locally(core, graph, &key, reason, false);
                }
                Ok(receipt) => {
                    match receipt.operation.clone() {
                        // Operation-tracked: bind the server's handle, then read the
                        // record off the stream. The stream is what carries the
                        // post-execution generation, which the write response does not.
                        Some(id) => {
                            if core
                                .try_update(|c| c.bind_id(&key, id.clone()))
                                .is_some_and(|r| r.is_ok())
                            {
                                subscribe(core, graph, id);
                            }
                        }
                        // Not operation-tracked (`select`, `rescan`, `clone`,
                        // `delete-clone` never reach the server's planner). The HTTP
                        // answer is the whole outcome.
                        None => {
                            settle_locally(core, graph, &key, receipt.message, receipt.ok);
                        }
                    }
                }
            }
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
    let published = core
        .try_update(move |c| c.settle(&id, outcome).ok())
        .flatten();
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
fn subscribe(core: RwSignal<OperationsCore>, graph: RwSignal<GraphCore>, id: OperationId) {
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
            let _ = core.try_update(|c| c.observe(&ev.id, ev.state, ev.stage));
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

    // A transport failure is not an outcome — the operation may well have run. Close the
    // stream and leave the entry in flight; `GET /api/operations/{id}` remains the way to
    // reconcile, and the next repository read will show what actually happened.
    let on_error_source = source.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        on_error_source.close();
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
