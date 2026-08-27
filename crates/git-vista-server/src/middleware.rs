//! The versioned-API-contract middleware (M1.02, #102).
//!
//! One `tower` layer wraps every `/api/*` route and owns the whole transport
//! contract, so no individual handler has to:
//!
//! 1. **Request id** — every call gets a process-unique id, echoed in the
//!    `x-request-id` response header and inside any error, for log correlation.
//! 2. **Protocol negotiation** — every path *except* `/api/protocol` must carry
//!    the [`PROTOCOL_HEADER`] naming the client's protocol version; a missing,
//!    unparseable, or out-of-window value is refused with the structured
//!    [`ApiError`] envelope so the frontend can raise its "Update Required" screen.
//! 3. **Consistent errors** — any error a handler returned as a bare status +
//!    text (and the 500 a caught panic produces) is rewrapped into that same
//!    envelope, so the *whole* surface answers failures in one shape.
//! 4. **Contract headers** — the protocol version and request id are stamped onto
//!    every response, success or error.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::{BodyExt, StreamBody};

use git_vista_protocol::{
    check_compatibility, parse_protocol_header, ApiError, ErrorCode, IdempotencyKey, OperationId,
    RequestId, IDEMPOTENCY_HEADER, MAX_CLIENT_PROTOCOL, MIN_CLIENT_PROTOCOL, OPERATION_HEADER,
    PROTOCOL_HEADER, PROTOCOL_QUERY, PROTOCOL_VERSION, REQUEST_ID_HEADER,
};

/// The one path exempt from the protocol-header requirement: a client hits it
/// precisely to *learn* the protocol, so it cannot be required to already speak it.
const NEGOTIATION_PATH: &str = "/api/protocol";

/// Upper bound on how much of an error response body we hold in memory at once
/// to classify and rewrap it. Error messages are git stderr / short strings, so
/// a real refusal is far below this; the cap is what stops a pathological body
/// from being buffered whole.
///
/// It bounds *buffering*, not typed DTO delivery: an over-cap JSON object is
/// forwarded without ever being held whole (#336). For a plain-text refusal,
/// unread data is deliberately omitted from the bounded envelope, which says
/// it was truncated; later transport errors and trailers are still forwarded.
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

/// Mint a process-unique request id. A monotonic counter is enough to tie a
/// client-reported id to this run's log line; it needs no randomness (so nothing
/// like `getrandom` on the pure-crate side) and never blocks.
fn next_request_id() -> RequestId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    RequestId::new(format!("{n:016x}"))
}

/// The middleware wrapping every `/api/*` route — see the module docs for the
/// four things it guarantees.
pub(crate) async fn api_contract(request: Request, next: Next) -> Response {
    let request_id = next_request_id();
    let is_negotiation = request.uri().path() == NEGOTIATION_PATH;

    // Read the client's protocol header before `next` consumes the request.
    let gate = if is_negotiation {
        Ok(())
    } else {
        check_protocol(&request)
    };

    let response = match gate {
        Err((code, message)) => error_envelope(code, message, &request_id),
        Ok(()) => {
            let response = next.run(request).await;
            if response.status().is_client_error() || response.status().is_server_error() {
                rewrap_error(response, &request_id).await
            } else {
                relabel_json_success(response).await
            }
        }
    };

    with_contract_headers(response, &request_id)
}

/// Validate the inbound protocol header, returning the error code + message to
/// send when it's absent, malformed, or outside the accepted `[min, max]` window.
fn check_protocol(request: &Request) -> Result<(), (ErrorCode, String)> {
    let raw = match request.headers().get(PROTOCOL_HEADER) {
        Some(raw) => raw.to_str().map_err(|_| {
            (
                ErrorCode::InvalidProtocolHeader,
                format!("The {PROTOCOL_HEADER} header isn't valid text."),
            )
        })?,
        // The documented exception (M1.08): a progress stream is opened by
        // `EventSource`, which cannot set request headers at all, so that one
        // path may carry its version in the query string instead. Same parse,
        // same window check, same refusal — only the place it's read differs.
        None if accepts_protocol_query(request.uri().path()) => &protocol_query_value(request)
            .ok_or_else(|| {
                (
                    ErrorCode::MissingProtocolHeader,
                    format!(
                        "This stream needs a ?{PROTOCOL_QUERY}= parameter naming the \
                         protocol version. Reload the app to update."
                    ),
                )
            })?,
        None => {
            return Err((
                ErrorCode::MissingProtocolHeader,
                format!(
                    "This request needs the {PROTOCOL_HEADER} header. Reload the app to update."
                ),
            ))
        }
    };
    let client = parse_protocol_header(raw).ok_or_else(|| {
        (
            ErrorCode::InvalidProtocolHeader,
            format!("The {PROTOCOL_HEADER} header '{raw}' isn't a protocol version number."),
        )
    })?;
    if check_compatibility(client, MIN_CLIENT_PROTOCOL, MAX_CLIENT_PROTOCOL).is_compatible() {
        Ok(())
    } else {
        Err((
            ErrorCode::ProtocolIncompatible,
            format!(
                "This app speaks protocol v{client}, but the server accepts \
                 v{MIN_CLIENT_PROTOCOL}–v{MAX_CLIENT_PROTOCOL} (currently v{PROTOCOL_VERSION}). \
                 Reload the app to update."
            ),
        ))
    }
}

/// Whether `path` is the one route allowed to name its protocol version in the
/// query string — `GET /api/operations/{id}/events`, the SSE stream.
///
/// Matched structurally rather than by a wildcard so the exception cannot widen
/// by accident: a new `/api/operations/...` route does not inherit it.
fn accepts_protocol_query(path: &str) -> bool {
    path.starts_with("/api/operations/")
        && path.ends_with("/events")
        && path.matches('/').count() == 4
}

/// The `protocol=` query parameter's value, if the request carries one.
/// A hand-rolled scan rather than a query-string parser: one parameter, read in
/// one place, and nothing here is worth a dependency.
fn protocol_query_value(request: &Request) -> Option<String> {
    request.uri().query().and_then(|query| {
        query.split('&').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name == PROTOCOL_QUERY).then(|| value.to_string())
        })
    })
}

/// Put the client's idempotency key in scope for the request, and stamp the
/// operation id the planner minted onto the response (M1.08, #61).
///
/// This layer only *carries* the key. Whether a request needs one is decided at
/// the planner — the single place a mutation can begin — because a route list
/// here would drift the first time someone adds an endpoint, and the chokepoint
/// cannot. A malformed key is refused here, though: it is a wire error, and the
/// planner should never see a value that failed validation.
pub(crate) async fn idempotency(request: Request, next: Next) -> Response {
    let raw = request
        .headers()
        .get(IDEMPOTENCY_HEADER)
        .map(|value| value.to_str().unwrap_or_default().to_string());

    let Some(raw) = raw else {
        return next.run(request).await;
    };
    let key = match IdempotencyKey::new(raw) {
        Ok(key) => key,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("The {IDEMPOTENCY_HEADER} header isn't usable: {e}."),
            )
                .into_response()
        }
    };

    let minted: Arc<Mutex<Option<OperationId>>> = Arc::new(Mutex::new(None));
    let mut response =
        crate::operations::with_key(key, Arc::clone(&minted), next.run(request)).await;

    // The id exists only if this request actually reached the planner — a read,
    // or a write refused before admission, mints nothing and stamps nothing.
    let minted = minted.lock().ok().and_then(|slot| slot.clone());
    if let Some(id) = minted {
        if let Ok(value) = HeaderValue::from_str(id.as_str()) {
            response.headers_mut().insert(OPERATION_HEADER, value);
        }
    }
    response
}

/// Build a structured error response, its HTTP status taken from the code.
fn error_envelope(code: ErrorCode, message: String, request_id: &RequestId) -> Response {
    let status =
        StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(ApiError::new(code, message, request_id.clone())),
    )
        .into_response()
}

/// Rewrap a handler's plain-text error response into the [`ApiError`] envelope. A
/// response already carrying JSON (an envelope we produced, or a handler that
/// opted in) is passed through untouched, so this never double-wraps.
///
/// # Why the body is *split*, not collected
///
/// #323's sniff has to look at the bytes (see the long comment below for why
/// the content-type header cannot answer the question). The obvious way to get
/// them — `to_bytes(body, MAX_ERROR_BODY)` — is all-or-nothing: it returns
/// `Err` for a body one byte over the cap and hands back *nothing*, so an
/// oversized refusal used to arrive at the client as an empty envelope
/// carrying the status's canonical reason ("Bad Request") and none of what the
/// server actually said. That is reachable: `git`'s stderr is captured through
/// `git_cmd::git_output_bounded`'s plain `cmd.output()`, which applies no size
/// cap at all, so a `pre-commit` hook printing a megabyte of rejection text
/// becomes a megabyte of `message` inside a typed DTO. JSON escaping only
/// widens the gap — one control byte of hook output becomes six bytes of
/// `\uXXXX` — so a body can clear 64 KiB from well under it.
///
/// So this reads a **bounded prefix** and keeps the unread remainder as a body
/// to forward rather than collecting it. Two outcomes:
///
/// * the data ended inside the cap — the common case, and the classification
///   is exact: the full bytes either parse as a JSON object or they don't;
/// * the body ran past the cap — nothing beyond the prefix is ever held in
///   memory. A prefix that is the *beginning* of a JSON object (see
///   [`incomplete_json_object_prefix`]) gets the `application/json` label and
///   the whole body is streamed on untouched, which is exactly what the client
///   needs to parse its typed DTO; anything else is prose, and the prefix is
///   enveloped with an explicit truncation marker. Unread data is omitted,
///   while any later transport error or trailers frames remain observable.
///
/// The memory bound is therefore [`MAX_ERROR_BODY`] plus the one frame that
/// crossed it — never the body — whatever a hook decides to print.
async fn rewrap_error(response: Response, request_id: &RequestId) -> Response {
    if is_json(&response) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let status = parts.status;
    // Read off before `split_at_limit` consumes `body`: splitting and
    // rejoining never change the byte count, so whatever the pre-split body
    // already knew about its own length is still true of the rejoined one.
    let original_exact = body.size_hint().exact();
    let (head, remainder) = split_at_limit(body, MAX_ERROR_BODY).await;
    let (rest, complete, overflow) = match remainder {
        BodyRemainder::End => (None, true, false),
        BodyRemainder::Overflow(rest) => (Some(rest), false, true),
        // Trailers end the data portion cleanly. Keep them on the body, but
        // classify the complete data prefix rather than treating metadata as
        // a transport failure.
        BodyRemainder::Trailers(rest) => (Some(rest), true, false),
        // There is no complete byte body to classify after a transport error
        // frame. Put the consumed prefix back and preserve the original
        // response headers and frame semantics.
        BodyRemainder::Interrupted(rest) => {
            return Response::from_parts(parts, rejoin(head, Some(rest), original_exact));
        }
    };

    // #323: a handler's own typed error DTO — `AmendCommitError`,
    // `CommitError`, `FetchError`, `PullError`, `SignTagError` — travels out of
    // `execute()` through the exact same `(StatusCode, String)` shape a
    // plain-text refusal uses, because that dispatcher's return type is shared
    // by ~30 operation kinds and can't vary per handler (see
    // `planner::execute`). Axum's blanket `impl IntoResponse for String` always
    // stamps `text/plain` (axum-core `into_response.rs`'s `Cow<'static, str>`
    // impl), so the content-type header the `is_json` check above relies on can
    // never distinguish a pre-serialized JSON body from an actual plain-text
    // message here — only the bytes can. Without this, a body like
    // `{"kind":"StaleTip","message":"…"}` was read as plain text and escaped
    // whole into *this* envelope's own `message` field: the client parses the
    // outer `ApiError` fine, then finds literal wire JSON where it expected to
    // parse `AmendCommitError` — exactly the never-show-raw-JSON regression
    // #316 already fixed once on the frontend. A JSON *object* is unambiguous
    // enough to trust as "already carrying JSON": git's own stderr/stdout text
    // and this server's plain refusal prose never happen to parse as one.
    let head_is_json = if !complete {
        incomplete_json_object_prefix(&head)
    } else {
        matches!(
            serde_json::from_slice::<serde_json::Value>(&head),
            Ok(serde_json::Value::Object(_))
        )
    };
    if head_is_json {
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return Response::from_parts(parts, rejoin(head, rest, original_exact));
    }

    let message = String::from_utf8_lossy(&head).trim().to_string();
    let message = if message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("Request failed")
            .to_string()
    } else {
        message
    };
    // Say so rather than handing the client a sentence that stops mid-word and
    // reads as the whole of what the server said.
    let message = if overflow {
        format!("{message} … (truncated at {MAX_ERROR_BODY} bytes)")
    } else {
        message
    };
    let code = ErrorCode::from_status(status.as_u16());
    let mut enveloped = (
        status,
        Json(ApiError::new(code, message, request_id.clone())),
    )
        .into_response();
    if let Some(rest) = rest {
        // An over-cap prose suffix is deliberately omitted from the bounded
        // envelope, but transport failures and trailers later in that suffix
        // are not data and remain part of the response semantics. For a clean
        // data prefix followed directly by trailers, retain the whole tail.
        let tail = if overflow {
            retain_non_data_frames(rest)
        } else {
            rest
        };
        let body = std::mem::replace(enveloped.body_mut(), Body::empty());
        *enveloped.body_mut() = append_body(body, tail);
    }
    enveloped
}

