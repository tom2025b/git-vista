//! `GET /api/operations/{id}` and `GET /api/operations/{id}/events` (M1.08, #61):
//! ask what happened to an operation, or watch one happen.
//!
//! These are the *recovery* half of the lifecycle work. A client whose response
//! was lost — the iPad's SSH tunnel dropping mid-push is the case this project
//! is built around — has the operation id from the
//! [`OPERATION_HEADER`](git_vista_protocol::OPERATION_HEADER) and can ask here
//! rather than inferring the outcome from the graph.
//!
//! Both routes are **reads of write outcomes**, so they are registered only on
//! the loopback router, alongside the writes they describe (ADR 0005): the LAN
//! listener never sees them exist. Authentication is the ordinary session gate
//! every other read carries.
//!
//! ## Why the stream takes its protocol version in the query string
//!
//! The browser's `EventSource` cannot set request headers, so a stream client
//! physically cannot send [`PROTOCOL_HEADER`](git_vista_protocol::PROTOCOL_HEADER).
//! Rather than exempt this route from negotiation, the contract layer accepts
//! [`PROTOCOL_QUERY`](git_vista_protocol::PROTOCOL_QUERY) *for this path only*
//! and range-checks it with the same `check_compatibility` as the header path.
//! Nothing else may use it: a version in a URL is cacheable and log-visible in a
//! way a header is not, so the exception stays as narrow as the browser
//! limitation that forced it.
//!
//! ## Why the stream is bounded four ways
//!
//! It closes at the terminal event, sends a keep-alive comment every
//! [`HEARTBEAT`] (so a dead peer is noticed and an intermediary doesn't time the
//! connection out), gives up after [`MAX_STREAM_LIFETIME`], and takes a permit
//! from a process-wide cap. A client that opens streams and walks away must not
//! be able to accumulate connections; the first three bound one stream, the
//! fourth bounds all of them.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;

use git_vista_protocol::{
    IdempotencyKey, OperationByKeyResponse, OperationId, OperationStatus, ProgressEvent,
    UnixSeconds, PROGRESS_EVENT, RESULT_EVENT,
};

use crate::operations;

/// How often the stream sends a keep-alive comment while nothing is happening.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// How long one stream may stay open. Well past any real git operation; a
/// stream still open after this is a client that stopped reading, and the
/// record remains available from the plain `GET` either way.
const MAX_STREAM_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// `GET /api/operations/{id}` — the recorded status of one operation.
///
/// Answers for a *running* operation too (state `running`, no result yet), so a
/// client that reconnects mid-flight learns that its push is still going rather
/// than that nothing is known.
pub(crate) async fn operation_status(Path(id): Path<String>) -> Response {
    let Some(record) = resolve(&id) else {
        return not_found();
    };
    Json(record.status()).into_response()
}

/// `GET /api/operations/{id}/events` — the operation's progress as server-sent
/// events, ending with the terminal record.
///
/// The first event is always the *current* snapshot, not the next change: a
/// client that subscribes late (or after the operation already finished) gets an
/// immediate answer instead of waiting for a transition that already happened.
pub(crate) async fn operation_events(Path(id): Path<String>) -> Response {
    let Some(record) = resolve(&id) else {
        return not_found();
    };
    let Some(permit) = operations::StreamPermit::acquire() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many progress streams are open. Close a tab and try again.",
        )
            .into_response();
    };

    let stream = async_stream::stream! {
        // Held for the stream's whole life, including when the client
        // disconnects — axum drops the response body, which drops this.
        let _permit = permit;
        let mut rx = record.subscribe();

        let (first, mut terminal) = {
            let snapshot = rx.borrow_and_update();
            (encode(&snapshot), snapshot.is_terminal())
        };
        yield Ok::<Event, Infallible>(first);

        let deadline = tokio::time::Instant::now() + MAX_STREAM_LIFETIME;
        while !terminal {
            let next = tokio::select! {
                changed = rx.changed() => match changed {
                    Ok(()) => {
                        // Clone out and drop the borrow before yielding: a watch
                        // `Ref` is not `Send` and must not be held across one.
                        let snapshot = rx.borrow_and_update();
                        terminal = snapshot.is_terminal();
                        Some(encode(&snapshot))
                    }
                    // The record was dropped, which eviction refuses to do
                    // while live. Close rather than spin.
                    Err(_) => None,
                },
                _ = tokio::time::sleep_until(deadline) => None,
            };
            match next {
                Some(event) => yield Ok::<Event, Infallible>(event),
                None => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT))
        .into_response()
}

