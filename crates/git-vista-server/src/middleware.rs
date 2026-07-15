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

use axum::{
    body::to_bytes,
    extract::Request,
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use git_vista_protocol::{
    check_compatibility, parse_protocol_header, ApiError, ErrorCode, RequestId,
    MAX_CLIENT_PROTOCOL, MIN_CLIENT_PROTOCOL, PROTOCOL_HEADER, PROTOCOL_VERSION, REQUEST_ID_HEADER,
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
    let raw = request.headers().get(PROTOCOL_HEADER).ok_or_else(|| {
        (
            ErrorCode::MissingProtocolHeader,
            format!("This request needs the {PROTOCOL_HEADER} header. Reload the app to update."),
        )
    })?;
    let raw = raw.to_str().map_err(|_| {
        (
            ErrorCode::InvalidProtocolHeader,
            format!("The {PROTOCOL_HEADER} header isn't valid text."),
        )
    })?;
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
            .route("/api/branch", post(crate::handlers::branch::create_branch))
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
}