/// Read at most `limit` bytes off `body`, returning that prefix and — only when
/// the body had more to give — the *unread* remainder, ready to be forwarded.
///
/// [`BodyRemainder::Overflow`] is the signal "this body is longer than
/// `limit`"; it is not a second buffer. Nothing past the frame that crossed
/// the limit is ever polled here, so the peak allocation is `limit` plus that
/// one frame regardless of how much the handler had to say. A body that ends
/// exactly at `limit` reports [`BodyRemainder::End`]: the loop keeps reading
/// until the frame that overshoots, so
/// "exactly `limit` bytes" and "more than `limit` bytes" are distinguished by
/// the reader rather than guessed from the length (the same probe-byte
/// reasoning `git_cmd::read_to_cap` uses).
///
/// A transport error interrupts classification rather than masquerading as
/// clean EOF. Trailers, by contrast, cleanly finish the data portion and are
/// carried separately so the complete bytes can still be classified. In both
/// cases the original frame and unread body are retained for the caller.
enum BodyRemainder {
    End,
    Overflow(Body),
    Trailers(Body),
    Interrupted(Body),
}

/// Upper bound on how many frames [`split_at_limit`] will read while still
/// trying to classify a body against `limit` bytes.
///
/// The byte cap alone cannot bound the loop: a frame that carries zero data
/// bytes (an empty `Frame::data`, legal for any `http_body::Body` to yield,
/// and something a hand-rolled or adversarial stream can yield forever, each
/// one immediately `Ready`) never advances `head.len()`, so a byte-only exit
/// condition never fires. This budget is a second, independent exit: once
/// exhausted, the loop gives up on classification — precisely as though the
/// body were an oversized one — and forwards whatever remains **unread**,
/// rather than spinning on frames that will never cross the limit.
///
/// 4096 is picked to sit far above anything a real error body should ever
/// need: `MAX_ERROR_BODY` is 64 KiB, and even a body chunked unusually
/// finely — 16 bytes per frame — would still cross the byte limit in exactly
/// 4096 frames and return through the ordinary `Overflow` path well before
/// this budget is ever consulted. Only a run of frames that keep the loop
/// spinning *without* moving `head.len()` can actually exhaust it.
const MAX_SPLIT_FRAMES: usize = 4096;

async fn split_at_limit(mut body: Body, limit: usize) -> (Bytes, BodyRemainder) {
    let mut head: Vec<u8> = Vec::new();
    let mut frames_read: usize = 0;
    loop {
        if frames_read >= MAX_SPLIT_FRAMES {
            // Every frame read so far has already been folded into `head` (a
            // data frame's bytes appended) or has already caused an early
            // return in the branch below (trailers, a transport error, or a
            // byte-overflowing data frame) — nothing has been read-but-not-
            // yet-handled at this point, so there is no frame to carry
            // forward and `body` itself is exactly "everything unread".
            return (Bytes::from(head), BodyRemainder::Overflow(body));
        }
        let Some(frame) = body.frame().await else {
            return (Bytes::from(head), BodyRemainder::End);
        };
        frames_read += 1;
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                return (
                    Bytes::from(head),
                    BodyRemainder::Interrupted(prepend_frame(Err(error), body)),
                );
            }
        };
        let data = match frame.into_data() {
            Ok(data) => data,
            Err(frame) => {
                return (
                    Bytes::from(head),
                    BodyRemainder::Trailers(prepend_frame(Ok(frame), body)),
                );
            }
        };
        if head.len() + data.len() <= limit {
            head.extend_from_slice(&data);
            continue;
        }
        let room = limit - head.len();
        head.extend_from_slice(&data[..room]);
        let tail = data.slice(room..);
        return (
            Bytes::from(head),
            BodyRemainder::Overflow(prepend_frame(Ok(Frame::data(tail)), body)),
        );
    }
}

/// Put one already-polled frame back in front of a body without erasing later
/// errors or trailers. `Body::from_stream` accepts data only; `StreamBody` is
/// deliberately frame-level.
fn prepend_frame(first: Result<Frame<Bytes>, axum::Error>, mut rest: Body) -> Body {
    Body::new(StreamBody::new(async_stream::stream! {
        yield first;
        while let Some(frame) = rest.frame().await {
            yield frame;
        }
    }))
}

/// Wraps a body whose total byte count is already known, restoring that
/// length after it would otherwise be lost.
///
/// `prepend_frame` (which both `rejoin` and `split_at_limit` build on) routes
/// everything through `StreamBody::new(async_stream::stream! { .. })`, and an
/// async stream has no way to describe its own length up front — it reports
/// `SizeHint::default()` (lower `0`, upper `None`) regardless of what it will
/// actually yield. That is correct for a genuine stream, but wrong for a body
/// that started life with a known exact size and was merely split into two
/// pieces and stitched back together: no byte was added or dropped, so the
/// combined length is still known, and a caller further up (e.g. hyper
/// deciding whether to frame the response with `Content-Length` or fall back
/// to chunked transfer) deserves to be told that.
///
/// This delegates frame polling to the wrapped body untouched and overrides
/// only `size_hint`, so it changes nothing about what bytes are produced or
/// when — only what the body *claims* about its own length.
///
/// **`size_hint` describes what REMAINS, not what the body started with.**
/// That is `http_body::Body`'s contract, and the first version of this type
/// broke it: it stored the original total once and returned it forever, so
/// after a 7-byte body had handed over 6 bytes it still answered `Some(7)`
/// instead of `Some(1)`. Every read was correct at construction — which is
/// exactly why a check made before any frame was polled found nothing wrong.
/// `remaining` is therefore decremented as data frames go past.
struct KnownSizeBody {
    inner: Body,
    /// Bytes still to come, or `None` once the wrapped body has proven its
    /// own declared length was a lie.
    ///
    /// A body that yields MORE than it promised has invalidated the very
    /// claim this wrapper exists to carry forward. Saturating at zero would
    /// keep asserting an exact size — `Some(0)` — while more bytes were still
    /// arriving, which is the original defect in a new costume: a confident
    /// answer that is false. Going to `None` says the honest thing instead:
    /// the length was known, the body contradicted it, and it is not known
    /// any more.
    remaining: Option<u64>,
}

impl HttpBody for KnownSizeBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
        // Only DATA frames consume declared length. Trailers carry no bytes
        // the `Content-Length` this hint feeds would ever count, so a trailer
        // frame must leave `remaining` alone.
        if let std::task::Poll::Ready(Some(Ok(frame))) = &polled {
            if let Some(data) = frame.data_ref() {
                this.remaining = this
                    .remaining
                    .and_then(|left| left.checked_sub(data.len() as u64));
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        match self.remaining {
            Some(left) => SizeHint::with_exact(left),
            // Unknown, not zero: see `remaining`.
            None => SizeHint::default(),
        }
    }
}

/// Put a prefix back in front of the remainder it was split from, so the whole
/// body is forwarded byte-for-byte. `None` means there was no remainder and the
/// prefix *is* the body.
///
/// `original_exact` is the *pre-split* body's own `size_hint().exact()`, read
/// by the caller before it ever called `split_at_limit`. Splitting a body into
/// a bytes-in-hand prefix plus an unread remainder, then stitching the two
/// back together, does not change the total byte count — so when the original
/// length was known, the rejoined body still reports it, via
/// [`KnownSizeBody`], instead of silently acquiring the unknown-length default
/// the moment a remainder gets routed through `prepend_frame`. When the
/// original body never had a known length (a genuine stream), `None` here
/// leaves the rejoined body exactly as unknown-length as it already was.
fn rejoin(head: Bytes, rest: Option<Body>, original_exact: Option<u64>) -> Body {
    let joined = match rest {
        None => return Body::from(head),
        Some(rest) if head.is_empty() => rest,
        Some(rest) => prepend_frame(Ok(Frame::data(head)), rest),
    };
    match original_exact {
        Some(exact) => Body::new(KnownSizeBody {
            inner: joined,
            remaining: Some(exact),
        }),
        None => joined,
    }
}

/// Concatenate two frame-level bodies without converting either to a data
/// stream, so errors and trailers retain their original frame semantics.
fn append_body(mut first: Body, mut second: Body) -> Body {
    Body::new(StreamBody::new(async_stream::stream! {
        while let Some(frame) = first.frame().await {
            yield frame;
        }
        while let Some(frame) = second.frame().await {
            yield frame;
        }
    }))
}