/// `POST /api/operations/{id}/cancel` — ask a running operation to stop
/// (M2.20c, #229).
///
/// ## What this endpoint promises, and what it does not
///
/// It promises to **set the operation's cancellation latch**, which its
/// executor is watching, and to answer honestly about whether that could
/// possibly do anything. It does *not* promise the operation stops at a
/// particular point, and it never terminalises the record itself: only the
/// pipeline may do that, and only after it has observed what actually
/// happened to the repository. A cancel that lands one millisecond after
/// `git fetch` updated `refs/remotes/origin/main` must produce a terminal
/// record that says the ref moved — which is possible only if the pipeline,
/// not this handler, writes it.
///
/// Three refusals, each answering a different question the operator has:
///
/// * **404** — no such operation (or an id that isn't token-shaped). Same
///   answer for both, for the same reason the reads above give one: an id is
///   unguessable, and distinguishing them would say which ids exist.
/// * **409, "already finished"** — the record is terminal. Answering `202`
///   here would tell an operator their cancel took effect on an operation
///   that had already run to completion.
/// * **409, "cannot be cancelled"** — the operation's executor does not watch
///   the latch ([`planner::honours_cancellation`]). Setting it would be a
///   no-op dressed up as an action.
///
/// A repeated cancel of a still-running operation is `202` again: idempotent,
/// not an error — a client whose response was lost must be able to retry.
///
/// `202 Accepted` rather than `200`: the fetch's own outcome is a separate
/// record, and the client learns it from the stream or from
/// `GET /api/operations/{id}` exactly as it would have anyway.
pub(crate) async fn cancel_operation(Path(id): Path<String>) -> Response {
    let Some(record) = resolve(&id) else {
        return not_found();
    };
    let snapshot = record.status();
    if !crate::planner::honours_cancellation(&snapshot.operation) {
        return (
            StatusCode::CONFLICT,
            "This kind of operation cannot be cancelled — it does not run long \
             enough to have a cancellation point. Wait for it to finish.",
        )
            .into_response();
    }
    if !record.request_cancel() {
        return (
            StatusCode::CONFLICT,
            "This operation has already finished — read its recorded result \
             rather than cancelling it.",
        )
            .into_response();
    }
    (
        StatusCode::ACCEPTED,
        "Cancelling. The operation's own record will say what it managed to do \
         before it stopped.",
    )
        .into_response()
}

/// Look one record up by the raw path segment, validating its shape first: an
/// id that isn't token-shaped can't name a record this server minted, so it is
/// the same "no such operation" as one that was never issued.
fn resolve(id: &str) -> Option<std::sync::Arc<operations::Record>> {
    let id = OperationId::new(id).ok()?;
    operations::lookup(&id)
}

/// The same answer for an id that never existed and one that has been evicted.
/// Deliberately identical: an id is unguessable, so distinguishing the two would
/// only tell a caller which ids the server has ever minted.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        "No such operation — it may have finished long enough ago to be forgotten.",
    )
        .into_response()
}

