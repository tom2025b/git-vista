//! `GET /api/repository/events` — the repository change feed (M12.05, #555).
//!
//! One server-sent event stream per client, carrying
//! [`ChangeFeedSnapshot`](git_vista_protocol::ChangeFeedSnapshot): the live
//! repository generation, what moved since the last snapshot, and what the feed
//! itself is currently able to do.
//!
//! # The generation this serves is the planner's, and that is a decision
//!
//! Five generation recipes already ship in this server, and `staging.rs`
//! records in its own source what comparing one against another costs: it
//! "409s forever, never admits". This feed carries the **planner** recipe —
//! bare decimal, minted by `planner::live_reading` calling
//! `generation_token` — because that is the digest `enforce_fresh` compares
//! before admitting an execution, and the invariant a freshness panel rests on
//! is that **the panel may never be more optimistic than the execution gate**.
//! Any other namespace breaks that at the type level: the panel would say
//! "current" about a digest the gate does not speak.
//!
//! The price is stated rather than fixed: the planner recipe folds worktree
//! status and the history recipe does not, so a pure editor save moves this
//! generation and re-reads a graph that cannot have changed. That is an
//! over-read — the fail-safe direction — and it is why this route does not mint
//! a sixth recipe to avoid it.
//!
//! # Bounded four ways, copied deliberately from the operations stream
//!
//! A keep-alive comment every [`HEARTBEAT`], a hard [`MAX_STREAM_LIFETIME`], a
//! process-wide [`StreamPermit`](crate::operations::StreamPermit) cap, and a
//! feed that stops entirely when its last stream closes. A client that opens
//! streams and walks away must not be able to accumulate connections, and the
//! watcher behind an abandoned stream must not keep consuming a shared system
//! resource.
//!
//! # The stream ends when the session selects a different repository
//!
//! A feed is bound to one worktree, and the session can select another at any
//! moment. A stream that kept publishing the old repository's generation would
//! be answering a freshness question about a repository nobody is looking at,
//! *confidently* — which is worse than not answering. So the loop re-reads the
//! session's selection and closes when it has moved; the client reconnects,
//! discards the log it can no longer difference against, and starts again on
//! the new repository.
//!
//! # Registered with the reads
//!
//! This describes the repository, not a write outcome, so both listeners serve
//! it (ADR 0005). It carries no path, no plan and no operation id — the health
//! value's one path-shaped field is reduced to a git-dir-relative label before
//! it reaches this module (`reconciliation::watch_label`).

use std::convert::Infallible;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};

use git_vista_protocol::change_feed::{ChangeFeedSnapshot, SNAPSHOT_EVENT};

use crate::operations;
use crate::reconciliation;

/// How often the stream sends a keep-alive comment while nothing is happening.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// How long one stream may stay open before the client is asked to reconnect.
///
/// Until it does, its freshness is `Unknown` and the panel says so — which is
/// the honest reading of "this client has not been told anything for a while",
/// and the reason the deadline is safe to have at all.
const MAX_STREAM_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// How often a quiet stream wakes to check whether the session has selected a
/// different repository.
///
/// A poll rather than a notification, deliberately: the selection is a shared
/// cell any request can write, and threading a change signal from every write
/// site to every open stream would be a second mechanism to keep correct. One
/// lock read per second per stream, against a hard cap of 32 streams, is not a
/// cost worth a mechanism.
const SELECTION_CHECK: Duration = Duration::from_secs(1);

/// `GET /api/repository/events` — the change feed for the current selection.
///
/// The first event is the *current* snapshot rather than the next change, so a
/// client that connects late — or reconnects after a suspension — gets an
/// immediate answer instead of sitting at "couldn't tell" until something
/// moves.
pub(crate) async fn repository_events() -> Response {
    let (repo, _) = crate::state::current();
    let Some(permit) = operations::StreamPermit::acquire() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many live streams are open. Close a tab and try again.",
        )
            .into_response();
    };
    // Holding this `Arc` for the stream's whole life is what keeps the feed
    // running; dropping it when the last stream closes is what stops the
    // watcher and releases its inotify watches.
    let feed = reconciliation::attach(&repo);
    let mut snapshots = feed.subscribe();
    // Captured here, in the handler, while the request's selection scope is
    // still on the task. The stream body below is polled by the response
    // writer, outside that scope — a capture attempted down there would find
    // nothing and silently pin the launch selection.
    let selection = crate::state::SelectionReader::capture();

    let stream = async_stream::stream! {
        let _permit = permit;
        let _feed = feed;
        let deadline = tokio::time::Instant::now() + MAX_STREAM_LIFETIME;
        let mut sent: Option<ChangeFeedSnapshot> = None;
        loop {
            if selection.path().as_deref() != Some(repo.as_path()) {
                break;
            }
            let current = snapshots.borrow_and_update().clone();
            // Only what the client has not already been sent. The loop also
            // wakes on a timer to re-check the selection, and re-sending the
            // same snapshot every few seconds would make a client's log full of
            // readings nothing took.
            if current.is_some() && current != sent {
                let Some(snapshot) = current.clone() else {
                    break;
                };
                match Event::default().event(SNAPSHOT_EVENT).json_data(&snapshot) {
                    Ok(event) => {
                        sent = current;
                        yield Ok::<Event, Infallible>(event);
                    }
                    // A snapshot that will not serialise is this server's own
                    // defect, and sending "{}" would render as a feed with no
                    // health at all. Close instead: a closed stream reconnects
                    // and says "couldn't tell" in the meantime, which is true.
                    Err(_) => break,
                }
            }
            let alive = tokio::select! {
                changed = snapshots.changed() => changed.is_ok(),
                // The selection heartbeat. A session can select a different
                // repository while this feed is perfectly quiet, and a quiet
                // feed is exactly the case where nothing else would wake this
                // loop to notice.
                () = tokio::time::sleep(SELECTION_CHECK) => true,
                _ = tokio::time::sleep_until(deadline) => false,
            };
            if !alive {
                break;
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT))
        .into_response()
}
