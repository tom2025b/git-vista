//! The change feed's client half — wasm only (M12.05, #555).
//!
//! One `EventSource` on `GET /api/repository/events`, one log of what it
//! published, and nothing else. Every *decision* — whether the plan on screen is
//! still current, what to say about it, whether the confirm control may be
//! offered — lives in [`super::core`], host-tested, because `cargo test` never
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

use git_vista_protocol::change_feed::ChangeFeedSnapshot;
use git_vista_protocol::{PROTOCOL_QUERY, PROTOCOL_VERSION};

use super::core::{freshness, FeedLog, PlanFreshness, PlanOnScreen};

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
    pub fn of(&self, plan: &PlanOnScreen) -> PlanFreshness {
        self.log.with(|log| freshness(plan, log))
    }
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