/// One snapshot as an SSE event.
///
/// A terminal snapshot is sent as the full [`OperationStatus`] under the
/// `result` event name — everything the client needs to reconcile, so no
/// follow-up `GET` is required — while an in-flight one is the small
/// [`ProgressEvent`] under `progress`. Two names, so a client's handlers don't
/// have to inspect the payload to know which it got.
fn encode(snapshot: &OperationStatus) -> Event {
    if snapshot.is_terminal() {
        return Event::default()
            .event(RESULT_EVENT)
            .json_data(snapshot)
            .unwrap_or_else(|_| Event::default().event(RESULT_EVENT).data("{}"));
    }
    let progress = ProgressEvent {
        id: snapshot.id.clone(),
        state: snapshot.state,
        stage: snapshot.stage,
        at: UnixSeconds(crate::activity::now_secs()),
        // M2.20c (#229): the transfer report, when there is one. This is what
        // makes a fetch legible on the stream — `stage` sits at `Executing`
        // for its whole life and only this field moves.
        progress: snapshot.progress,
    };
    Event::default()
        .event(PROGRESS_EVENT)
        .json_data(&progress)
        .unwrap_or_else(|_| Event::default().event(PROGRESS_EVENT).data("{}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        CommitMessage, GitOperation, IdempotencyKey, OperationHash, RepositoryToken, WorktreeToken,
    };

    use crate::operations::Admission;

    fn admit(name: &str) -> (OperationId, crate::operations::OperationHandle) {
        let key = IdempotencyKey::new(format!("handler-{name}")).unwrap();
        let op = GitOperation::CommitOnHead {
            message: CommitMessage::new(name).unwrap(),
            allow_empty: true,
        };
        let hash = OperationHash::new("a".repeat(64)).unwrap();
        match operations::admit(
            &key,
            &op,
            &hash,
            RepositoryToken::new("test-repo").unwrap(),
            WorktreeToken::new("test-worktree").unwrap(),
        ) {
            Admission::Fresh(handle, record) => (record.id(), handle),
            _ => panic!("a fresh key must be admitted"),
        }
    }

    /// An id that never existed and one that has been forgotten answer the
    /// same, and neither leaks whether the server ever minted it.
    #[tokio::test]
    async fn an_unknown_operation_is_not_found() {
        let status = operation_status(Path("0123456789abcdef".to_string())).await;
        assert_eq!(status.status(), StatusCode::NOT_FOUND);

        let events = operation_events(Path("0123456789abcdef".to_string())).await;
        assert_eq!(events.status(), StatusCode::NOT_FOUND);
    }

    /// An id that isn't even token-shaped can't name a record this server
    /// minted, so it is refused as "no such operation" rather than reaching
    /// the map at all.
    #[tokio::test]
    async fn a_malformed_id_is_not_found_rather_than_an_error() {
        let response = operation_status(Path("not a token".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A live record is readable while it is still running — a reconnecting
    /// client learns its push is in flight, not that nothing is known.
    #[tokio::test]
    async fn a_running_operation_is_readable_before_it_finishes() {
        let (id, handle) = admit("still-running");
        let response = operation_status(Path(id.as_str().to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        handle.finish(StatusCode::OK, "done".into(), None);
    }

    /// The stream opens as `text/event-stream`, and its permit is released
    /// when the response (and so the body) is dropped.
    #[tokio::test]
    async fn a_stream_opens_as_server_sent_events_and_frees_its_permit() {
        let (id, handle) = admit("stream-opens");
        handle.finish(StatusCode::OK, "done".into(), None);

        let response = operation_events(Path(id.as_str().to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        drop(response);

        // The cap is process-wide; if the permit had leaked, exhausting it here
        // would leave the next test unable to open a stream at all.
        let permits: Vec<_> = std::iter::repeat_with(operations::StreamPermit::acquire)
            .take_while(Option::is_some)
            .collect();
        assert!(
            !permits.is_empty(),
            "the finished stream must have released its permit"
        );
    }

    /// The cap is hard: a client that opens and abandons streams cannot
    /// accumulate connections past it.
    #[tokio::test]
    async fn the_stream_cap_refuses_rather_than_growing() {
        let (id, handle) = admit("stream-cap");
        handle.finish(StatusCode::OK, "done".into(), None);

        let _held: Vec<_> = (0..crate::operations::MAX_LIVE_STREAMS)
            .map(|_| operations::StreamPermit::acquire().expect("under the cap"))
            .collect();

        let response = operation_events(Path(id.as_str().to_string())).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