/// Drop only unread data frames from an intentionally truncated prose suffix.
/// Errors and trailers are transport semantics, not message bytes, and must
/// still be observed by the client without requiring this layer to drain or
/// buffer the remainder before returning the response.
fn retain_non_data_frames(mut body: Body) -> Body {
    Body::new(StreamBody::new(async_stream::stream! {
        while let Some(frame) = body.frame().await {
            match frame {
                Err(error) => yield Err(error),
                Ok(frame) => {
                    if let Err(non_data) = frame.into_data() {
                        yield Ok(non_data);
                    }
                }
            }
        }
    }))
}

/// Whether `bytes` is the start of a JSON object that was cut off before the
/// closing brace.
///
/// The distinction `serde_json` makes is what carries this: a truncated object
/// fails with [`serde_json::Error::is_eof`] ("the input ended while more was
/// expected"), whereas prose that merely *starts* with `{` fails with a syntax
/// error at the first token that isn't JSON. Prose is refused on the first
/// character anyway — the leading-`{` check is what keeps an empty or
/// whitespace-only prefix (also an EOF error) from reading as JSON.
///
/// A *complete* object is deliberately false here: when unread bytes remain,
/// they may be trailing garbage, so a valid prefix cannot prove the whole body
/// is JSON.
///
/// Sniffed through `from_utf8_lossy` rather than the raw bytes because the cut
/// can land in the middle of a multi-byte character, which the byte parser
/// would report as a *syntax* error and this would then misread as prose; a
/// replacement character is still a legal JSON string character, so the lossy
/// copy fails at end-of-input the way the truncation actually did. The lossy
/// copy is only ever looked at — what gets forwarded is the original bytes.
fn incomplete_json_object_prefix(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    if text.trim_start().as_bytes().first() != Some(&b'{') {
        return false;
    }
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(_) => false,
        Err(e) => e.is_eof(),
    }
}

