//! The persistent-client loop: lazy first authentication plus exactly one
//! re-authenticate-and-retry on `401` (M10.02, #457 — lifted here from
//! `git-vista-mcp::tools`, where M2.23b, #246 built and proved it).
//!
//! # What earned the move
//!
//! A one-shot process re-authenticates on its next run, so it needs none of
//! this — which is why #456 (ADR 0101) deliberately left these two helpers
//! behind in the MCP crate when `auth.rs` and `http.rs` moved. The rule was
//! explicit: they lift when a *second* long-lived consumer exists, so the
//! seam has a caller on each side to keep it honest. `gv-tui`'s event loop
//! (phase 2a of #457) is that consumer: a terminal session that stays open
//! across a server restart must survive the `401` the restart produces the
//! same way the MCP bridge does — re-authenticate once with the
//! self-replacing bootstrap token, retry once with the NEW cookie, and give
//! up honestly on a second `401` rather than looping. ADR 0102 records the
//! decision; ADR 0101 records why it waited.
//!
//! # The three legs, and why they are injected
//!
//! Both helpers are generic over their fetch/post and auth closures so every
//! leg — lazy first auth, `401` → re-auth → retry with the fresh cookie,
//! `401` → `401` giving up — is unit-testable with no server. Production
//! passes [`crate::http::get`] / [`crate::http::post_json`] and
//! [`crate::auth::authenticate`]. The trap the retry tests exist for: a
//! retry that resent the STALE cookie would loop `401` forever in
//! production while looking superficially like a retry.
//!
//! # What a failure string may carry
//!
//! Path, status, and the *server's own* body — never the session. Every
//! error these helpers build reaches a human (an MCP host's log, `gv-tui`'s
//! status line), so the cookie and CSRF token must not be able to reach it;
//! `tests::a_failed_request_never_leaks_the_session_cookie_or_csrf_into_its_error`
//! pins that on both legs.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Session;
    use crate::http::HttpResponse;

    fn session(cookie: &str) -> Session {
        Session {
            cookie: cookie.to_string(),
            csrf: "csrf".to_string(),
        }
    }

    fn resp(status: u16, body: &[u8]) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn the_first_call_authenticates_lazily_and_sends_that_cookie() {
        let mut sess = None;
        let mut seen = Vec::new();
        let body = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, cookie| {
                seen.push(cookie.to_string());
                Ok(resp(200, b"ok"))
            },
            &mut || Ok(session("gv_session=first")),
        )
        .unwrap();
        assert_eq!(body, b"ok");
        assert_eq!(seen, ["gv_session=first"]);
        assert_eq!(sess.unwrap().cookie, "gv_session=first");
    }

    #[test]
    fn a_401_reauthenticates_once_and_retries_with_the_new_cookie() {
        // The trap this test exists for: a retry that resends the STALE
        // cookie would loop 401 forever in production while looking
        // superficially like a retry.
        let mut sess = Some(session("gv_session=stale"));
        let mut seen = Vec::new();
        let body = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, cookie| {
                seen.push(cookie.to_string());
                if cookie == "gv_session=stale" {
                    Ok(resp(401, b""))
                } else {
                    Ok(resp(200, b"fresh"))
                }
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"fresh");
        assert_eq!(seen, ["gv_session=stale", "gv_session=fresh"]);
    }

    #[test]
    fn a_second_401_gives_up_rather_than_retrying_forever() {
        let mut sess = Some(session("gv_session=stale"));
        let mut fetches = 0;
        let err = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, _| {
                fetches += 1;
                Ok(resp(401, b"no"))
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap_err();
        assert_eq!(fetches, 2, "exactly one retry, never a loop");
        assert!(err.contains("even after re-authenticating"));
    }

    #[test]
    fn authed_post_sends_the_sessions_csrf_token_alongside_its_cookie() {
        let mut sess = Some(session("gv_session=live"));
        let mut seen: Vec<(String, String)> = Vec::new();
        let body = authed_post(
            "/api/select",
            b"{}",
            &mut sess,
            &mut |_, _, cookie, csrf| {
                seen.push((cookie.to_string(), csrf.to_string()));
                Ok(resp(200, b"Selected."))
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"Selected.");
        assert_eq!(seen, [("gv_session=live".to_string(), "csrf".to_string())]);
    }

    /// Every failure string these two helpers build reaches a human — an
    /// MCP host's log, `gv-tui`'s status line — so neither may carry the
    /// live cookie or CSRF token. The error is assembled from the path, the
    /// status and the *server's* body — never from the session — and this
    /// pins that, in both the first-response and the retried-after-401 legs.
    #[test]
    fn a_failed_request_never_leaks_the_session_cookie_or_csrf_into_its_error() {
        const COOKIE: &str = "gv_session=CookieSecretABCDEF";
        const CSRF: &str = "CsrfSecret123456";
        let secret_session = || Session {
            cookie: COOKIE.to_string(),
            csrf: CSRF.to_string(),
        };

        let mut sess = Some(secret_session());
        let get_err = authed_fetch(
            "/api/status/v2",
            &mut sess,
            &mut |_, _| Ok(resp(500, b"the server said no")),
            &mut || panic!("500 is not 401 — no re-authentication"),
        )
        .unwrap_err();

        let mut sess = Some(secret_session());
        let post_err = authed_post(
            "/api/plan",
            b"{}",
            &mut sess,
            &mut |_, _, _, _| Ok(resp(401, b"stale")),
            &mut || Ok(secret_session()),
        )
        .unwrap_err();

        for err in [&get_err, &post_err] {
            // Anti-vacuity first: these really are the messages a client sees,
            // carrying the server's own words — not empty strings that would
            // pass the leak check for the wrong reason.
            assert!(err.contains("/api/"), "{err}");
            assert!(!err.contains("CookieSecretABCDEF"), "cookie leaked: {err}");
            assert!(!err.contains("CsrfSecret123456"), "csrf leaked: {err}");
        }
        assert!(get_err.contains("the server said no"), "{get_err}");
        assert!(
            post_err.contains("even after re-authenticating"),
            "{post_err}"
        );
    }

    #[test]
    fn authed_post_reauthenticates_once_on_401_like_authed_fetch() {
        let mut sess = Some(session("gv_session=stale"));
        let mut seen = Vec::new();
        let body = authed_post(
            "/api/select",
            b"{}",
            &mut sess,
            &mut |_, _, cookie, _csrf| {
                seen.push(cookie.to_string());
                if cookie == "gv_session=stale" {
                    Ok(resp(401, b""))
                } else {
                    Ok(resp(200, b"Selected."))
                }
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"Selected.");
        assert_eq!(seen, ["gv_session=stale", "gv_session=fresh"]);
    }
}
