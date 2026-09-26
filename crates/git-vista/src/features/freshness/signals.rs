// **Signed:** codex · 2026-09-26T12:39:32-04:00
// last_edited_by: codex
//! The change feed's client half — wasm only (M12.05, #555).
//!
//! One `EventSource` on `GET /api/repository/events`, one log of what it
//! published, and the live-graph follow-up that log drives (#852). Every
//! *decision* — whether the plan on screen is still current, whether a
//! snapshot should check history, whether two history-v1 Frames disagree —
//! lives in [`super::core`], host-tested, because `cargo test` never
//! compiles this file.
//!
//! # Why the protocol version rides in the query string
//!
//! `EventSource` cannot set request headers, so it physically cannot send
//! `PROTOCOL_HEADER`. ADR 0020 gave the operations stream a query-string
//! negotiation path for exactly this reason and the server allows it for that
//! path alone; #555 adds the second, matched exactly rather than by a wildcard.
//!
//! # What a dropped stream means, and why the log is cleared
//!
//! A client that misses snapshots cannot difference across the gap: the refs
//! that moved while it was away were never delivered. So a drop clears the log,
//! and every plan on screen falls back to the answer that claims least. The
//! alternative — carrying on as though the span were continuous — would let the
//! reassuring sentence be printed over changes nobody saw.

use leptos::*;
use wasm_bindgen::prelude::*;

use git_vista_protocol::change_feed::{ChangeFeedHealth, ChangeFeedSnapshot};
use git_vista_protocol::{GenerationToken, PROTOCOL_QUERY, PROTOCOL_VERSION};

use crate::features::graph::core::GraphCore;
use crate::features::status::signals::StatusResource;

use super::core::{
    verdict, BoundFeed, FeedTicket, HistoryCheck, HistoryFollower, HistoryProbe, PlanSlot,
    PlanVerdict,
};

/// How long to wait before re-opening a change feed that dropped.
///
/// Unbounded retries, deliberately, and this is the one stream where that is
/// right: an operations stream that cannot be re-established has an outcome to
/// settle and a menu to unblock (#232's lockout), while this one has no state to
/// release — a client with no feed simply says "couldn't tell" until it has one.
/// Giving up permanently would make that sentence permanent too.
///
/// Reconnect always requests the same bound target. Session selection in another
/// tab neither ends this connection nor supplies a replacement target.
const REATTACH_INTERVAL_MS: u64 = 1_000;

/// The repository change feed, as the app holds it.
#[derive(Clone, Copy)]
pub struct Freshness {
    feed: RwSignal<BoundFeed>,
    history: RwSignal<HistoryFollower>,
}

impl Default for Freshness {
    fn default() -> Self {
        Self::new()
    }
}

impl Freshness {
    pub fn new() -> Self {
        Self {
            feed: create_rw_signal(BoundFeed::default()),
            history: create_rw_signal(HistoryFollower::default()),
        }
    }

    /// Open the stream. Called once, from `App`.
    pub fn connect(&self, graph: RwSignal<GraphCore>) {
        let feed = self.feed;
        let socket = store_value(None::<FeedSocket>);
        let binding = create_memo(move |_| graph.get().live_binding());
        create_effect(move |_| {
            let target = binding.get();
            let mut ticket = None;
            feed.update(|f| ticket = f.bind(target));
            if let Some(ticket) = ticket {
                socket.update_value(|s| *s = None);
                subscribe(feed, socket, graph, ticket);
            } else if binding.get_untracked().is_none() {
                socket.update_value(|s| *s = None);
            }
        });
        on_cleanup(move || {
            let _ = feed.try_update(|f| f.dispose());
            let _ = socket.try_update_value(|s| *s = None);
        });
    }

    /// A tracked read: the panel re-renders when a snapshot arrives.
    ///
    /// One fold, so the button, the notice and the Rebuild offer are three
    /// readings of one verdict rather than three computations that can drift.
    pub fn of(&self, slot: &PlanSlot) -> PlanVerdict {
        self.feed.with(|feed| verdict(slot, &feed.log))
    }

    /// The most recently published health, or `None` before the first
    /// snapshot (or just after a reconnect clears the log) — a tracked read,
    /// so #663's topbar affordance re-renders on every publication like the
    /// plan panel does. `Clone`d out rather than borrowed: the caller may
    /// hold this across the reactive scope that produced it.
    pub fn health(&self) -> Option<ChangeFeedHealth> {
        self.feed
            .with(|feed| feed.log.latest().map(|s| s.health.clone()))
    }

    /// The most recently recorded snapshot, or `None` before the first
    /// publication and again after a reconnect clears the log. Tracked, so
    /// the live-graph follow-up re-runs on every reading.
    pub fn latest_snapshot(&self) -> Option<ChangeFeedSnapshot> {
        self.feed.with(|feed| feed.log.latest().cloned())
    }

    /// Follow committed history from this feed (#852).
    ///
    /// The feed is a hint that *something* moved. The graph is pinned to
    /// history-v1, a different recipe from the planner token the feed
    /// carries, so this never compares those tokens. It refetches status
    /// on every reading, probes `GET /api/frame` when a Frame is already
    /// on screen, and remounts only when that Frame's generation moved.
    /// A snapshot that arrives while the seed is still loading sets a
    /// pending check; the Ready transition runs it so an in-flight first
    /// page cannot strand a stale graph.
    pub fn follow_graph(
        self,
        graph: RwSignal<GraphCore>,
        graph_ready: Signal<bool>,
        displayed: Signal<Option<GenerationToken>>,
        status: StatusResource,
    ) {
        let history = self.history;
        create_effect(move |_| {
            let snapshot = self.latest_snapshot();
            let source = self.feed.with(|f| f.ticket().cloned());
            let current = graph.get();
            let ready = graph_ready.get();
            let frame = displayed.get();
            // Timer writes wake this effect without another feed event.
            // Untracked writes avoid notifying ourselves while dispatching.
            self.history_check();
            let mut actions = None;
            history.update_untracked(|h| {
                actions = Some(h.update(
                    source.as_ref(),
                    snapshot.as_ref(),
                    &current,
                    ready,
                    frame.as_ref(),
                ));
            });
            let Some(actions) = actions else { return };
            if actions.refetch_status {
                status.refetch();
            }
            if let Some(probe) = actions.probe {
                run_history_probe(history, graph, graph_ready, displayed, probe);
            }
        });
    }

