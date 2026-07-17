//! Wire-level request protections for the loopback service (M1.04, #57).
//!
//! Two `tower` layers implement the "Local and SSH Session Design" and "Browser
//! Security Headers" sections of the [`SECURITY_MODEL`](../../../docs/SECURITY_MODEL.md):
//!
//!   * [`require_auth`] wraps every `/api/*` route (inside the M1.02 contract
//!     layer, so its refusals still come back as the structured error envelope).
//!     It enforces, in order: an HTTP **method** allowlist, a **Host** allowlist
//!     that defeats DNS-rebinding by refusing any host that is not loopback, an
//!     **Origin** allowlist (present ⇒ same-origin; `null` refused), a
//!     **content-type** rule that blocks form-encoded CSRF, and finally a valid
//!     **session** — plus a matching **CSRF** token on every state-changing
//!     request. It also stamps `Cache-Control: no-store` on every API response.
//!   * [`security_headers`] stamps the CSP and companion headers onto *every*
//!     response (API and the SPA shell alike), denying framing and cross-origin
//!     embedding and pinning script/style/connect sources to `'self'`.
//!
//! Two paths are exempt from the *session* requirement (never from Host/Origin):
//! `GET /api/protocol`, hit to learn the protocol before anything else, and
//! `GET`/`POST /api/session`, which is how a client checks or establishes a
//! session in the first place. Everything else needs one.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, header::HeaderName, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::session::{ct_eq, SessionManager, CSRF_HEADER, SESSION_COOKIE};

/// The one endpoint hit before a session exists, to learn the protocol.
const NEGOTIATION_PATH: &str = "/api/protocol";
/// The session endpoint: `GET` checks, `POST` establishes — both pre-session.
const SESSION_PATH: &str = "/api/session";

/// State threaded into [`require_auth`]: the session store plus the loopback-only
/// Host/Origin policy. Cheap to clone (two `Arc`s / a port number).
#[derive(Clone)]
pub(crate) struct AuthState {
    pub manager: Arc<SessionManager>,
    pub hosts: HostPolicy,
}

/// Which `Host`/`Origin` values the loopback listener accepts. Only loopback
/// names pass, which makes a DNS-rebinding attack (whose `Host` is the attacker's
/// domain) fail closed.
#[derive(Clone)]
pub(crate) struct HostPolicy {
    /// The port the service is bound to; a `Host`/`Origin` naming a different port
    /// is rejected.
    port: u16,
}

impl HostPolicy {
    /// Create the strict loopback policy for the listener's fixed port.
    pub(crate) fn loopback(port: u16) -> Self {
        Self { port }
    }

    /// Whether a raw `Host` header value is acceptable. The host must be a
    /// loopback literal and any supplied port must match the bind port.
    fn host_allowed(&self, host: &str) -> bool {
        let (name, port) = split_host_port(host);
        if let Some(port) = port {
            if port != self.port {
                return false;
            }
        }
        is_loopback_name(name)
    }

    /// Whether an `Origin` header value is acceptable: a same-origin `http`/`https`
    /// origin whose host passes [`host_allowed`](Self::host_allowed). The literal
    /// `null` (opaque origin — a sandboxed frame, a `file://` page, some redirects)
    /// is always refused, as the security model requires for mutations.
    fn origin_allowed(&self, origin: &str) -> bool {
        if origin.eq_ignore_ascii_case("null") {
            return false;
        }
        let rest = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"));
        match rest {
            Some(host) => self.host_allowed(host),
            None => false,
        }
    }
}

