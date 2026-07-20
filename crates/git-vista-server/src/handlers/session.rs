//! The session endpoints (M1.04, #57; LAN rate limit and `via_lan` ADR 0005).
//!
//!   * `POST /api/session` — exchange the one-time bootstrap token (read by the
//!     SPA from the `#s=<token>` URL fragment) for an HttpOnly, `SameSite=Strict`
//!     session cookie, returning the session's CSRF token in the body. On the LAN
//!     listener this is also rate-limited per source IP (ADR 0005).
//!   * `GET  /api/session` — report whether the caller already holds a live
//!     session (and hand back its CSRF token), so a reload recovers without
//!     re-bootstrapping. Both are exempt from the session gate in
//!     [`crate::security`] — they are how a session comes to exist.
//!   * `DELETE /api/session` — revoke the current session and clear the cookie.
//!
//! Every response also carries `via_lan`, stamped from which router served the
//! request (see [`SessionState`]) — the frontend's mode screen uses it to hide
//! the Active option on a LAN session. This is a UI signal only: the LAN
//! router's write routes are structurally absent regardless (main.rs).
//!
//! The cookie is **not** `Secure`: the supported modes (Local, SSH tunnel, LAN
//! view) all serve plain HTTP, where a `Secure` cookie would simply be dropped.
//! When an HTTPS LAN/paired mode arrives (a later milestone) the flag must be added.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State},
    http::{header::SET_COOKIE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use git_vista_protocol::{SessionInfo, SessionRequest};

use crate::ratelimit::SignInLimiter;
use crate::security::cookie_value;
use crate::session::{SessionManager, SESSION_COOKIE, SESSION_MAX_AGE_SECS};

/// Per-router session-handler state: the shared session store, whether this
/// router is the LAN listener (stamped into every `SessionInfo.via_lan`), and
/// an optional sign-in rate limiter — `Some` only on the LAN router.
#[derive(Clone)]
pub(crate) struct SessionState {
    pub manager: Arc<SessionManager>,
    pub via_lan: bool,
    pub rate_limiter: Option<Arc<SignInLimiter>>,
}

/// `POST /api/session`: exchange a bootstrap token for a session cookie.
pub(crate) async fn create_session(
    State(state): State<SessionState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SessionRequest>,
) -> Response {
    if let Some(limiter) = &state.rate_limiter {
        if !limiter.check(addr.ip()) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many sign-in attempts from this address. Try again in a minute.",
            )
                .into_response();
        }
    }
    match state.manager.exchange(body.token.trim()) {
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
                    via_lan: state.via_lan,
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
pub(crate) async fn session_status(State(state): State<SessionState>, headers: HeaderMap) -> Response {
    let csrf = cookie_value(&headers, SESSION_COOKIE).and_then(|id| state.manager.validate(id));
    Json(SessionInfo {
        authenticated: csrf.is_some(),
        csrf,
        via_lan: state.via_lan,
    })
    .into_response()
}

/// `DELETE /api/session`: revoke the current session and clear the cookie. Passes
/// the auth gate (needs a live session + CSRF), so it only ever revokes the
/// caller's own session. Clearing the cookie is unconditional, so a
/// double-logout still leaves the browser clean.
pub(crate) async fn revoke_session(State(state): State<SessionState>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie_value(&headers, SESSION_COOKIE) {
        state.manager.revoke(id);
    }
    let clear = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    (
        [(SET_COOKIE, clear)],
        Json(SessionInfo {
            authenticated: false,
            csrf: None,
            via_lan: state.via_lan,
        }),
    )
        .into_response()
}
