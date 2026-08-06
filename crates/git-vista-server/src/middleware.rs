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
    body::to_bytes,
    extract::Request,
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use git_vista_protocol::{
    check_compatibility, parse_protocol_header, ApiError, ErrorCode, IdempotencyKey, OperationId,
    RequestId, IDEMPOTENCY_HEADER, MAX_CLIENT_PROTOCOL, MIN_CLIENT_PROTOCOL, OPERATION_HEADER,
    PROTOCOL_HEADER, PROTOCOL_QUERY, PROTOCOL_VERSION, REQUEST_ID_HEADER,
};

/// The one path exempt from the protocol-header requirement: a client hits it
/// precisely to *learn* the protocol, so it cannot be required to already speak it.
const NEGOTIATION_PATH: &str = "/api/protocol";

/// Upper bound on how much of an error response body we'll buffer to rewrap it.
/// Error messages are git stderr / short strings; this cap also stops a
/// pathological body from being buffered whole.
const MAX_ERROR_BODY: usize = 64 * 1024;

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
                response
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
async fn rewrap_error(response: Response, request_id: &RequestId) -> Response {
    if is_json(&response) {
        return response;
    }
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_ERROR_BODY)
        .await
        .unwrap_or_default();
    let message = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("Request failed")
            .to_string()
    } else {
        message
    };
    let code = ErrorCode::from_status(status.as_u16());
    (
        status,
        Json(ApiError::new(code, message, request_id.clone())),
    )
        .into_response()
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
        http::Request as HttpRequest,
        routing::{get, post},
        Router,
    };
    use git_vista_protocol::ApiError;
    use tower::ServiceExt;

    // A tiny router carrying the contract layer over a few representative routes:
    // the exempt negotiation endpoint, a plain OK route, a route that returns a
    // handler-style `(StatusCode, String)` error, and the real `create_branch`
    // write handler (to exercise body rejection at the wire).
    fn app() -> Router {
        Router::new()
            .route("/api/protocol", get(|| async { "negotiation" }))
            .route("/api/ok", get(|| async { "ok-body" }))
            .route(
                "/api/boom",
                get(|| async { (StatusCode::NOT_FOUND, "No such commit.") }),
            )
            // #323: a handler that *hand-serializes* its own JSON body. The
            // real `/api/amend-commit` refusal path, reached through this
            // layer rather than by calling the planner function directly —
            // which is precisely the gap that let the defect sit unnoticed.
            .route(
                "/api/amend-refusal",
                get(|| async {
                    crate::planner::amend_refusal(
                        git_vista_protocol::AmendFailureKind::Other,
                        "Commit message can't be empty.",
                    )
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

    async fn body_string(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), MAX_ERROR_BODY)
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

    /// #323: a hand-serialized JSON body must reach the client as **one** JSON
    /// object, not as an `ApiError` envelope with the real payload buried in
    /// its `message` field as an escaped string.
    ///
    /// `amend_refusal` builds an `AmendCommitError` and serializes it itself,
    /// returning a `String` — which axum labels `text/plain`. `rewrap_error`
    /// keys on content-type, sees a non-JSON body, and wraps it. The endpoint's
    /// documented contract ("**every** 400 body from this route parses as
    /// `AmendCommitError`") is then false at the wire.
    ///
    /// This can only be seen through the layer. The existing contract-suite
    /// test calls the planner function directly, so it agrees with the
    /// docstring and misses the defect entirely.
    #[tokio::test]
    async fn a_hand_serialized_refusal_is_not_double_encoded() {
        let resp = app()
            .oneshot(get_req(
                "/api/amend-refusal",
                Some(&PROTOCOL_VERSION.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body = body_string(resp).await;

        // The contract: it parses as the payload type, directly.
        let refusal: git_vista_protocol::AmendCommitError = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("400 body did not parse as AmendCommitError ({e}): {body}"));
        assert_eq!(refusal.message, "Commit message can't be empty.");

        // And the failure mode, named explicitly: not an envelope carrying the
        // real body as an escaped string.
        assert!(
            serde_json::from_str::<ApiError>(&body).is_err(),
            "the refusal was rewrapped into an ApiError envelope — double-encoded: {body}"
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
