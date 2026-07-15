//! The session endpoints (M1.04, #57): establish, check, and revoke a session.
//!
//!   * `POST /api/session` — exchange the one-time bootstrap token (read by the
//!     SPA from the `#s=<token>` URL fragment) for an HttpOnly, `SameSite=Strict`
//!     session cookie, returning the session's CSRF token in the body.
//!   * `GET  /api/session` — report whether the caller already holds a live
//!     session (and hand back its CSRF token), so a reload recovers without
//!     re-bootstrapping. Both are exempt from the session gate in
//!     [`crate::security`] — they are how a session comes to exist.
//!   * `DELETE /api/session` — revoke the current session and clear the cookie.
//!
//! The cookie is **not** `Secure`: the supported modes (Local, SSH tunnel) serve
//! plain HTTP on loopback, where a `Secure` cookie would simply be dropped. When
//! an HTTPS LAN/paired mode arrives (a later milestone) the flag must be added.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header::SET_COOKIE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use git_vista_protocol::{SessionInfo, SessionRequest};

use crate::security::cookie_value;
use crate::session::{SessionManager, SESSION_COOKIE, SESSION_MAX_AGE_SECS};

/// `POST /api/session`: exchange a bootstrap token for a session cookie.
pub(crate) async fn create_session(
    State(manager): State<Arc<SessionManager>>,
    Json(body): Json<SessionRequest>,
) -> Response {
    match manager.exchange(body.token.trim()) {
        Some(session) => {
            let cookie = format!(
                "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_MAX_AGE_SECS}",
                session.id
            );
            (
                [(SET_COOKIE, cookie)],
                Json(SessionInfo {
                    authenticated: true,
                    csrf: Some(session.csrf),
                }),
            )
                .into_response()
        }
        // The one auth failure a normal client recovers from — the contract layer
        // maps this 401 to the `unauthenticated` code the SPA keys its bootstrap
        // screen on.
        None => (
            StatusCode::UNAUTHORIZED,
            "That setup link is invalid or has expired. Get a fresh one from `gv`.",
        )
            .into_response(),
    }
}

/// `GET /api/session`: report the current session state (always `200`).
pub(crate) async fn session_status(
    State(manager): State<Arc<SessionManager>>,
    headers: HeaderMap,
) -> Response {
    let csrf = cookie_value(&headers, SESSION_COOKIE).and_then(|id| manager.validate(id));
    Json(SessionInfo {
        authenticated: csrf.is_some(),
        csrf,
    })
    .into_response()
}

/// `DELETE /api/session`: revoke the current session and clear the cookie. Passes
/// the auth gate (needs a live session + CSRF), so it only ever revokes the
/// caller's own session. Clearing the cookie is unconditional, so a
/// double-logout still leaves the browser clean.
pub(crate) async fn revoke_session(
    State(manager): State<Arc<SessionManager>>,
    headers: HeaderMap,
) -> Response {
    if let Some(id) = cookie_value(&headers, SESSION_COOKIE) {
        manager.revoke(id);
    }
    let clear = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    (
        [(SET_COOKIE, clear)],
        Json(SessionInfo {
            authenticated: false,
            csrf: None,
        }),
    )
        .into_response()
}