/// Split a `host[:port]` (or `[ipv6]:port`) into its host and optional port.
fn split_host_port(host: &str) -> (&str, Option<u16>) {
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` or `[::1]` — the bracketed literal, then an optional port.
        if let Some((name, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (name, port);
        }
        return (host, None);
    }
    match host.rsplit_once(':') {
        // Only treat the tail as a port when it parses as one — otherwise it's a
        // bare host (no colon-bearing hostnames reach us in loopback mode).
        Some((name, tail)) => match tail.parse::<u16>() {
            Ok(port) => (name, Some(port)),
            Err(_) => (host, None),
        },
        None => (host, None),
    }
}

/// Whether `name` is a loopback host literal — the set a browser resolves to
/// `127.0.0.1` / `::1` for a same-machine or SSH-tunnelled connection.
fn is_loopback_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1"
}

/// State-changing methods carry the CSRF + content-type requirements; reads
/// (`GET`/`HEAD`) need only a session.
fn is_state_changing(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// The method allowlist: anything outside it (`OPTIONS`, `TRACE`, `CONNECT`, …) is
/// refused. We serve no CORS, so a cross-origin preflight `OPTIONS` gets no
/// allow-headers and the browser blocks the real request — exactly what we want.
fn is_allowed_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Read a named cookie's value out of the `Cookie` header (`a=1; b=2`). A minimal
/// parser — we look up one exact name and want no cookie-crate dependency. Shared
/// with [`crate::handlers::session`] for reading the session cookie.
pub(crate) fn cookie_value<'a>(headers: &'a header::HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then_some(v.trim())
    })
}

/// Whether the `Content-Type` (if any) is JSON. A missing content type is fine
/// (our bodyless writes send none); a *present* non-JSON type on a write is the
/// form-encoded CSRF vector we reject.
fn content_type_ok_for_write(headers: &header::HeaderMap) -> bool {
    match headers.get(header::CONTENT_TYPE) {
        None => true,
        Some(value) => value
            .to_str()
            .map(|v| {
                v.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("application/json")
            })
            .unwrap_or(false),
    }
}

/// Build a plain `(status, message)` error. It is returned *inside* the M1.02
/// contract layer, which rewraps it into the structured [`ApiError`] envelope with
/// the request id and a code derived from the status — so auth refusals answer in
/// the same shape as every other failure. Carries `no-store` like every other API
/// response, so a refusal is never cached either.
fn deny(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        message.to_string(),
    )
        .into_response()
}

/// The `/api/*` gate — see the module docs for the full ordered list of checks.
pub(crate) async fn require_auth(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let headers = request.headers();

    // 1. Method allowlist.
    if !is_allowed_method(&method) {
        return deny(
            StatusCode::METHOD_NOT_ALLOWED,
            "This method is not allowed.",
        );
    }

    // 2. Host — the anti-DNS-rebinding check. A missing Host on HTTP/1.1 is itself
    //    invalid; treat it as unacceptable.
    let host_ok = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| state.hosts.host_allowed(h));
    if !host_ok {
        return deny(
            StatusCode::FORBIDDEN,
            "Request rejected: unexpected Host. git-vista only answers on localhost.",
        );
    }

    // 3. Origin — when the browser sends one it must be same-origin; `null` is
    //    always refused. (Same-origin GETs often omit it; that's allowed.)
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        if !state.hosts.origin_allowed(origin) {
            return deny(
                StatusCode::FORBIDDEN,
                "Request rejected: cross-origin request to a local-only service.",
            );
        }
    }

    // 4. Content type on writes — blocks the form-encoded CSRF vector.
    if is_state_changing(&method) && !content_type_ok_for_write(headers) {
        return deny(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Request rejected: state-changing requests must send JSON.",
        );
    }

    // 5. Session + CSRF, unless this is a pre-session endpoint.
    let session_exempt = path == NEGOTIATION_PATH
        || (path == SESSION_PATH && matches!(method, Method::GET | Method::POST));
    if !session_exempt {
        let cookie = cookie_value(headers, SESSION_COOKIE);
        if is_state_changing(&method) {
            // Writes need a live session *and* the matching CSRF header.
            let expected = match cookie.and_then(|id| state.manager.validate(id)) {
                Some(csrf) => csrf,
                None => return deny(StatusCode::UNAUTHORIZED, "No active session. Reconnect."),
            };
            let presented = headers.get(CSRF_HEADER).and_then(|c| c.to_str().ok());
            let csrf_ok = presented.is_some_and(|c| ct_eq(expected.as_bytes(), c.as_bytes()));
            if !csrf_ok {
                return deny(
                    StatusCode::FORBIDDEN,
                    "Request rejected: missing or invalid CSRF token.",
                );
            }
        } else {
            // Reads need only a live session.
            if cookie.and_then(|id| state.manager.validate(id)).is_none() {
                return deny(StatusCode::UNAUTHORIZED, "No active session. Reconnect.");
            }
        }
    }

    let mut response = next.run(request).await;
    // Authenticated API data is never cacheable — the security model's
    // `Cache-Control: no-store` for API responses.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The browser hardening headers, stamped on every response (API and SPA shell).
///
/// The CSP pins every fetchable source to `'self'`, denies framing
/// (`frame-ancestors 'none'`) and object/base hijacking, and keeps `connect-src`
/// same-origin so a compromised script can't exfiltrate to another host. Two
/// necessary relaxations: `'wasm-unsafe-eval'` (the WebAssembly runtime needs it)
/// and `'unsafe-inline'` for script/style — Trunk injects an *inline* module
/// script to boot the wasm whose hash changes every build, so a static
/// nonce/hash header can't pin it; the residual XSS surface is bounded by Leptos's
/// default output escaping and the loopback + session model. See ADR 0004.
const CSP: &str = "default-src 'self'; \
     script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; \
     font-src 'self'; \
     connect-src 'self'; \
     object-src 'none'; \
     base-uri 'none'; \
     frame-ancestors 'none'; \
     form-action 'self'";

/// Stamp the hardening headers. Guarded so a header a handler already set (e.g. a
/// stricter `Cache-Control`) is not clobbered here.
pub(crate) async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    set_if_absent(headers, header::CONTENT_SECURITY_POLICY, CSP);
    set_if_absent(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set_if_absent(headers, header::REFERRER_POLICY, "no-referrer");
    set_if_absent(
        headers,
        HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    );
    set_if_absent(
        headers,
        HeaderName::from_static("cross-origin-resource-policy"),
        "same-origin",
    );
    set_if_absent(
        headers,
        HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=()",
    );
    set_if_absent(headers, header::X_FRAME_OPTIONS, "DENY");
    response
}

/// Insert `(name, value)` only if the response doesn't already carry `name`.
fn set_if_absent(headers: &mut header::HeaderMap, name: HeaderName, value: &'static str) {
    if !headers.contains_key(&name) {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> HostPolicy {
        HostPolicy::loopback(8080)
    }

    #[test]
    fn split_host_port_handles_forms() {
        assert_eq!(split_host_port("localhost"), ("localhost", None));
        assert_eq!(split_host_port("127.0.0.1:8080"), ("127.0.0.1", Some(8080)));
        assert_eq!(split_host_port("[::1]:8080"), ("::1", Some(8080)));
        assert_eq!(split_host_port("[::1]"), ("::1", None));
    }

    #[test]
    fn loopback_hosts_pass_and_others_fail() {
        let p = loopback();
        assert!(p.host_allowed("localhost:8080"));
        assert!(p.host_allowed("127.0.0.1:8080"));
        assert!(p.host_allowed("localhost"));
        assert!(p.host_allowed("[::1]:8080"));
        // DNS-rebinding attacker host, wrong port, and LAN IP all fail.
        assert!(!p.host_allowed("evil.example.com"));
        assert!(!p.host_allowed("localhost:9999"));
        assert!(!p.host_allowed("192.168.1.5:8080"));
    }

    #[test]
    fn origin_must_be_same_origin_and_not_null() {
        let p = loopback();
        assert!(p.origin_allowed("http://localhost:8080"));
        assert!(p.origin_allowed("http://127.0.0.1:8080"));
        assert!(!p.origin_allowed("null"));
        assert!(!p.origin_allowed("https://evil.example.com"));
        assert!(!p.origin_allowed("http://localhost:9999"));
        assert!(!p.origin_allowed("ftp://localhost:8080"));
    }

    #[test]
    fn content_type_rule_blocks_forms_but_allows_bodyless_and_json() {
        let mut h = header::HeaderMap::new();
        assert!(content_type_ok_for_write(&h)); // bodyless write: no content type
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(content_type_ok_for_write(&h));
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        assert!(!content_type_ok_for_write(&h));
    }

    #[test]
    fn cookie_lookup_finds_the_named_value() {
        let mut h = header::HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; gv_session=abc123; x=2"),
        );
        assert_eq!(cookie_value(&h, SESSION_COOKIE), Some("abc123"));
        assert_eq!(cookie_value(&h, "missing"), None);
    }
}

/// End-to-end tests of the auth layer wired over a router — the "route tests for
/// missing/invalid session, CSRF, Origin, Host, content type" the security model's
/// testing section calls for. Drives the real [`require_auth`] layer (without the
/// contract layer, so refusals arrive as plain status codes) via `oneshot`.
#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::handlers::session::{create_session, revoke_session, session_status};
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::{get, post},
        Router,
    };
    use git_vista_protocol::SessionInfo;
    use tower::ServiceExt;

    /// The wired router plus the session store it shares, so a test can read the
    /// current bootstrap token to establish a session.
    fn app() -> (Router, Arc<SessionManager>) {
        let sessions = Arc::new(SessionManager::new(None));
        let auth_state = AuthState {
            manager: sessions.clone(),
            hosts: HostPolicy::loopback(8080),
        };
        let router = Router::new()
            .route(
                "/api/session",
                get(session_status)
                    .post(create_session)
                    .delete(revoke_session),
            )
            .route("/api/commits", get(|| async { "graph" }))
            .route("/api/branch", post(|| async { "made" }))
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                require_auth,
            ))
            .with_state(sessions.clone());
        (router, sessions)
    }

    /// A request builder pre-loaded with a valid loopback `Host`.
    fn req(method: &str, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:8080")
    }

    /// Bootstrap a session, returning `(cookie value for the Cookie header, csrf)`.
    async fn bootstrap(router: &Router, sessions: &SessionManager) -> (String, String) {
        let token = sessions.current_bootstrap();
        let resp = router
            .clone()
            .oneshot(
                req("POST", "/api/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bootstrap should succeed");
        // The session id rides in the Set-Cookie header; the csrf in the JSON body.
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
        let cookie = set_cookie.split(';').next().unwrap().to_string();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let info: SessionInfo = serde_json::from_slice(&bytes).unwrap();
        (cookie, info.csrf.unwrap())
    }

    #[tokio::test]
    async fn a_read_without_a_session_is_401_and_no_store() {
        let (router, _) = app();
        let resp = router
            .oneshot(req("GET", "/api/commits").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Every API response carries the no-store cache directive.
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    #[tokio::test]
    async fn a_read_with_a_session_passes() {
        let (router, sessions) = app();
        let (cookie, _csrf) = bootstrap(&router, &sessions).await;
        let resp = router
            .oneshot(
                req("GET", "/api/commits")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// An SSH local forward is only a transport: dropping and recreating it must
    /// not revoke the browser's Git-Vista session. Model that boundary by ending
    /// one request/response completely, then reconnecting with the same cookie
    /// through a fresh service call and finally reading the graph again.
    #[tokio::test]
    async fn a_session_survives_tunnel_disconnect_and_reconnect() {
        let (router, sessions) = app();
        let (cookie, _csrf) = bootstrap(&router, &sessions).await;

        let before_disconnect = router
            .clone()
            .oneshot(
                req("GET", "/api/commits")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(before_disconnect.status(), StatusCode::OK);
        drop(before_disconnect);

        // No server-side connection object is retained. A new request carrying
        // the browser cookie recovers the session and its in-memory CSRF token.
        let reconnected = router
            .clone()
            .oneshot(
                req("GET", "/api/session")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reconnected.status(), StatusCode::OK);
        let bytes = to_bytes(reconnected.into_body(), 64 * 1024).await.unwrap();
        let info: SessionInfo = serde_json::from_slice(&bytes).unwrap();
        assert!(info.authenticated);
        assert!(info.csrf.is_some());

        let graph_after_reconnect = router
            .oneshot(
                req("GET", "/api/commits")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(graph_after_reconnect.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_write_needs_both_session_and_csrf() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;

        // No session at all → 401.
        let resp = router
            .clone()
            .oneshot(req("POST", "/api/branch").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Session but no CSRF header → 403.
        let resp = router
            .clone()
            .oneshot(
                req("POST", "/api/branch")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Session + matching CSRF → through.
        let resp = router
            .oneshot(
                req("POST", "/api/branch")
                    .header(header::COOKIE, cookie)
                    .header(CSRF_HEADER, csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_form_content_type_write_is_415() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;
        let resp = router
            .oneshot(
                req("POST", "/api/branch")
                    .header(header::COOKIE, cookie)
                    .header(CSRF_HEADER, csrf)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("name=x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn a_bad_host_is_403_before_anything_else() {
        let (router, _) = app();
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/commits")
                    .header(header::HOST, "evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_unknown_method_is_405() {
        let (router, _) = app();
        let resp = router
            .oneshot(req("OPTIONS", "/api/commits").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_cross_origin_or_null_origin_is_403() {
        for origin in ["https://evil.example.com", "null"] {
            let (router, _) = app();
            let resp = router
                .oneshot(
                    req("GET", "/api/commits")
                        .header(header::ORIGIN, origin)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "origin {origin}");
        }
    }

    #[tokio::test]
    async fn the_bootstrap_token_is_single_use_over_the_wire() {
        let (router, sessions) = app();
        let token = sessions.current_bootstrap();
        let post = || {
            router.clone().oneshot(
                req("POST", "/api/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
        };
        assert_eq!(post().await.unwrap().status(), StatusCode::OK);
        // The same token can't be redeemed twice — the second try is unauthorized.
        assert_eq!(post().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoking_a_session_stops_it_working() {
        let (router, sessions) = app();
        let (cookie, csrf) = bootstrap(&router, &sessions).await;
        // Revoke (a write: needs session + csrf).
        let resp = router
            .clone()
            .oneshot(
                req("DELETE", "/api/session")
                    .header(header::COOKIE, cookie.clone())
                    .header(CSRF_HEADER, csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The same cookie no longer authenticates a read.
        let resp = router
            .oneshot(
                req("GET", "/api/commits")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