    /// Separate from server feed health: a client GET failure does not mean
    /// the server sweep is blind. Exhaustion is also reported to the console.
    pub fn history_check(&self) -> HistoryCheck {
        self.history.with(|h| h.check.clone())
    }
}

fn run_history_probe(
    history: RwSignal<HistoryFollower>,
    graph: RwSignal<GraphCore>,
    graph_ready: Signal<bool>,
    displayed: Signal<Option<GenerationToken>>,
    probe: HistoryProbe,
) {
    spawn_local(async move {
        let result = crate::api::fetch_frame_for_target(
            Some(&probe.binding.worktree),
            &crate::features::graph::core::HistoryView::Live,
        )
        .await
        .and_then(|frame| {
            frame
                .worktree_id
                .map(|id| (id, frame.generation))
                .ok_or_else(|| {
                    crate::api::HistoryFetchError::Decode(
                        "History probe returned no repository identity.".into(),
                    )
                })
        })
        .map_err(|error| error.to_string());
        let (Some(mut current), Some(ready), Some(frame)) = (
            graph.try_get_untracked(),
            graph_ready.try_get_untracked(),
            displayed.try_get_untracked(),
        ) else {
            return;
        };
        let before = current.clone();
        let mut retry = None;
        let _ = history.try_update(|h| {
            retry = h.complete(&probe, result, &mut current, ready, frame.as_ref());
            if let HistoryCheck::Failed { attempts, reason } = &h.check {
                leptos::logging::error!(
                    "History check failed after {attempts} attempts: {reason}. Refresh to retry."
                );
            }
        });
        if current != before {
            graph.set(current);
        }
        if let Some(delay_ms) = retry {
            crate::api::sleep_ms(delay_ms).await;
            if let Some(current) = graph.try_get_untracked() {
                let _ = history.try_update(|h| h.retry(&probe, &current));
            }
        }
    });
}

/// Own every JS callback until its source is detached. No forgotten closures.
struct FeedSocket {
    source: web_sys::EventSource,
    snapshot: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _error: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for FeedSocket {
    fn drop(&mut self) {
        self.source.close();
        self.source.set_onerror(None);
        let _ = self.source.remove_event_listener_with_callback(
            git_vista_protocol::change_feed::SNAPSHOT_EVENT,
            self.snapshot.as_ref().unchecked_ref(),
        );
    }
}

fn connection_failed(
    feed: RwSignal<BoundFeed>,
    socket: StoredValue<Option<FeedSocket>>,
    graph: RwSignal<GraphCore>,
    ticket: FeedTicket,
) {
    if graph
        .try_get_untracked()
        .and_then(|g| g.live_binding())
        .as_ref()
        != Some(&ticket.binding)
    {
        return;
    }
    if feed.try_update(|f| f.failed(&ticket)) != Some(true) {
        return;
    }
    // Retain the executing error closure until this callback returns. The
    // logically retired timer cannot reopen anything after binding/disposal.
    spawn_local(async move {
        crate::api::sleep_ms(REATTACH_INTERVAL_MS).await;
        if graph
            .try_get_untracked()
            .and_then(|g| g.live_binding())
            .as_ref()
            != Some(&ticket.binding)
        {
            return;
        }
        let Some(next) = feed.try_update(|f| f.reconnect(&ticket)).flatten() else {
            return;
        };
        let _ = socket.try_update_value(|s| *s = None);
        subscribe(feed, socket, graph, next);
    });
}

fn subscribe(
    feed: RwSignal<BoundFeed>,
    socket: StoredValue<Option<FeedSocket>>,
    graph: RwSignal<GraphCore>,
    ticket: FeedTicket,
) {
    let url = format!(
        "/api/repository/events?{PROTOCOL_QUERY}={PROTOCOL_VERSION}&repo={}",
        crate::api::encode_component(&ticket.binding.worktree)
    );
    let Ok(source) = web_sys::EventSource::new(&url) else {
        connection_failed(feed, socket, graph, ticket);
        return;
    };
    let reading_ticket = ticket.clone();
    let on_snapshot =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            if graph
                .try_get_untracked()
                .and_then(|g| g.live_binding())
                .as_ref()
                != Some(&reading_ticket.binding)
            {
                return;
            }
            let Some(text) = e.data().as_string() else {
                return;
            };
            let Ok(snapshot) = serde_json::from_str::<ChangeFeedSnapshot>(&text) else {
                return;
            };
            let _ = feed.try_update(|f| f.record(&reading_ticket, snapshot));
        });
    if source
        .add_event_listener_with_callback(
            git_vista_protocol::change_feed::SNAPSHOT_EVENT,
            on_snapshot.as_ref().unchecked_ref(),
        )
        .is_err()
    {
        source.close();
        connection_failed(feed, socket, graph, ticket);
        return;
    }
    let dropped = source.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        dropped.close();
        connection_failed(feed, socket, graph, ticket.clone());
    });
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    let _ = socket.try_update_value(|s| {
        *s = Some(FeedSocket {
            source,
            snapshot: on_snapshot,
            _error: on_error,
        })
    });
}
