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
    history_requires_reload, live_followup, verdict, FeedLog, LiveFollowup, PlanSlot, PlanVerdict,
};

/// How long to wait before re-opening a change feed that dropped.
///
/// Unbounded retries, deliberately, and this is the one stream where that is
/// right: an operations stream that cannot be re-established has an outcome to
/// settle and a menu to unblock (#232's lockout), while this one has no state to
/// release — a client with no feed simply says "couldn't tell" until it has one.
/// Giving up permanently would make that sentence permanent too.
///
/// A second rather than the operations stream's two, because the ordinary cause
/// of a clean close here is not a failure at all: the server ends the stream
/// when this session selects a different repository, and the reconnect is how
/// the feed follows it. A user who has just opened a repository should not
/// watch a panel say "couldn't tell" for longer than it takes to read it.
const REATTACH_INTERVAL_MS: u64 = 1_000;

/// The repository change feed, as the app holds it.
#[derive(Clone, Copy)]
pub struct Freshness {
    log: RwSignal<FeedLog>,
}

impl Default for Freshness {
    fn default() -> Self {
        Self::new()
    }
}

impl Freshness {
    pub fn new() -> Self {
        Self {
            log: create_rw_signal(FeedLog::new()),
        }
    }

    /// Open the stream. Called once, from `App`.
    pub fn connect(&self) {
        subscribe(self.log);
    }

    /// A tracked read: the panel re-renders when a snapshot arrives.
    ///
    /// One fold, so the button, the notice and the Rebuild offer are three
    /// readings of one verdict rather than three computations that can drift.
    pub fn of(&self, slot: &PlanSlot) -> PlanVerdict {
        self.log.with(|log| verdict(slot, log))
    }

    /// The most recently published health, or `None` before the first
    /// snapshot (or just after a reconnect clears the log) — a tracked read,
    /// so #663's topbar affordance re-renders on every publication like the
    /// plan panel does. `Clone`d out rather than borrowed: the caller may
    /// hold this across the reactive scope that produced it.
    pub fn health(&self) -> Option<ChangeFeedHealth> {
        self.log.with(|log| log.latest().map(|s| s.health.clone()))
    }

    /// The most recently recorded snapshot, or `None` before the first
    /// publication and again after a reconnect clears the log. Tracked, so
    /// the live-graph follow-up re-runs on every reading.
    pub fn latest_snapshot(&self) -> Option<ChangeFeedSnapshot> {
        self.log.with(|log| log.latest().cloned())
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
        let pending = create_rw_signal(false);
        let last_seq = create_rw_signal(None::<u64>);
        create_effect(move |_| {
            let Some(snapshot) = self.latest_snapshot() else {
                last_seq.set(None);
                return;
            };
            if last_seq.get_untracked() == Some(snapshot.seq) {
                return;
            }
            last_seq.set(Some(snapshot.seq));
            if live_followup(&snapshot) != LiveFollowup::CheckHistory {
                return;
            }
            if graph.get_untracked().view().is_historical() {
                return;
            }
            status.refetch();
            if graph_ready.get_untracked() && displayed.get_untracked().is_some() {
                probe_history(graph, displayed);
            } else {
                pending.set(true);
            }
        });
        create_effect(move |_| {
            if !graph_ready.get() || !pending.get() {
                return;
            }
            pending.set(false);
            if graph.get_untracked().view().is_historical() {
                return;
            }
            status.refetch();
            probe_history(graph, displayed);
        });
    }
}

fn probe_history(graph: RwSignal<GraphCore>, displayed: Signal<Option<GenerationToken>>) {
    if graph.get_untracked().view().is_historical() {
        return;
    }
    let epoch = graph.get_untracked().epoch();
    spawn_local(async move {
        let Ok(frame) = crate::api::fetch_frame().await else {
            return;
        };
        if graph.get_untracked().epoch() != epoch {
            return;
        }
        if graph.get_untracked().view().is_historical() {
            return;
        }
        if history_requires_reload(displayed.get_untracked().as_ref(), &frame.generation) {
            let _ = graph.try_update(|g| g.force_bump());
        }
    });
}

fn subscribe(log: RwSignal<FeedLog>) {
    let url = format!("/api/repository/events?{PROTOCOL_QUERY}={PROTOCOL_VERSION}");
    let Ok(source) = web_sys::EventSource::new(&url) else {
        return;
    };

    let on_snapshot =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let Some(text) = e.data().as_string() else {
                return;
            };
            let Ok(snapshot) = serde_json::from_str::<ChangeFeedSnapshot>(&text) else {
                return;
            };
            // `try_update`: this closure outlives nothing today, but a disposed
            // owner must drop the snapshot rather than panic inside a browser.
            let _ = log.try_update(|log| log.record(snapshot));
        });
    source
        .add_event_listener_with_callback(
            git_vista_protocol::change_feed::SNAPSHOT_EVENT,
            on_snapshot.as_ref().unchecked_ref(),
        )
        .ok();
    on_snapshot.forget();

    let dropped = source.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        dropped.close();
        // Everything this client saw is now on the far side of a gap.
        let _ = log.try_update(|log| log.clear());
        spawn_local(async move {
            crate::api::sleep_ms(REATTACH_INTERVAL_MS).await;
            subscribe(log);
        });
    });
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();
}
