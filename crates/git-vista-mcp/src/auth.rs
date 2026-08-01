//! Bootstrap-token authentication against the running git-vista-server —
//! exactly the way `gv` authenticates (M2.23a, #245).
//!
//! The flow mirrors the SPA's: read the `0600` one-time token, exchange it via
//! `POST /api/session` for an `HttpOnly` session cookie plus a CSRF token, and
//! reuse both for every later request. The token is **single-use and
//! self-replacing** — the server mints a fresh one into the same file the
//! moment one is spent — so this crate consuming a token never locks a human
//! out; it only rotates the file.
//!
//! # Where the secret is allowed to live
//!
//! The token is read from disk once and held in a local only long enough to be
//! sent; the session cookie and CSRF token live in [`Session`] fields in
//! memory. None of the three ever lands in argv, an environment variable, or
//! any file this crate writes — #245's acceptance criterion, load-bearing
//! because argv and env are world-visible on this box (`/proc/<pid>/cmdline`,
//! `/proc/<pid>/environ`) in ways a `0600` file is not.

use std::path::PathBuf;

use crate::http::{self, HttpResponse};

/// `$XDG_STATE_HOME/git-vista`, or `~/.local/state/git-vista` when unset —
/// **a deliberate mirror of `git-vista-server`'s `state::state_dir()`**, not an
/// import: #245 keeps this crate from linking the server crate, and the
/// resolution is three lines of stable XDG convention. If the server's ever
/// changes, the live integration test fails on the handshake, which is the
/// alarm we want.
fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir().join("git-vista-state"));
    base.join("git-vista")
}

/// Where the server writes the one-time bootstrap token, `0600`. Mirrors
/// `state::bootstrap_token_path()`.
pub fn bootstrap_token_path() -> PathBuf {
    state_dir().join("bootstrap.token")
}

/// An authenticated session: the cookie pair the server set, and the CSRF
/// token writes must echo. Memory only.
#[derive(Debug, Clone)]
pub struct Session {
    /// The `gv_session=<id>` pair, exactly as it must appear in a `Cookie`
    /// header. Extracted from the exchange response's `Set-Cookie`.
    pub cookie: String,
    /// The per-session CSRF token (`x-git-vista-csrf` on state-changing
    /// requests). Carried even though this slice is read-only, so the write
    /// slices (#248/#249) inherit a complete session, not a partial one.
    #[allow(dead_code)] // read by the coming write slices; kept whole per #245's scope
    pub csrf: String,
}

/// Read the current bootstrap token and exchange it for a session.
///
/// Errors are strings meant for the MCP client's eyes (they become tool-call
/// errors); none of them ever embeds the token itself.
pub fn authenticate() -> Result<Session, String> {
    let path = bootstrap_token_path();
    let token = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "could not read the bootstrap token at {}: {e}. Is git-vista-server running? \
             (it writes a fresh token there on every start)",
            path.display()
        )
    })?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!(
            "the bootstrap token file at {} is empty — the server may be mid-rotation; retry",
            path.display()
        ));
    }

    let body = serde_json::to_vec(&git_vista_protocol::SessionRequest {
        token: token.to_string(),
    })
    .map_err(|e| format!("could not encode the session request: {e}"));
    let body = body?;

    let resp = http::post_json("/api/session", &body, None, None)?;
    if resp.status != 200 {
        return Err(format!(
            "POST /api/session answered {} — the token may have expired (the server \
             refreshes it periodically; re-run, or restart via its service if this persists): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        ));
    }

    let info: git_vista_protocol::SessionInfo = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("could not parse the session response: {e}"))?;
    let csrf = info
        .csrf
        .ok_or("the server authenticated us but returned no CSRF token — protocol drift?")?;
    let cookie = session_cookie_pair(&resp)
        .ok_or("the server authenticated us but set no gv_session cookie — protocol drift?")?;

    Ok(Session { cookie, csrf })
}

/// Pull the `gv_session=<value>` pair out of the exchange response's
/// `Set-Cookie` header, dropping the attributes (`HttpOnly; SameSite=Strict;
/// Path=/` and friends) that belong to the response, not to our next request.
fn session_cookie_pair(resp: &HttpResponse) -> Option<String> {
    let set_cookie = resp.header("set-cookie")?;
    let pair = set_cookie.split(';').next()?.trim();
    if pair.starts_with("gv_session=") && pair.len() > "gv_session=".len() {
        Some(pair.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The XDG mirror must prefer `$XDG_STATE_HOME` and fall back to
    /// `~/.local/state` — the same resolution the server uses. Env-var tests
    /// mutate process state, so both cases run in one test to avoid a race
    /// between parallel test threads.
    #[test]
    fn token_path_follows_the_servers_xdg_resolution() {
        // Explicit XDG_STATE_HOME wins.
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-test-state");
        assert_eq!(
            bootstrap_token_path(),
            PathBuf::from("/tmp/xdg-test-state/git-vista/bootstrap.token")
        );
        // Empty XDG_STATE_HOME falls back to ~/.local/state, like the server's
        // `.filter(|p| !p.as_os_str().is_empty())`.
        std::env::set_var("XDG_STATE_HOME", "");
        std::env::set_var("HOME", "/tmp/home-test");
        assert_eq!(
            bootstrap_token_path(),
            PathBuf::from("/tmp/home-test/.local/state/git-vista/bootstrap.token")
        );
        std::env::remove_var("XDG_STATE_HOME");
    }

    #[test]
    fn the_session_cookie_pair_is_extracted_without_its_attributes() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![(
                "set-cookie".into(),
                "gv_session=abc123; HttpOnly; SameSite=Strict; Path=/".into(),
            )],
            body: Vec::new(),
        };
        assert_eq!(
            session_cookie_pair(&resp).as_deref(),
            Some("gv_session=abc123")
        );
    }

    #[test]
    fn a_response_without_the_gv_cookie_yields_none_rather_than_junk() {
        let none = HttpResponse {
            status: 200,
            headers: vec![("set-cookie".into(), "other=1; Path=/".into())],
            body: Vec::new(),
        };
        assert_eq!(session_cookie_pair(&none), None);
        let empty = HttpResponse {
            status: 200,
            headers: vec![("set-cookie".into(), "gv_session=".into())],
            body: Vec::new(),
        };
        assert_eq!(session_cookie_pair(&empty), None);
    }
}
