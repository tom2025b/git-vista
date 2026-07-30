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
//! Every response also carries `hook_policy` (M1.13a, #66, ADR 0025; corrected
//! by #202). It is now the **measured** policy for the current selection — see
//! [`session_hook_policy_for`] — not a function of `via_lan`. ADR 0025's
//! original `via_lan → Restricted/Allow` mapping was a stand-in adopted when the
//! server had no real hook policy to report at all; M1.13b gave it one, and that
//! stand-in became a wrong answer rather than a placeholder. It is gone.
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

use git_vista_protocol::{HookPolicy, SessionInfo, SessionRequest};

use crate::ratelimit::SignInLimiter;
use crate::sandbox::hook_policy::hook_policy_for_repo;
use crate::sandbox::probe::ProbeVerdict;
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

/// The hook policy a session discloses: the **real** per-repository policy for
/// the current selection, measured the same way the git-spawn chokepoint
/// measures it.
///
/// # What replaced `hook_policy_for(via_lan)`, and why it had to go
///
/// The previous mapping was ADR 0025's: `Restricted` for a LAN session, `Allow`
/// for a loopback one. After `HookPolicy` widened to the sandbox's own tier
/// names those two constants resolved to `Strict` and `Unsandboxed`, which made
/// the LAN branch actively harmful rather than merely stale:
/// [`HookPolicy::Strict`] is the **one** variant that silences INV-15's banner
/// ([`HookPolicy::requires_banner`]). So the least-trusted session shape — the
/// one reached from another machine on the network — was the only one
/// guaranteed to show the user nothing, no matter what policy its repository
/// actually ran under. A repository the operator had explicitly trusted to run
/// **unsandboxed** disclosed "strict" to a LAN client. That is precisely the
/// failure INV-15 names: a policy computed but not disclosed is worse than none,
/// because it manufactures the appearance of a safety property.
///
/// # What a LAN session should disclose
///
/// The same thing every other session discloses, and the reasoning is short.
/// Disclosure reports what enforcement *does*; it is not a lever for expressing
/// what we wish enforcement did. `via_lan` is not an input to
/// `sandbox::tier_for` and never reaches `sandbox::policy_for` — a local
/// operation on an untrusted repository is `Tier::Strict` on both listeners, a
/// remote one is `Tier::Network` on both, and only per-repository operator trust
/// yields `Tier::Unsandboxed`. Two listeners, one dispatch. Any router-dependent
/// answer here would therefore be a claim about a distinction the enforcement
/// path does not make.
///
/// The intuition behind ADR 0025's stand-in — "the less-trusted session should
/// see the more conservative value" — is not wrong, it is *misplaced*: it is an
/// argument for LAN sessions getting a stricter **tier**, which is a change to
/// `tier_for` (and a future Team-mode ADR), not a change to what we report.
/// Reporting a stricter tier than is enforced would be the same lie in the
/// opposite direction, and — as above — it lands on the exact value that
/// silences the warning. If reduced LAN trust should mean something, it must
/// mean it in `sandbox::tier_for` first, and this function will then report it
/// for free.
///
/// Note the direction the fix moves the banner: a LAN session viewing a trusted
/// repository now *gains* a banner it never had. That is the point of the fix,
/// not a side effect of it.
///
/// # Known gap: this is a snapshot of the current selection
///
/// `POST /api/select` can move the selection without the client re-fetching
/// `/api/session`, so this value can go stale mid-session. Named rather than
/// papered over. The non-stale, per-repository disclosure is
/// [`git_vista_protocol::RepositoryDescriptor::hook_policy`], which the client
/// refetches with the catalog and which INV-15 treats as the authoritative
/// per-repository answer; this session-level field is the coarse "what am I
/// looking at right now" signal the persistent banner keys on.
fn session_hook_policy() -> HookPolicy {
    session_hook_policy_for(
        crate::state::current_path_if_set().as_deref(),
        crate::sandbox::probe::boot_verdict(),
    )
}