/// Label a *success* body that is hand-serialized JSON as `application/json`.
///
/// The same ambiguity `rewrap_error` resolves for errors exists on the success
/// side, and #336 is where it surfaced. Every write executor returns
/// `(StatusCode, String)` — a channel shared by ~30 operation kinds — and puts a
/// pre-serialized DTO in that `String`: `AmendCommitSuccess`, `FetchSuccess`,
/// `PullSuccess`. Axum's blanket `impl IntoResponse for String` stamps all of
/// them `text/plain`, so a 200 carrying a JSON object went out claiming to be
/// prose. Only `/api/amend-commit` escaped that, because
/// `handlers::commit::amend_route_response` sniffed *every* output of
/// `plan_and_execute` and not just the refusals — the second thing that layer
/// did, which the issue and the handoff both described as a refusal-only fix.
/// Deleting it without this would have silently un-labeled that route's 200 and
/// left the other four wrong; labeling it here fixes all five at once, in the
/// one place that already owns "the whole surface answers in one shape".
///
/// # Why a declared length is the gate
///
/// This runs on **every** non-error response, including the M1.08 progress
/// stream (`/api/operations/{id}/events`), which is an `async_stream` that stays
/// open for the life of an operation. Reading even one frame of it here would
/// stall the very thing it exists to deliver. So the gate is the body's own
/// [`http_body::Body::size_hint`]: a complete in-memory `String` reports an
/// exact size, a stream reports none. Only a body whose exact length is already
/// known, and fits [`MAX_ERROR_BODY`], is looked at — anything else is passed
/// through without being polled at all.
///
/// The `content-length` header is deliberately *not* what is consulted. It does
/// not exist yet this far up the stack — hyper writes it when it serializes the
/// response — so a gate on the header reads `None` for every route and relabels
/// nothing, which is how the first version of this function silently did that.
///
/// Unlike the error path this only ever *relabels*. A success body is the
/// endpoint's own payload; there is no envelope to put it in, and one that
/// isn't JSON is left exactly as the handler built it.
async fn relabel_json_success(response: Response) -> Response {
    if is_json(&response) {
        return response;
    }
    // The body's own exact size, not the `content-length` header: that header
    // does not exist yet at this point in the stack — hyper writes it when it
    // serializes the response — so reading it here would gate on `None` for
    // every route and relabel nothing.
    let exact = response.body().size_hint().exact();
    if exact.is_none_or(|len| len > MAX_ERROR_BODY as u64) {
        return response;
    }

    // `split_at_limit` rather than `to_bytes` even though the length says it
    // fits: a header that lies must not cost the body. Nothing is lost on any
    // path here — what is not relabeled is rejoined and passed on byte-for-byte.
    let (mut parts, body) = response.into_parts();
    let (head, remainder) = split_at_limit(body, MAX_ERROR_BODY).await;
    let (rest, complete) = match remainder {
        BodyRemainder::End => (None, true),
        BodyRemainder::Trailers(rest) => (Some(rest), true),
        BodyRemainder::Overflow(rest) | BodyRemainder::Interrupted(rest) => (Some(rest), false),
    };
    let is_json_object = complete
        && matches!(
            serde_json::from_slice::<serde_json::Value>(&head),
            Ok(serde_json::Value::Object(_))
        );
    if is_json_object {
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    // `exact` was already established above (the gate this function opens
    // on) to be `Some` and within `MAX_ERROR_BODY` before `split_at_limit`
    // ever ran, so it is exactly the "known original length" `rejoin` needs
    // to keep reporting after the prefix and remainder are stitched back
    // together.
    Response::from_parts(parts, rejoin(head, rest, exact))
}

/// Whether a response already carries a JSON content type.
fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

/// Stamp the protocol version and request id onto a response, so every reply —
/// success or error — is traceable and carries the negotiation datum.
fn with_contract_headers(mut response: Response, request_id: &RequestId) -> Response {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&PROTOCOL_VERSION.to_string()) {
        headers.insert(PROTOCOL_HEADER, value);
    }
    if let Ok(value) = HeaderValue::from_str(request_id.as_str()) {
        headers.insert(REQUEST_ID_HEADER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{HeaderMap, Request as HttpRequest},
        routing::{get, post},
        Router,
    };
    use git_vista_protocol::{AmendCommitError, AmendCommitSuccess, AmendFailureKind, ApiError};
    use http_body::Frame;
    use http_body_util::StreamBody;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tower::ServiceExt;

    // A tiny router carrying the contract layer over a few representative routes:
    // the exempt negotiation endpoint, a plain OK route, a route that returns a
    // handler-style `(StatusCode, String)` error, and the real `create_branch`
    // write handler (to exercise body rejection at the wire).
    /// Comfortably past [`MAX_ERROR_BODY`], so nothing below depends on the
    /// exact cap — only on being over it.
    const OVERSIZED_LEN: usize = MAX_ERROR_BODY + 8 * 1024;

    /// The first words of the oversized *prose* refusal, so a test can prove
    /// the client still receives what the server actually said rather than the
    /// status's canonical reason.
    const OVERSIZED_PROSE_OPENING: &str = "the pre-commit hook rejected this: ";

    fn oversized_prose() -> String {
        format!("{OVERSIZED_PROSE_OPENING}{}", "y".repeat(OVERSIZED_LEN))
    }

    /// A hook's rejection text, past the cap and full of the newlines and
    /// control bytes real hook output carries — the escaping is the point, since
    /// `\n` doubles and a control byte becomes six `\uXXXX` bytes, so the
    /// serialized DTO clears 64 KiB from a message already over it.
    fn oversized_hook_output() -> String {
        let unit = "policy check failed\n\tsee CONTRIBUTING.md\u{1}\n";
        unit.repeat(OVERSIZED_LEN / unit.len() + 1)
    }

    /// Exactly one complete JSON object in the sniffed prefix, followed by a
    /// byte that makes the whole body invalid JSON. The arithmetic is pinned
    /// so a cap change cannot quietly turn this into a different boundary.
    fn complete_json_prefix_with_trailing_garbage() -> String {
        let body = format!(r#"{{"m":"{}"}}X"#, "a".repeat(MAX_ERROR_BODY - 8));
        assert_eq!(
            body[..MAX_ERROR_BODY].len(),
            MAX_ERROR_BODY,
            "the sniffed prefix must be exactly the cap"
        );
        assert_eq!(
            body.len(),
            MAX_ERROR_BODY + 1,
            "the fixture must leave exactly one unread garbage byte"
        );
        body
    }

    fn app() -> Router {
        Router::new()
            .route("/api/protocol", get(|| async { "negotiation" }))
            .route("/api/ok", get(|| async { "ok-body" }))
            .route(
                "/api/boom",
                get(|| async { (StatusCode::NOT_FOUND, "No such commit.") }),
            )
            // Mimics `planner::amend_refusal`'s exact shape (#323): a handler
            // returning `(StatusCode, String)` where the `String` is already a
            // pre-serialized JSON DTO, produced the same way
            // `serde_json::to_string(&AmendCommitError { .. })` is. Axum's
            // blanket `String` impl stamps this `text/plain`, same as
            // `/api/boom` above — the two routes are otherwise identical at
            // the wire, and only the *body's own shape* tells them apart.
            .route(
                "/api/typed-refusal",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        serde_json::to_string(&AmendCommitError {
                            kind: AmendFailureKind::StaleTip,
                            message: "HEAD has moved since this amend was reviewed.".to_string(),
                        })
                        .unwrap(),
                    )
                }),
            )
            // The same typed-DTO shape as `/api/typed-refusal`, but with a
            // `message` past `MAX_ERROR_BODY` — the #336 edge. `git`'s stderr
            // is captured with no size cap (`git_cmd::git_output_bounded`'s
            // plain `cmd.output()`), so a rejecting hook that prints more than
            // 64 KiB puts more than 64 KiB into the DTO, and JSON escaping only
            // widens it further.
            .route(
                "/api/oversized-typed-refusal",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        serde_json::to_string(&AmendCommitError {
                            kind: AmendFailureKind::HookRejected,
                            message: oversized_hook_output(),
                        })
                        .unwrap(),
                    )
                }),
            )
            // The same size, but genuinely plain text: the other half of the
            // #336 edge, where the answer is to envelope what fits and say so
            // rather than to forward it.
            .route(
                "/api/oversized-prose",
                get(|| async { (StatusCode::BAD_REQUEST, oversized_prose()) }),
            )
            .route(
                "/api/oversized-prose-then-error",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::new(StreamBody::new(async_stream::stream! {
                            yield Ok::<Frame<Bytes>, std::io::Error>(Frame::data(
                                Bytes::from(oversized_prose())
                            ));
                            yield Err::<Frame<Bytes>, _>(std::io::Error::other(
                                "oversized body exploded"
                            ));
                        })))
                        .unwrap()
                }),
            )
            .route(
                "/api/oversized-prose-then-trailers",
                get(|| async {
                    let mut trailers = HeaderMap::new();
                    trailers.insert("x-overflow-proof", HeaderValue::from_static("kept"));
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::new(StreamBody::new(async_stream::stream! {
                            yield Ok::<Frame<Bytes>, std::convert::Infallible>(Frame::data(
                                Bytes::from(oversized_prose())
                            ));
                            yield Ok(Frame::trailers(trailers));
                        })))
                        .unwrap()
                }),
            )
            // The first MAX_ERROR_BODY bytes are a complete JSON object, but
            // the unread byte makes the full response invalid JSON. A complete
            // prefix cannot prove an over-cap body is JSON.
            .route(
                "/api/complete-json-prefix-with-trailing-garbage",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        complete_json_prefix_with_trailing_garbage(),
                    )
                }),
            )
            // Frame-level contract fixtures. Classification has to stop when
            // it encounters either transport failure or trailers, but the
            // consumed prefix and that frame must still reach the caller.
            .route(
                "/api/data-then-error",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::new(StreamBody::new(async_stream::stream! {
                            yield Ok::<Frame<Bytes>, std::io::Error>(Frame::data(
                                Bytes::from_static(b"{}")
                            ));
                            yield Err::<Frame<Bytes>, _>(std::io::Error::other(
                                "body exploded"
                            ));
                        })))
                        .unwrap()
                }),
            )
            .route(
                "/api/data-then-trailers",
                get(|| async {
                    let mut trailers = HeaderMap::new();
                    trailers.insert("x-proof", HeaderValue::from_static("kept"));
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::new(StreamBody::new(async_stream::stream! {
                            yield Ok::<Frame<Bytes>, std::convert::Infallible>(Frame::data(
                                Bytes::from_static(b"{}")
                            ));
                            yield Ok(Frame::trailers(trailers));
                        })))
                        .unwrap()
                }),
            )
            // A *success* in the shared write channel: a 200 whose `String` is
            // already a serialized DTO, exactly as `exec_amend_commit` and
            // `exec_fetch` build theirs. Axum stamps it `text/plain`; only the
            // bytes say otherwise.
            .route(
                "/api/typed-success",
                get(|| async {
                    (
                        StatusCode::OK,
                        serde_json::to_string(&AmendCommitSuccess {
                            message: "Amended commit.".to_string(),
                            old_tip: "1".repeat(40),
                            new_tip: Some("0".repeat(40)),
                            amended_published_commit: Some(false),
                        })
                        .unwrap(),
                    )
                }),
            )
            // A 200 that is genuinely prose, so the relabel cannot be "always".
            .route("/api/plain-success", get(|| async { "not json at all" }))
            // A 200 with no declared length — the shape the M1.08 progress
            // stream has. It must reach the client unread and unlabeled.
            .route(
                "/api/streamed-success",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        // A bare JSON object, deliberately: if the size-hint
                        // gate ever leaks, this body *would* be read and
                        // relabeled `application/json`, and the test below goes
                        // red. A fixture that could not be mistaken for JSON
                        // would pass whether the gate worked or not.
                        .body(Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>(Bytes::from_static(b"{\"progress\":1}"));
                        }))
                        .unwrap()
                }),
            )
            .route("/api/branch", post(crate::handlers::branch::create_branch))
            // The M1.08 stream route: the one path that may negotiate through
            // the query string, and the one whose id echoes the key in scope.
            .route(
                "/api/operations/{id}/events",
                get(|| async { "stream-would-start-here" }),
            )
            .route(
                "/api/operations/{id}",
                get(|| async {
                    crate::operations::current_key()
                        .map(|key| key.as_str().to_string())
                        .unwrap_or_else(|| "no-key".to_string())
                }),
            )
            .layer(axum::middleware::from_fn(idempotency))
            .layer(axum::middleware::from_fn(api_contract))
    }

    /// Read a whole test response. The cap is deliberately far above
    /// [`MAX_ERROR_BODY`] — these tests exist partly to prove what the
    /// middleware does with a body *larger* than its own buffering cap, so
    /// reading them back at that cap would collapse the very thing under test.
    const TEST_BODY_CAP: usize = 8 * 1024 * 1024;

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), TEST_BODY_CAP)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn get_req(path: &str, protocol: Option<&str>) -> HttpRequest<axum::body::Body> {
        let mut b = HttpRequest::get(path);
        if let Some(p) = protocol {
            b = b.header(PROTOCOL_HEADER, p);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn missing_protocol_header_is_refused_with_a_structured_envelope() {
        let resp = app().oneshot(get_req("/api/ok", None)).await.unwrap();
        assert_eq!(resp.status(), 426);
        // Every response — even a refusal — carries the contract headers.
        assert!(resp.headers().get(PROTOCOL_HEADER).is_some());
        assert!(resp.headers().get(REQUEST_ID_HEADER).is_some());
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(err.error.code, ErrorCode::MissingProtocolHeader);
        assert_eq!(err.protocol, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn a_compatible_client_passes_through_untouched() {
        let resp = app()
            .oneshot(get_req("/api/ok", Some(&PROTOCOL_VERSION.to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(body_string(resp).await, "ok-body");
    }

    #[tokio::test]
    async fn an_out_of_window_client_is_refused_as_incompatible() {
        let resp = app()
            .oneshot(get_req("/api/ok", Some("999999")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 426);
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(err.error.code, ErrorCode::ProtocolIncompatible);
    }

    #[tokio::test]
    async fn an_unparseable_header_is_refused_as_invalid() {
        let resp = app()
            .oneshot(get_req("/api/ok", Some("not-a-number")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 426);
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(err.error.code, ErrorCode::InvalidProtocolHeader);
    }

    #[tokio::test]
    async fn the_negotiation_endpoint_is_exempt_from_the_header() {
        // No protocol header, yet /api/protocol is served — and still gets the
        // contract headers so a client can read the request id.
        let resp = app().oneshot(get_req("/api/protocol", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(body_string(resp).await, "negotiation");
    }

    #[tokio::test]
    async fn a_plain_handler_error_is_rewrapped_into_the_envelope() {
        let resp = app()
            .oneshot(get_req("/api/boom", Some(&PROTOCOL_VERSION.to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(err.error.code, ErrorCode::NotFound);
        assert_eq!(err.error.message, "No such commit.");
    }

    /// #323 regression: a handler's own typed error DTO — the exact shape
    /// `planner::amend_refusal` produces — must reach the client as that DTO,
    /// not get read as plain text and escaped into the generic envelope's
    /// `message` field a second time. Both halves of the contract are checked
    /// deliberately: the shape alone (a) would still pass even if the
    /// content-type fix were missing, since `serde_json::from_str` doesn't
    /// look at headers; only (b) proves the double-encoding is actually gone.
    #[tokio::test]
    async fn a_handlers_typed_json_body_reaches_the_client_unescaped() {
        let resp = app()
            .oneshot(get_req(
                "/api/typed-refusal",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = body_string(resp).await;

        // (a) The client's own `classify_amend_response` parses this shape
        // directly — not as `ApiError` — so it must still deserialize as
        // `AmendCommitError` after passing through the contract layer.
        let parsed: AmendCommitError = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("body was not a bare AmendCommitError: {e}\nbody={body}"));
        assert_eq!(parsed.kind, AmendFailureKind::StaleTip);
        assert_eq!(
            parsed.message,
            "HEAD has moved since this amend was reviewed."
        );
        // Before the fix, this body was `{"code":...,"message":"{\"kind\":...","request_id":...}`
        // — still valid JSON, so parsing it as `AmendCommitError` would fail
        // loudly rather than silently, but let's also pin the un-escaped
        // shape directly: the raw wire text must never contain the outer
        // envelope's own field names doubled around the inner one.
        assert!(
            !body.contains("\\\"kind\\\""),
            "the inner DTO must not be escaped into an outer string field: {body}"
        );

        // (b) The content-type must say JSON — this is what lets `is_json`
        // recognise the body as already-JSON on any future pass through this
        // middleware, and it's the half a shape-only assertion can't catch.
        assert!(
            content_type.starts_with("application/json"),
            "expected an application/json content-type, got {content_type:?}"
        );
    }

    /// #336: the same typed DTO, past [`MAX_ERROR_BODY`], must still reach the
    /// client **whole**.
    ///
    /// This is the edge the amend route's own local relabeling layer used to
    /// cover alone, and the reason it could be collapsed into this one
    /// mechanism (ADR 0084). Before the fix, `to_bytes`' all-or-nothing collect
    /// returned `Err` for an over-cap body and `unwrap_or_default()` turned it
    /// into *no bytes at all*: the client received an `ApiError` envelope
    /// carrying the string "Bad Request" and none of what the hook said, on
    /// every route that hand-serializes its error DTO — `/api/commit`,
    /// `/api/fetch`, `/api/pull`, `/api/tag` included, none of which ever had a
    /// route-local layer to fall back on.
    ///
    /// The message is compared byte-for-byte, not merely "parsed": a fix that
    /// forwarded a *truncated* body would still fail to parse and would look
    /// like this test working, while a fix that forwarded only the prefix and
    /// closed the object for it would parse and be silently wrong.
    #[tokio::test]
    async fn an_oversized_typed_refusal_reaches_the_client_whole() {
        let resp = app()
            .oneshot(get_req(
                "/api/oversized-typed-refusal",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("application/json"),
            "an over-cap typed refusal must be labeled JSON like an under-cap \
             one — the client parses the DTO, not an envelope: got {content_type:?}"
        );
        let body = body_string(resp).await;
        assert!(
            body.len() > MAX_ERROR_BODY,
            "the fixture must actually exceed the cap or this test proves \
             nothing: serialized body was {} bytes",
            body.len()
        );
        let parsed: AmendCommitError = serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!(
                "an over-cap typed refusal did not survive as a bare \
                 AmendCommitError ({e}): {} bytes, starting {:?}",
                body.len(),
                &body[..body.len().min(120)]
            )
        });
        assert_eq!(parsed.kind, AmendFailureKind::HookRejected);
        assert_eq!(
            parsed.message,
            oversized_hook_output(),
            "the hook's own text must arrive byte-for-byte, not truncated at \
             the buffering cap"
        );
    }

    /// What a [`ScriptedBody`] does on its next poll.
    ///
    /// The first version of this fixture could only yield ready data and a
    /// trailer. That shape is exactly why six mutations survived an
    /// adversarial pass: a wrapper that mishandles `Pending`, an error frame,
    /// or a frame arriving *after* some state transition cannot be caught by a
    /// body that never produces one.
    enum Step {
        Data(&'static [u8]),
        /// Yields `Poll::Pending` exactly once, then continues to the next
        /// step. Consumes no bytes and supplies no evidence either way.
        PendingOnce,
        Error,
        Trailers,
    }

    /// A test body that plays a scripted sequence of frame shapes.
    ///
    /// It exists because `Body::from("abcdefg")` is `Full<Bytes>` and yields
    /// **exactly one** frame — asserted, not assumed, by
    /// `body_from_a_str_yields_exactly_one_frame` below. A drain loop over a
    /// one-frame body runs once: first frame *is* last frame, so it can only
    /// check two endpoints.
    struct ScriptedBody {
        steps: VecDeque<Step>,
        pending_fired: bool,
    }

    impl ScriptedBody {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                pending_fired: false,
            }
        }

        fn data(chunks: &[&'static [u8]]) -> Self {
            Self::new(chunks.iter().map(|c| Step::Data(c)).collect())
        }
    }

    impl HttpBody for ScriptedBody {
        type Data = Bytes;
        type Error = axum::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            let this = self.get_mut();
            // Loops so that a spent `PendingOnce` ADVANCES to the next step
            // rather than ending the stream. Getting this wrong made the very
            // first run of the Pending test fail on its own continuation — the
            // fixture, not the code under test.
            loop {
                match this.steps.front() {
                    None => return Poll::Ready(None),
                    Some(Step::PendingOnce) if !this.pending_fired => {
                        this.pending_fired = true;
                        // Deliberately does NOT register the waker: every test
                        // that drives this path polls it by hand.
                        return Poll::Pending;
                    }
                    Some(Step::PendingOnce) => {
                        // Already yielded its one Pending — drop it and carry
                        // on to whatever it was standing in front of.
                        this.steps.pop_front();
                        this.pending_fired = false;
                        continue;
                    }
                    Some(_) => {
                        return match this.steps.pop_front().expect("checked non-empty") {
                            Step::Data(d) => {
                                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(d)))))
                            }
                            Step::PendingOnce => unreachable!("handled above"),
                            Step::Error => Poll::Ready(Some(Err(axum::Error::new(
                                std::io::Error::other("scripted body failure"),
                            )))),
                            Step::Trailers => {
                                let mut trailers = HeaderMap::new();
                                trailers.insert("x-checksum", HeaderValue::from_static("ok"));
                                Poll::Ready(Some(Ok(Frame::trailers(trailers))))
                            }
                        };
                    }
                }
            }
        }
    }

    /// A body that records the waker supplied by its caller before returning
    /// `Pending`. Unlike [`ScriptedBody`], this exercises the liveness half of
    /// the `poll_frame` contract: forwarding `Pending` is insufficient if the
    /// wrapper substituted a waker that can never wake the real task.
    struct WakerProbeBody {
        captured: Arc<Mutex<Option<std::task::Waker>>>,
    }

    impl HttpBody for WakerProbeBody {
        type Data = Bytes;
        type Error = axum::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            *self
                .get_mut()
                .captured
                .lock()
                .expect("the waker probe lock is not poisoned") = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    struct CountingWake(Arc<std::sync::atomic::AtomicUsize>);

    impl std::task::Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Poll a body once, by hand, with a no-op waker.
    ///
    /// Needed because `Pending` is unreachable through `.frame().await` — the
    /// await simply suspends, and the test can never observe the state the
    /// wrapper is in at that moment. Mutation N3 (clearing the size hint on a
    /// single `Pending`) survived precisely because nothing could look here.
    fn poll_once(body: &mut KnownSizeBody) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        Pin::new(body).poll_frame(&mut cx)
    }

    /// The precondition the drain test depends on, asserted rather than
    /// believed: a `Body::from(&str)` really is a single frame. If a future
    /// axum/http-body ever splits it, this fails and tells you why the
    /// multi-frame fixture exists.
    #[tokio::test]
    async fn body_from_a_str_yields_exactly_one_frame() {
        let mut body = Body::from("abcdefg");
        let mut frames = 0;
        while let Some(f) = std::pin::Pin::new(&mut body).frame().await {
            f.expect("no errors");
            frames += 1;
        }
        assert_eq!(
            frames, 1,
            "Body::from(&str) is Full<Bytes> and yields one frame — the reason \
             `ScriptedBody` exists"
        );
    }

    /// `KnownSizeBody` must report what REMAINS, not what the body started
    /// with — `http_body::Body`'s actual contract, and the thing the first
    /// version of this wrapper got wrong.
    ///
    /// **Every expected value below is a LITERAL**, never `total - seen`.
    /// Computing the expectation with the same arithmetic the implementation
    /// uses is how an assertion quietly becomes `f(x) == f(x)`; the numbers
    /// here are worked out by hand from the fixture's own chunk sizes.
    ///
    /// Three frames, so the loop genuinely iterates: 3 + 2 + 2 = 7 bytes,
    /// giving four observation points — 7 before anything, then 4, 2, 0. A
    /// wrapper that held its total until the last frame (the mutation that
    /// survived the first version of this test) dies on the very first
    /// intermediate check.
    #[tokio::test]
    async fn a_known_size_body_reports_what_remains_not_what_it_started_with() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::data(&[b"abc", b"de", b"fg"])),
            remaining: Some(7),
        };

        let expected_after_each_frame = [Some(4u64), Some(2), Some(0)];

        assert_eq!(
            body.size_hint().exact(),
            Some(7),
            "before anything is polled, all 7 bytes still remain"
        );

        let mut seen_frames = 0usize;
        while let Some(frame) = std::pin::Pin::new(&mut body).frame().await {
            frame.expect("the fixture yields no errors");
            assert_eq!(
                body.size_hint().exact(),
                expected_after_each_frame[seen_frames],
                "after frame {} the remaining count is wrong",
                seen_frames + 1
            );
            seen_frames += 1;
        }
        assert_eq!(
            seen_frames, 3,
            "precondition: the fixture must really deliver three separate \
            frames, or this test degrades to the endpoint check it replaced"
        );
    }

    /// An empty DATA frame is still a frame, not an end-of-stream marker, and
    /// it consumes zero declared bytes. A wrapper that launders it into
    /// `Ready(None)` hides every later frame; one that merely invalidates the
    /// hint also invents evidence that the inner body never supplied.
    #[tokio::test]
    async fn an_empty_data_frame_is_forwarded_without_consuming_length() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::new(vec![
                Step::Data(b""),
                Step::Data(b"abc"),
            ])),
            remaining: Some(3),
        };

        let empty = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("an empty data frame is not EOF")
            .expect("no error")
            .into_data()
            .expect("the first frame is data");
        assert!(empty.is_empty(), "precondition: the first frame is empty");
        assert_eq!(
            body.size_hint().exact(),
            Some(3),
            "zero bytes consumed must leave all 3 bytes promised"
        );

        let data = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("data after an empty frame remains reachable")
            .expect("no error")
            .into_data()
            .expect("the second frame is data");
        assert_eq!(data, Bytes::from_static(b"abc"));
        assert_eq!(body.size_hint().exact(), Some(0));
    }

    /// The trailer invariant, which the production code asserts in a comment
    /// and — until now — no test held.
    ///
    /// Reachable in production: `BodyRemainder::Trailers(rest)` is rejoined
    /// with `original_exact` on the error path. A trailer carries no bytes the
    /// `Content-Length` this hint feeds would ever count, so it must leave
    /// `remaining` untouched.
    #[tokio::test]
    async fn a_trailer_frame_does_not_decrement_the_remaining_count() {
        let mut body = KnownSizeBody {
            // Leave two bytes deliberately outstanding before the trailer.
            // A zero-at-trailer fixture cannot distinguish "left unchanged"
            // from the wrong implementation "force to zero".
            inner: Body::new(ScriptedBody::new(vec![Step::Data(b"ab"), Step::Trailers])),
            remaining: Some(4),
        };

        let first = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("a data frame")
            .expect("no error");
        assert!(first.is_data(), "precondition: first frame carries data");
        assert_eq!(body.size_hint().exact(), Some(2), "2 data bytes consumed");

        let second = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("a trailer frame")
            .expect("no error");
        assert!(
            second.is_trailers(),
            "precondition: the fixture must actually yield a trailer, or this \
             test proves nothing about trailers"
        );
        assert_eq!(
            body.size_hint().exact(),
            Some(2),
            "a trailer carries no counted bytes, so it must not move the \
             nonzero remaining count"
        );
    }

    /// The companion case, and the reason `remaining` is an `Option` rather
    /// than a saturating counter: a wrapped body that yields MORE than its
    /// declared length has disproved the claim this wrapper carries.
    ///
    /// Asserts the full shape of the hint, not merely that `exact()` is
    /// `None`: `exact()` also returns `None` for any range with unequal
    /// bounds, so checking it alone would accept a hint that still asserted,
    /// say, "between 2 and 9 bytes". Unknown means lower 0, upper None.
    #[tokio::test]
    async fn a_body_that_overruns_its_declared_length_stops_claiming_one() {
        let mut body = KnownSizeBody {
            // THREE chunks, not two. The first crosses the declared boundary
            // directly (2 promised, 3 delivered), and the later frames prove
            // that invalidating the hint neither stops polling nor lets a
            // confident exact claim reappear.
            inner: Body::new(ScriptedBody::data(&[b"abc", b"de", b"f"])),
            remaining: Some(2),
        };

        let first = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the boundary-crossing frame remains")
            .expect("no error");
        assert_eq!(first.data_ref().map(Bytes::len), Some(3));
        let hint = body.size_hint();
        assert_eq!(
            hint.exact(),
            None,
            "a body that outran its own declared length must stop claiming an \
             exact size — an exact claim that is false is the defect this type \
             exists to avoid"
        );
        assert_eq!(hint.lower(), 0, "unknown means no lower bound is promised");
        assert_eq!(
            hint.upper(),
            None,
            "crossing the boundary inside one frame must invalidate the hint \
             immediately, never saturate at an exact zero"
        );

        let second = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the first post-invalidation frame remains")
            .expect("no error");
        assert_eq!(second.data_ref().map(Bytes::len), Some(2));
        let hint = body.size_hint();
        assert_eq!(hint.exact(), None, "unknown must stay unknown");
        assert_eq!(hint.lower(), 0, "unknown keeps a zero lower bound");
        assert_eq!(hint.upper(), None, "unknown keeps no upper bound");

        let third = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the second post-invalidation frame remains")
            .expect("no error");
        assert_eq!(third.data_ref().map(Bytes::len), Some(1));
        let hint = body.size_hint();
        assert_eq!(hint.exact(), None, "later data cannot restore exactness");
        assert_eq!(hint.lower(), 0, "unknown keeps a zero lower bound");
        assert_eq!(hint.upper(), None, "unknown keeps no upper bound");

        assert!(
            std::pin::Pin::new(&mut body).frame().await.is_none(),
            "precondition: the fixture delivered exactly three frames"
        );
    }

    /// Invalidating the byte-count hint does not invalidate non-DATA frame
    /// semantics. A trailer after an overrun remains observable with its exact
    /// metadata, and the hint remains unknown before and after it.
    #[tokio::test]
    async fn trailers_after_an_overrun_are_preserved() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::new(vec![
                Step::Data(b"abc"),
                Step::Trailers,
            ])),
            remaining: Some(2),
        };

        let data = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the boundary-crossing frame remains")
            .expect("no error")
            .into_data()
            .expect("the first frame is data");
        assert_eq!(data, Bytes::from_static(b"abc"));
        assert_eq!(body.size_hint().exact(), None);

        let trailers = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("trailers after an overrun are not EOF")
            .expect("trailers are not an error")
            .into_trailers()
            .expect("the second frame carries trailers");
        assert_eq!(
            trailers.get("x-checksum"),
            Some(&HeaderValue::from_static("ok")),
            "the wrapper must preserve exact trailer metadata"
        );
        let hint = body.size_hint();
        assert_eq!(hint.exact(), None, "a trailer cannot restore exactness");
        assert_eq!(hint.lower(), 0, "unknown keeps a zero lower bound");
        assert_eq!(hint.upper(), None, "unknown keeps no upper bound");
    }

    /// N1, and the worst of them: `is_end_stream` is not cosmetic metadata.
    ///
    /// Hyper checks it before retaining a response body — its HTTP/1
    /// dispatcher drops the body receiver when it returns true, and its HTTP/2
    /// server sends an end-stream response instead of building body state. A
    /// wrapper that wrongly answers `true` therefore makes a non-empty
    /// response go out **bodyless**: silent data loss, strictly worse than the
    /// wrong size hint this type was written to fix.
    ///
    /// `KnownSizeBody` delegates the method correctly and always has. Nothing
    /// asserted it, so the constant-`true` mutation passed every test.
    #[tokio::test]
    async fn a_known_size_body_with_bytes_left_is_not_end_of_stream() {
        let body = KnownSizeBody {
            inner: Body::new(ScriptedBody::data(&[b"abc"])),
            remaining: Some(3),
        };
        assert!(
            !body.is_end_stream(),
            "a body with bytes still to come must not report end-of-stream — \
             Hyper would suppress the payload entirely"
        );
    }

    /// N2, the same method in the other direction: the wrapper must DELEGATE
    /// this answer, never invent one. A constant `false` is also wrong, and
    /// also passed everything.
    #[tokio::test]
    async fn a_known_size_body_delegates_end_of_stream_to_the_inner_body() {
        let mut body = KnownSizeBody {
            inner: Body::from("x"),
            // Deliberately disagree with the one-byte inner body. If this were
            // `Some(1)`, draining the inner would make both `is_end_stream`
            // and `remaining == Some(0)` true at once, so the wrong
            // conjunction of those independent states would pass.
            remaining: Some(9),
        };
        assert!(
            !body.is_end_stream(),
            "precondition: a Full<Bytes> with its one frame still pending is \
             not yet at end of stream"
        );
        while std::pin::Pin::new(&mut body).frame().await.is_some() {}
        assert_eq!(
            body.size_hint().exact(),
            Some(8),
            "precondition: the byte hint remains nonzero after the inner ends"
        );
        assert!(
            body.is_end_stream(),
            "once the inner body is drained the wrapper must say so — the \
             answer belongs to `inner`, and this type only ever forwards it"
        );
    }

    /// A spent or invalidated size hint is an accounting state, not a stream
    /// lifecycle state. The inner body may still have an overrun frame,
    /// trailers, or an error queued after the declared byte count reaches
    /// zero. Hyper consults `is_end_stream` before polling those frames, so
    /// deriving this answer from `remaining` would silently discard them.
    ///
    /// This kills the mutation `remaining == Some(0) || inner.is_end_stream()`:
    /// the first frame spends the declared two bytes while two frames remain.
    /// The second frame then invalidates the hint while one frame remains,
    /// pinning the same invariant after `remaining` becomes `None`.
    #[tokio::test]
    async fn a_spent_or_invalidated_hint_does_not_end_the_inner_stream() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::data(&[b"ab", b"cde", b"f"])),
            remaining: Some(2),
        };

        let exact = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the exact-length frame remains")
            .expect("no error");
        assert_eq!(exact.data_ref().map(Bytes::len), Some(2));
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(
            !body.is_end_stream(),
            "spending the declared byte count must not hide an overrun frame"
        );

        let overrun = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the overrun frame remains")
            .expect("no error");
        assert_eq!(overrun.data_ref().map(Bytes::len), Some(3));
        assert_eq!(body.size_hint().exact(), None);
        assert!(
            !body.is_end_stream(),
            "invalidating the hint must not hide a later frame either"
        );

        let after_invalidation = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the post-invalidation frame remains")
            .expect("no error");
        assert_eq!(after_invalidation.data_ref().map(Bytes::len), Some(1));
    }

    /// N3: a `Pending` poll consumes no bytes and contradicts nothing, so it
    /// must leave the remaining count exactly where it was.
    ///
    /// Unreachable through `.frame().await`, which simply suspends — so this
    /// polls by hand with a no-op waker. That blind spot is the entire reason
    /// the mutation survived.
    #[tokio::test]
    async fn a_pending_poll_does_not_disturb_the_remaining_count() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::new(vec![
                Step::PendingOnce,
                Step::Data(b"abc"),
            ])),
            remaining: Some(3),
        };
        assert!(
            matches!(poll_once(&mut body), Poll::Pending),
            "precondition: the fixture must actually return Pending here, or \
             this test proves nothing about Pending"
        );
        assert_eq!(
            body.size_hint().exact(),
            Some(3),
            "Pending consumed no bytes, so all 3 must still be promised"
        );

        assert!(matches!(poll_once(&mut body), Poll::Ready(Some(Ok(_)))));
        assert_eq!(
            body.size_hint().exact(),
            Some(0),
            "and the data frame that followed still counts normally"
        );
    }

    /// The wrapper must pass the caller's actual context through to its inner
    /// body. Returning the inner `Pending` while polling it with a no-op waker
    /// looks correct in a hand-polled test, but the real future can then sleep
    /// forever because its wakeup was registered against nobody.
    #[test]
    fn a_pending_inner_body_receives_the_callers_waker() {
        let captured = Arc::new(Mutex::new(None));
        let mut body = KnownSizeBody {
            inner: Body::new(WakerProbeBody {
                captured: Arc::clone(&captured),
            }),
            remaining: Some(3),
        };
        let wake_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let caller = std::task::Waker::from(Arc::new(CountingWake(Arc::clone(&wake_count))));
        let mut cx = Context::from_waker(&caller);

        assert!(matches!(Pin::new(&mut body).poll_frame(&mut cx), Poll::Pending));
        let inner_waker = captured
            .lock()
            .expect("the waker probe lock is not poisoned")
            .take()
            .expect("the inner body must receive a waker before returning Pending");
        assert!(
            inner_waker.will_wake(&caller),
            "KnownSizeBody must forward the caller's waker, not substitute an \
             inert context"
        );
        inner_waker.wake_by_ref();
        assert_eq!(
            wake_count.load(Ordering::Relaxed),
            1,
            "waking through the inner body's captured waker must reach the caller"
        );
    }

    /// Reaching the declared byte count says nothing about whether the inner
    /// body is ready to yield its terminal trailers. `Pending` after exact
    /// DATA must remain `Pending`; laundering it into EOF would make the next
    /// frame unreachable even though the byte accounting looked complete.
    #[test]
    fn a_pending_poll_after_size_exhaustion_is_not_end_of_stream() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::new(vec![
                Step::Data(b"ab"),
                Step::PendingOnce,
                Step::Trailers,
            ])),
            remaining: Some(2),
        };

        let Poll::Ready(Some(Ok(data))) = poll_once(&mut body) else {
            panic!("the exact-length data frame must arrive first");
        };
        assert_eq!(data.data_ref().map(Bytes::len), Some(2));
        assert_eq!(body.size_hint().exact(), Some(0));

        assert!(
            matches!(poll_once(&mut body), Poll::Pending),
            "Pending after exact exhaustion is not clean EOF"
        );
        assert_eq!(
            body.size_hint().exact(),
            Some(0),
            "Pending consumes no bytes at the boundary"
        );

        let Poll::Ready(Some(Ok(trailers))) = poll_once(&mut body) else {
            panic!("the trailers after Pending must remain reachable");
        };
        assert!(trailers.is_trailers(), "the final frame carries trailers");
    }

    /// N4: an error frame must reach the caller as an error.
    ///
    /// Turning it into a clean `Ready(None)` converts a transport failure into
    /// a silent truncation — the response looks complete and is not. An
    /// existing test proves the middleware preserves errors generally, but its
    /// fixture has no exact size, so `rejoin` never wraps it in
    /// `KnownSizeBody` and this seam went unexercised.
    #[tokio::test]
    async fn an_error_frame_is_not_laundered_into_a_clean_end_of_stream() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::new(vec![Step::Data(b"ab"), Step::Error])),
            remaining: Some(9),
        };
        let first = std::pin::Pin::new(&mut body).frame().await;
        assert!(
            matches!(first, Some(Ok(_))),
            "precondition: data comes first"
        );
        assert!(
            matches!(std::pin::Pin::new(&mut body).frame().await, Some(Err(_))),
            "an error frame must surface as an error, never as clean \
            end-of-stream — that would report a truncated body as complete"
        );
    }

    /// Delivering every declared byte does not make a later transport error
    /// optional. The size hint describes DATA only; an error queued after an
    /// exact-length frame must still reach the caller with its identity intact
    /// and must not perturb the zero remaining count.
    #[tokio::test]
    async fn an_error_after_exact_data_is_not_laundered_into_eof() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::new(vec![Step::Data(b"ab"), Step::Error])),
            remaining: Some(2),
        };

        let data = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("the exact-length frame remains")
            .expect("the first frame is not an error")
            .into_data()
            .expect("the first frame is data");
        assert_eq!(data, Bytes::from_static(b"ab"));
        assert_eq!(body.size_hint().exact(), Some(0));

        let error = std::pin::Pin::new(&mut body)
            .frame()
            .await
            .expect("an error after exact DATA is not clean EOF")
            .expect_err("the second frame remains an error");
        assert_eq!(
            error.to_string(),
            "scripted body failure",
            "the wrapper must preserve the original transport error"
        );
        assert_eq!(
            body.size_hint().exact(),
            Some(0),
            "an error frame consumes no declared bytes"
        );
    }

    /// The opposite direction, which the first version of this fix left
    /// undefended: a body that yields FEWER bytes than it declared.
    ///
    /// The wrapper cannot detect this while frames are still arriving — a
    /// short body and a slow one look identical until the stream ends. What it
    /// must not do is keep promising the shortfall AFTER end-of-stream, since
    /// by then the promise is unfulfillable. Documenting the current behaviour
    /// honestly: `remaining` stays at the undelivered count, and the honest
    /// reading is that this hint was wrong from the start because the wrapped
    /// body lied. Pinned so a future change here is deliberate rather than
    /// accidental.
    #[tokio::test]
    async fn an_underrunning_body_is_recorded_as_it_actually_behaves() {
        let mut body = KnownSizeBody {
            inner: Body::new(ScriptedBody::data(&[b"ab"])),
            remaining: Some(9),
        };
        let mut delivered = 0usize;
        while let Some(frame) = std::pin::Pin::new(&mut body).frame().await {
            if let Some(data) = frame.expect("no errors").data_ref() {
                delivered += data.len();
            }
        }
        assert_eq!(delivered, 2, "precondition: the body really does underrun");
        assert_eq!(
            body.size_hint().exact(),
            Some(7),
            "current behaviour, pinned deliberately: 9 were promised, 2 \
             arrived, and the wrapper still reports the 7 that never came. \
             This is a faithful echo of the wrapped body's own false claim, \
             not an independent one — but a caller reading it after \
             end-of-stream is being told about bytes that will never arrive. \
             If this is ever changed to report unknown, change it here first"
        );
    }

    /// #515 defect 1: `rejoin` must not silently downgrade a body with a known
    /// exact length to unknown length just because classifying it required
    /// splitting it into a bytes-in-hand prefix plus an unread remainder.
    ///
    /// `/api/oversized-typed-refusal` is exactly that split: the handler's
    /// `String` body reports its own exact `size_hint` up front, `head` ends
    /// up non-empty (`MAX_ERROR_BODY` bytes), and a remainder still exists
    /// past it — the third, previously-lossy arm of `rejoin`.
    ///
    /// This is deliberately not "the bytes survive" — the test right above
    /// already proves that, and would still pass with the size hint silently
    /// reset to unknown. What is checked here is the numeric length the
    /// rejoined body *claims* about itself, compared against a length
    /// computed independently (the same serialization the fixture route
    /// performs), not against whatever `split_at_limit`/`rejoin` themselves
    /// happen to report.
    #[tokio::test]
    async fn rejoin_preserves_a_known_exact_size_across_a_non_empty_split() {
        let expected_total = serde_json::to_string(&AmendCommitError {
            kind: AmendFailureKind::HookRejected,
            message: oversized_hook_output(),
        })
        .unwrap()
        .len() as u64;
        assert!(
            expected_total > MAX_ERROR_BODY as u64,
            "the fixture must actually cross the cap, or `rejoin`'s empty-head \
             arm (which needs no fix) would be the one exercised instead"
        );

        let resp = app()
            .oneshot(get_req(
                "/api/oversized-typed-refusal",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.body().size_hint().exact(),
            Some(expected_total),
            "the rejoined body must still report the original body's exact \
             length rather than acquiring StreamBody's unknown-length default"
        );
        // N1 at the real seam: Hyper consults this before deciding whether the
        // response has a body at all.
        assert!(
            !resp.body().is_end_stream(),
            "a non-empty rejoined body must not claim end-of-stream — Hyper \
             would send the response bodyless"
        );
        // The size-hint fix must not have disturbed the bytes it was already
        // proven (above) to deliver whole.
        assert_eq!(body_string(resp).await.len() as u64, expected_total);
    }

    /// `None` means the original body was genuinely streaming and made no
    /// exact-length claim. Defaulting that absence to zero is not conservative:
    /// Hyper can turn `Some(0)` into `Content-Length: 0` and suppress the
    /// nonempty frames that follow before polling gets a chance to invalidate
    /// the lie.
    #[tokio::test]
    async fn rejoin_does_not_invent_zero_for_an_unknown_length() {
        let rest = Body::from_stream(async_stream::stream! {
            yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"def"));
        });
        assert_eq!(
            rest.size_hint().exact(),
            None,
            "precondition: the remainder is a genuine unknown-length stream"
        );

        let body = rejoin(Bytes::from_static(b"abc"), Some(rest), None);
        let hint = body.size_hint();
        assert_eq!(hint.exact(), None, "unknown must not default to exact zero");
        assert_eq!(hint.lower(), 0, "unknown has no positive lower bound");
        assert_eq!(hint.upper(), None, "unknown has no upper bound");

        let bytes = axum::body::to_bytes(body, 16)
            .await
            .expect("the rejoined stream remains readable");
        assert_eq!(bytes, Bytes::from_static(b"abcdef"));
    }

    /// #336, the other half: an over-cap body that is genuinely plain text is
    /// still enveloped — and the envelope carries what the server said, with an
    /// explicit truncation marker, instead of collapsing to the status's
    /// canonical reason.
    ///
    /// Two things are asserted separately on purpose. That the message is
    /// non-empty rules out the old `unwrap_or_default()` behaviour; that it
    /// *opens with the server's own words* rules out a fix that kept some
    /// arbitrary slice. The marker is asserted last so a client is never handed
    /// a sentence that stops mid-word and reads as the whole answer.
    #[tokio::test]
    async fn an_oversized_prose_refusal_is_enveloped_with_what_fits_and_says_so() {
        let resp = app()
            .oneshot(get_req(
                "/api/oversized-prose",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body = body_string(resp).await;
        let err: ApiError = serde_json::from_str(&body).unwrap_or_else(|e| {
            panic!("an over-cap prose refusal must still be a proper envelope ({e})")
        });
        assert_ne!(
            err.error.message,
            StatusCode::BAD_REQUEST.canonical_reason().unwrap(),
            "the envelope collapsed to the canonical reason — the server's own \
             words were discarded, which is exactly the #336 defect"
        );
        assert!(
            err.error.message.starts_with(OVERSIZED_PROSE_OPENING),
            "the envelope must open with what the server actually said: {:?}",
            &err.error.message[..err.error.message.len().min(120)]
        );
        assert!(
            err.error.message.contains("truncated"),
            "a truncated message must say it was truncated rather than read as \
             the whole of the answer"
        );
    }

    /// The paired negative for the prefix sniff: prose that merely *begins*
    /// with `{` and runs past the cap must not be mistaken for a truncated JSON
    /// object and forwarded as `application/json`.
    ///
    /// Without this, `incomplete_json_object_prefix` could be weakened to "the
    /// first non-space byte is `{`" and every test above would still pass,
    /// while a client received an English sentence labeled JSON — the
    /// regression `handlers::commit`'s own sniff comment warns is *worse* than
    /// the double-encoding #323 set out to fix.
    #[tokio::test]
    async fn over_cap_prose_that_merely_starts_with_a_brace_is_not_read_as_json() {
        let opening = "{ this is not JSON, it is a shell trace: ";
        assert!(
            !incomplete_json_object_prefix(
                format!("{opening}{}", "z".repeat(OVERSIZED_LEN)).as_bytes()
            ),
            "a prefix that starts with a brace but breaks JSON syntax on the \
             next token is prose, not a truncated object"
        );
        // …and the genuine article still reads as one, so the check above is
        // not just "always false".
        assert!(
            incomplete_json_object_prefix(
                format!(
                    r#"{{"kind":"hook_rejected","message":"{}"#,
                    "a".repeat(OVERSIZED_LEN)
                )
                .as_bytes()
            ),
            "an object cut off mid-string is exactly what a truncated prefix of \
             a typed DTO looks like"
        );
    }

    /// A complete JSON value at the cap says nothing about the unread bytes.
    /// Treating the prefix as proof would forward invalid JSON with a JSON
    /// content type instead of the bounded prose envelope.
    #[tokio::test]
    async fn a_complete_json_prefix_with_trailing_garbage_is_not_labeled_json() {
        let fixture = complete_json_prefix_with_trailing_garbage();
        assert!(
            serde_json::from_str::<serde_json::Value>(&fixture).is_err(),
            "the full fixture must be invalid JSON or this test proves nothing"
        );

        let resp = app()
            .oneshot(get_req(
                "/api/complete-json-prefix-with-trailing-garbage",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let raw = body_string(resp).await;
        let error: ApiError = serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!(
                "a complete prefix plus unread garbage must be enveloped as prose, \
                 not forwarded as invalid JSON ({e})"
            )
        });
        assert!(
            error.error.message.contains("truncated"),
            "the envelope must disclose that unread bytes were omitted"
        );
    }

    /// A response-body failure is part of the body stream, not a clean EOF.
    /// The contract layer may decline to classify the body, but it must not
    /// turn `[Data("{}"), Err(boom)]` into a successful two-byte body.
    #[tokio::test]
    async fn a_body_error_after_data_is_preserved() {
        let resp = app()
            .oneshot(get_req(
                "/api/data-then-error",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        let mut body = resp.into_body();
        let first = body
            .frame()
            .await
            .expect("the data frame must remain")
            .expect("the first frame is data");
        assert_eq!(first.into_data().expect("first frame is data"), "{}");
        let error = body
            .frame()
            .await
            .expect("the error frame must remain")
            .expect_err("the second frame must still be an error");
        assert!(
            error.to_string().contains("body exploded"),
            "the original transport error must survive: {error}"
        );
    }

    /// Trailers carry end-to-end metadata. Looking only at data frames must
    /// not erase them while rebuilding the response body.
    #[tokio::test]
    async fn trailers_after_data_are_preserved() {
        let resp = app()
            .oneshot(get_req(
                "/api/data-then-trailers",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("application/json")),
            "a complete JSON object remains JSON when trailers follow"
        );
        let mut body = resp.into_body();
        let first = body
            .frame()
            .await
            .expect("the data frame must remain")
            .expect("the first frame is data");
        assert_eq!(first.into_data().expect("first frame is data"), "{}");
        let trailers = body
            .frame()
            .await
            .expect("the trailers frame must remain")
            .expect("trailers are not a body error")
            .into_trailers()
            .expect("the second frame must still be trailers");
        assert_eq!(
            trailers
                .get("x-proof")
                .expect("the original x-proof trailer must survive"),
            "kept"
        );
    }

    /// Crossing the byte cap deliberately truncates unread prose data, but it
    /// must not turn a later transport failure into clean EOF or discard
    /// end-to-end trailer metadata along with those data bytes.
    #[tokio::test]
    async fn an_oversized_prose_tail_keeps_later_errors_and_trailers() {
        let resp = app()
            .oneshot(get_req(
                "/api/oversized-prose-then-error",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        let mut body = resp.into_body();
        let envelope = body
            .frame()
            .await
            .expect("the bounded envelope must remain")
            .expect("the envelope frame is data")
            .into_data()
            .expect("the first frame is the envelope");
        let parsed: ApiError = serde_json::from_slice(&envelope)
            .expect("the retained prefix must be a complete error envelope");
        assert!(parsed.error.message.contains("truncated"));
        let error = body
            .frame()
            .await
            .expect("the post-cap error frame must remain")
            .expect_err("the second frame must still be an error");
        assert!(
            error.to_string().contains("oversized body exploded"),
            "the original post-cap transport error must survive: {error}"
        );

        let resp = app()
            .oneshot(get_req(
                "/api/oversized-prose-then-trailers",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        let mut body = resp.into_body();
        let envelope = body
            .frame()
            .await
            .expect("the bounded envelope must remain")
            .expect("the envelope frame is data")
            .into_data()
            .expect("the first frame is the envelope");
        serde_json::from_slice::<ApiError>(&envelope)
            .expect("the retained prefix must be a complete error envelope");
        let trailers = body
            .frame()
            .await
            .expect("the post-cap trailers frame must remain")
            .expect("trailers are not a body error")
            .into_trailers()
            .expect("the second frame must still be trailers");
        assert_eq!(
            trailers
                .get("x-overflow-proof")
                .expect("the original post-cap trailer must survive"),
            "kept"
        );
    }

    /// The reader's own boundary: a body that ends *exactly* at the cap is
    /// complete, not "over it". Getting this wrong would push every 64-KiB-on-
    /// the-nose refusal down the truncation path and stamp it as truncated when
    /// nothing was lost.
    #[tokio::test]
    async fn a_body_that_ends_exactly_at_the_cap_reports_no_remainder() {
        let exact = vec![b'x'; MAX_ERROR_BODY];
        let (head, rest) = split_at_limit(Body::from(exact.clone()), MAX_ERROR_BODY).await;
        assert_eq!(head.len(), MAX_ERROR_BODY);
        assert!(
            matches!(rest, BodyRemainder::End),
            "a body of exactly the cap has no remainder to forward"
        );

        let (head, rest) =
            split_at_limit(Body::from([exact, vec![b'x']].concat()), MAX_ERROR_BODY).await;
        assert_eq!(head.len(), MAX_ERROR_BODY);
        assert!(
            matches!(rest, BodyRemainder::Overflow(_)),
            "one byte past the cap is a body with a remainder"
        );
    }

    /// #515 defect 2: a run of ready-but-empty data frames never grows
    /// `head.len()`, so a byte-only exit condition can never fire on its own —
    /// this proves `split_at_limit` also gives up on frame *count*.
    ///
    /// The body is a genuinely endless stream (no bound on iterations, no
    /// upper bound on frames), so a version of `split_at_limit` without the
    /// frame budget would never return from this call at all. Per this
    /// repo's own caution against wall-clock assertions, the bound here is
    /// not measuring speed — it is the only way to tell "hangs forever" from
    /// "returns" without actually waiting forever: a correct implementation
    /// returns almost immediately (a few thousand cheap iterations), and a
    /// broken one never returns, so any generous bound cleanly separates the
    /// two.
    ///
    /// Deliberately **not** `#[tokio::test]` plus `tokio::time::timeout`:
    /// measured directly against this exact fixture, that combination does
    /// not work. A stream whose every poll is immediately ready and
    /// self-wakes (exactly what an endless run of always-ready empty frames
    /// is) can starve a single-runtime timer outright — cooperative
    /// scheduling only interleaves at points where a task actually returns
    /// `Poll::Pending` to the scheduler and lets something else run, and nothing
    /// here forces that until frames stop coming, which is precisely the
    /// mechanism under test. So this drives `split_at_limit` on its own OS
    /// thread with its own throwaway runtime, and bounds the wait from the
    /// *main* thread with a plain `std::sync::mpsc::recv_timeout` — a deadline
    /// enforced by the OS, independent of whatever the worker thread's own
    /// runtime does internally, which is what makes it trustworthy here.
    #[test]
    fn an_endless_run_of_empty_frames_does_not_spin_split_at_limit() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let endless_empty_frames = Body::new(StreamBody::new(async_stream::stream! {
                loop {
                    yield Ok::<Frame<Bytes>, std::io::Error>(Frame::data(Bytes::new()));
                }
            }));
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("building a throwaway single-threaded runtime for this worker thread");
            let outcome = rt.block_on(split_at_limit(endless_empty_frames, MAX_ERROR_BODY));
            // If `split_at_limit` really is stuck, the main thread below has
            // already given up and dropped `rx` by the time (if ever) this
            // line is reached — a `send` failing on a dropped receiver must
            // not panic this (by-then-irrelevant, leaked) thread.
            let _ = tx.send(outcome);
        });

        let (head, remainder) = rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "split_at_limit must give up once its frame budget is exhausted \
             instead of spinning forever on frames that never grow `head`",
        );
        assert!(
            head.is_empty(),
            "every yielded frame carried zero data bytes, so the accumulated \
             prefix must still be empty"
        );
        assert!(
            matches!(remainder, BodyRemainder::Overflow(_)),
            "giving up on classification must forward the still-unread, \
             still-endless body rather than reporting a clean end"
        );
    }

    /// #336: a **success** body that is hand-serialized JSON is labeled
    /// `application/json` too, on every route rather than one.
    ///
    /// This is the half of `amend_route_response` that neither the issue nor
    /// the handoff described. That layer sniffed *every* output of
    /// `plan_and_execute`, not just the refusals, so `/api/amend-commit`'s 200
    /// was the only correctly-labeled success in the shared write channel —
    /// `/api/commit`, `/api/fetch`, `/api/pull` and `/api/tag` all sent a JSON
    /// object claiming to be `text/plain`. Deleting the layer without this
    /// un-labeled the one that worked; `relabel_json_success` labels all five.
    ///
    /// Caught by `state::tests::selection_flow_carries_mode_and_gates_writes`
    /// on CI, which cannot reach its own success leg in a container without
    /// Landlock — the reason it is pinned here as well, where no sandbox is
    /// involved and the assertion runs everywhere.
    #[tokio::test]
    async fn a_handlers_typed_json_success_is_labeled_json_too() {
        let resp = app()
            .oneshot(get_req(
                "/api/typed-success",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("application/json"),
            "a 200 carrying a serialized DTO must not claim to be prose: got \
             {content_type:?}"
        );
        let body = body_string(resp).await;
        let parsed: AmendCommitSuccess = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("the payload must survive relabeling ({e}): {body}"));
        assert_eq!(parsed.message, "Amended commit.");
    }

    /// The paired negative: a 200 that is genuinely prose keeps its own
    /// content-type and its own bytes. Without this, `relabel_json_success`
    /// could be "always label 200s JSON" and the test above would still pass.
    #[tokio::test]
    async fn a_plain_text_success_is_left_exactly_as_the_handler_built_it() {
        let resp = app()
            .oneshot(get_req(
                "/api/plain-success",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("text/plain"),
            "prose must stay prose: got {content_type:?}"
        );
        assert_eq!(body_string(resp).await, "not json at all");
    }

    /// The stream guard, and the reason `relabel_json_success` gates on a
    /// declared `content-length` rather than on status: the M1.08 progress
    /// stream (`/api/operations/{id}/events`) is a 200 that stays open for the
    /// life of an operation. Reading one frame of it to classify it would stall
    /// the thing it exists to deliver.
    ///
    /// Asserted on the header rather than by timing: a body with no declared
    /// length must come back with its own content-type untouched, which is only
    /// true if nothing polled it. The payload here deliberately *is* a JSON
    /// object inside an SSE frame, so a relabel that ignored the length gate
    /// would have something to latch onto.
    #[tokio::test]
    async fn a_streaming_success_is_never_read_to_classify_it() {
        let resp = app()
            .oneshot(get_req(
                "/api/streamed-success",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("text/event-stream"),
            "a stream must reach the client unread and unlabeled: got \
             {content_type:?} — the gate leaked and the body was polled"
        );

        // The mechanism the assertion above rests on, stated directly: a
        // streamed body reports no exact size, an in-memory one does. If this
        // ever stops holding, the assertion above silently stops testing
        // anything.
        assert!(
            Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from_static(b"{}"));
            })
            .size_hint()
            .exact()
            .is_none(),
            "a streamed body must not report an exact size"
        );
        assert_eq!(
            Body::from("{}").size_hint().exact(),
            Some(2),
            "an in-memory body must report its exact size"
        );
    }

    // --- The "no path-based repository selection" guard, at the wire ------------
    //
    // Repository selection is process-global (`state::CURRENT`), set only at
    // startup (CLI arg) and by `POST /api/clone` (to a *server-chosen* temp dir).
    // No handler reads a repo/path from the request. This test pins that at the
    // wire: a write body carrying a stray path/repo field is *rejected*, never
    // silently dropped — so no future handler can start honouring one.
    #[tokio::test]
    async fn a_write_body_smuggling_a_repo_path_is_rejected() {
        let body = r#"{"name":"b","commit":"c","repo":"/etc/passwd"}"#;
        let req = HttpRequest::post("/api/branch")
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        // Rejected before the handler runs (deny_unknown_fields), and the refusal
        // is the structured envelope like every other error.
        assert!(
            resp.status().is_client_error(),
            "unexpected status: {}",
            resp.status()
        );
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        // The `repo` field never reached a handler — this is a body-shape refusal.
        assert!(
            err.error.message.to_lowercase().contains("repo")
                || err.error.message.to_lowercase().contains("unknown field"),
            "message should name the rejected field: {}",
            err.error.message
        );
    }

    // -----------------------------------------------------------------------
    // M1.08 — the stream's query-string negotiation, and the idempotency scope
    // -----------------------------------------------------------------------

    /// `EventSource` cannot set headers, so the stream route — and only it —
    /// may name its protocol version in the query string.
    #[tokio::test]
    async fn the_stream_route_negotiates_through_the_query_string() {
        let path = format!("/api/operations/abc/events?{PROTOCOL_QUERY}={PROTOCOL_VERSION}");
        let resp = app().oneshot(get_req(&path, None)).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// The exception is narrow: it buys the stream nothing but a different
    /// *place* to read the version from. An out-of-window value in the query
    /// string is refused exactly like an out-of-window header.
    #[tokio::test]
    async fn an_out_of_window_query_version_is_refused_like_a_header() {
        let path = format!("/api/operations/abc/events?{PROTOCOL_QUERY}=999999");
        let resp = app().oneshot(get_req(&path, None)).await.unwrap();
        assert_eq!(resp.status(), 426);
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(err.error.code, ErrorCode::ProtocolIncompatible);
    }

    /// A stream opened with no version at all is still refused — the route is
    /// exempt from the *header*, never from negotiation.
    #[tokio::test]
    async fn a_stream_with_no_version_anywhere_is_refused() {
        let resp = app()
            .oneshot(get_req("/api/operations/abc/events", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), 426);
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(err.error.code, ErrorCode::MissingProtocolHeader);
    }

    /// And no *other* path inherits the exception, including its siblings under
    /// `/api/operations/`.
    #[tokio::test]
    async fn no_other_route_may_negotiate_through_the_query_string() {
        let path = format!("/api/operations/abc?{PROTOCOL_QUERY}={PROTOCOL_VERSION}");
        let resp = app().oneshot(get_req(&path, None)).await.unwrap();
        assert_eq!(resp.status(), 426);
        assert!(!accepts_protocol_query("/api/operations/abc"));
        assert!(!accepts_protocol_query("/api/operations/abc/events/extra"));
        assert!(!accepts_protocol_query("/api/commits"));
        assert!(accepts_protocol_query("/api/operations/abc/events"));
    }

    /// A valid key reaches the handler through the task-local scope — which is
    /// how fifteen write handlers get it without naming it.
    #[tokio::test]
    async fn a_valid_idempotency_key_is_in_scope_for_the_handler() {
        let req = HttpRequest::get("/api/operations/abc")
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .header(IDEMPOTENCY_HEADER, "gv-abc-123")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(body_string(resp).await, "gv-abc-123");
    }

    /// A malformed key is a wire error, refused before any handler sees it —
    /// the planner must never be handed a value that failed validation.
    #[tokio::test]
    async fn a_malformed_idempotency_key_is_refused_at_the_wire() {
        let req = HttpRequest::get("/api/operations/abc")
            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
            .header(IDEMPOTENCY_HEADER, "not a token")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let err: ApiError = serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(
            err.error.message.contains(IDEMPOTENCY_HEADER),
            "the refusal should name the header: {}",
            err.error.message
        );
    }

    /// A request with no key at all passes through — reads need none, and
    /// whether a *write* needs one is the planner's call, not this layer's.
    #[tokio::test]
    async fn a_request_without_a_key_passes_through() {
        let resp = app()
            .oneshot(get_req(
                "/api/operations/abc",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(body_string(resp).await, "no-key");
    }
}
