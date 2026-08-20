//! Session bootstrap and protocol handshake — `POST /api/session`,
//! `GET /api/session`, `GET /api/protocol`.
//!
//! Split out of the former monolithic `api.rs`: these three endpoints are
//! what runs before anything else — exchanging the one-time bootstrap token
//! for a session, recovering that session on reload, and checking the
//! server's protocol contract — so they share nothing endpoint-specific with
//! the rest of the client beyond the transport plumbing `super::` reaches
//! back for.

use git_vista_protocol::{ProtocolInfo, SessionInfo, SessionRequest};

use super::{
    network_error, req_get, req_post, send_read, timeout_error, with_deadline, REQUEST_TIMEOUT_MS,
};

/// Exchange a one-time bootstrap token for a session (`POST /api/session`, M1.04).
/// On success the server sets the HttpOnly session cookie and returns the CSRF
/// token; a `401` means the token was wrong or expired. The token travels in the
/// JSON body, never the URL, so it can't land in a server log.
///
/// Bounded and retried once on a network-level failure (#218) — the same
/// timeout+retry [`send_read`] gives every history read. Before this, session
/// establishment had **neither**: a single dropped or silently-dead connection
/// (the SSH-tunnel-drop shape #216/#218 exist for) parked this future forever,
/// with no error for [`establish_session`] to recover from, and no automatic
/// bump of the history reload once a session did eventually land — the graph
/// panel would sit on `SeedLoading`/`SeedError` until the user reloaded by hand.
/// The retry is safe even though the token is single-use: if the first attempt
/// actually landed server-side, the second gets an "invalid token" answer, and
/// [`establish_session`] already falls through to [`get_session`] on any
/// `post_session` failure to pick up the cookie the first attempt set.
pub async fn post_session(token: &str) -> Result<SessionInfo, String> {
    let body = SessionRequest {
        token: token.to_string(),
    };
    let attempt = || async {
        let sent = async {
            req_post("/api/session")
                .json(&body)
                .map_err(|e| e.to_string())?
                .send()
                .await
                .map_err(network_error)
        };
        with_deadline(sent, REQUEST_TIMEOUT_MS)
            .await
            .unwrap_or_else(|| Err(timeout_error()))
    };
    let resp = match attempt().await {
        Ok(resp) => resp,
        Err(_) => attempt().await?,
    };
    if resp.ok() {
        resp.json::<SessionInfo>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Report the current session state (`GET /api/session`, M1.04): whether the
/// browser's cookie still names a live session, and its CSRF token if so. Hit on
/// load (and after a failed bootstrap) so a reload recovers the session — and the
/// CSRF token writes need — without re-exchanging a token.
///
/// Routed through [`send_read`] (#218), for the same reason [`post_session`]
/// now has its own timeout+retry: on an already-bootstrapped browser (the
/// `#s=` fragment stripped after first use, so every later load skips straight
/// to this call) this bare GET *was* the entirety of session establishment,
/// with no timeout and no retry — a single dropped request here, not just in
/// the history reads, could leave `establish_session()` hanging or erroring
/// with nothing to self-heal it.
pub async fn get_session() -> Result<SessionInfo, String> {
    let url = format!("/api/session?t={}", js_sys::Date::now());
    let resp = send_read(&url).await.map_err(|e| e.to_string())?;
    resp.json::<SessionInfo>().await.map_err(|e| e.to_string())
}

/// Fetch the server's protocol contract (`GET /api/protocol`, M1.02): the
/// current protocol version and the `[min, max]` client-version window it
/// accepts. Hit at startup — and on every reload — so the app can raise an
/// "Update Required" screen instead of silently talking to an incompatible
/// server. This endpoint needs no protocol header; sending it is harmless.
pub async fn fetch_protocol() -> Result<ProtocolInfo, String> {
    let url = format!("/api/protocol?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<ProtocolInfo>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}