/// [`session_hook_policy`] with both process-globals hoisted into parameters,
/// so the mapping is testable without a booted server or a process-wide
/// selection.
///
/// **It takes no `via_lan`, and that absence is the fix**: there is no input by
/// which a LAN session can be handed a different — in particular, a
/// banner-silencing — answer than a loopback one.
///
/// # Both `None` arms, and the refusal, fold to `HookPolicy::default()`
///
/// [`HookPolicy::default`] is [`HookPolicy::Blocked`], documented in the
/// protocol crate as the value meaning *"hooks are not known to be running"* —
/// which is exactly the state here: no verdict measured, no repository
/// selected, or the host refusing the operation outright (INV-13 / ADR 0029).
/// It flies the banner, which is the only property that must hold. Deliberately
/// **not** [`HookPolicy::Strict`]: that is the one value that claims a guarantee
/// *and* goes silent, so defaulting to it would turn "we don't know" into an
/// unearned green light — the same mistake the old LAN branch made.
///
/// None of these arms is reachable in a live server: `main` gates boot on the
/// probe and sets the selection before any listener binds. They exist because a
/// total function with an honest worst case is better than an `expect()` on a
/// network-reachable path.
fn session_hook_policy_for(
    repo: Option<&std::path::Path>,
    verdict: Option<&ProbeVerdict>,
) -> HookPolicy {
    let (Some(repo), Some(verdict)) = (repo, verdict) else {
        return HookPolicy::default();
    };
    hook_policy_for_repo(repo, verdict).unwrap_or_else(|refused| {
        eprintln!("git-vista: session discloses no hook policy — {refused}");
        HookPolicy::default()
    })
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
                    hook_policy: session_hook_policy(),
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
    State(state): State<SessionState>,
    headers: HeaderMap,
) -> Response {
    let csrf = cookie_value(&headers, SESSION_COOKIE).and_then(|id| state.manager.validate(id));
    Json(SessionInfo {
        authenticated: csrf.is_some(),
        csrf,
        via_lan: state.via_lan,
        hook_policy: session_hook_policy(),
    })
    .into_response()
}

/// `DELETE /api/session`: revoke the current session and clear the cookie. Passes
/// the auth gate (needs a live session + CSRF), so it only ever revokes the
/// caller's own session. Clearing the cookie is unconditional, so a
/// double-logout still leaves the browser clean.
pub(crate) async fn revoke_session(
    State(state): State<SessionState>,
    headers: HeaderMap,
) -> Response {
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
            hook_policy: session_hook_policy(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `via_lan` stand-in this module's own doc comment argues for —
    /// pinned as a real test, not just prose, so a future change to the
    /// mapping is a deliberate act, not an accidental drift.
    #[test]
    fn hook_policy_defaults_restricted_on_lan_allow_otherwise() {
        assert_eq!(hook_policy_for(true), HookPolicy::Restricted);
        assert_eq!(hook_policy_for(false), HookPolicy::Allow);
    }

    /// What the stale mapping above now actually emits, spelled out in the
    /// four-variant vocabulary rather than left behind the transition aliases.
    ///
    /// This test exists to make the staleness *visible in a test run* instead
    /// of only in a doc comment: a loopback session claims `unsandboxed` while
    /// `sandbox::tier_for(NetworkNeed::Local, false)` gives it `Tier::Strict`.
    /// Deleting this test is part of Task 16.5's correction, not a way to
    /// quiet it.
    #[test]
    fn the_session_policy_is_stale_and_over_warns_rather_than_under_warns() {
        assert_eq!(hook_policy_for(false), HookPolicy::Unsandboxed);
        assert_eq!(hook_policy_for(true), HookPolicy::Strict);

        // The tier a local operation on an untrusted repository really runs
        // in, on either router — read from the dispatch itself, not restated.
        let enforced = crate::sandbox::hook_policy::hook_policy_for_tier(crate::sandbox::tier_for(
            crate::sandbox::NetworkNeed::Local,
            false,
        ));
        assert_eq!(enforced, HookPolicy::Strict);
        assert_ne!(
            hook_policy_for(false),
            enforced,
            "if these now agree, Task 16.5 has landed — delete this test and the \
             staleness note on `hook_policy_for`"
        );

        // The one property that makes leaving it stale tolerable: it errs
        // toward showing the banner, never toward silencing it.
        assert!(
            hook_policy_for(false).requires_banner(),
            "a stale session policy must never be the one variant that silences \
             the banner"
        );
    }
}
