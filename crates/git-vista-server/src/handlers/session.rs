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
//! Every response also carries `hook_policy` (M1.13a, #66, ADR 0025) —
//! `Restricted` when `via_lan` is true, `Allow` otherwise. **That mapping is
//! now stale**: `HookPolicy` widened to the four sandbox tier names in M1.13b
//! Task 16 and `via_lan` no longer selects a tier at all. See
//! [`hook_policy_for`]'s own doc comment for what is true instead, why the
//! correction is blocked, and why plan Task 16.5's proposed replacement is
//! itself wrong. The paragraph below records ADR 0025's original reasoning,
//! which is history now rather than current behaviour. `via_lan` is the
//! closest **existing** session distinction to `SECURITY_MODEL.md:236`'s
//! "Team mode should default to restricted" — a LAN-view session already
//! carries reduced trust (single-use bootstrap token, rate-limited,
//! read-scoped by the router's own absent write routes). This is a
//! deliberate stand-in, not a real implementation of "Team mode" (which
//! does not exist in this codebase yet — see ADR 0025); when Team mode is
//! actually built, its own default plugs into the same [`HookPolicy`] type
//! and this mapping does not need to change.
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

/// The hook policy a session discloses (M1.13a, #66, ADR 0025) — see this
/// module's own doc comment for why `via_lan` is the chosen stand-in.
///
/// # STALE, and knowingly left so — M1.13b Task 16.5 is blocked
///
/// This mapping is ADR 0025's, unchanged, and it is **no longer true**. It is
/// spelled with `HookPolicy::{Restricted, Allow}` — the transition aliases
/// `git-vista-protocol` kept when `HookPolicy` widened to the four tier names —
/// precisely so a reader sees an un-migrated value rather than a decided one.
/// What it now puts on the wire is `"strict"` for a LAN session and
/// `"unsandboxed"` for a loopback session.
///
/// **What is actually true after Task 8.** Every git spawn funnels through
/// `sandbox::policy_for`, whose tier comes from `sandbox::tier_for(need,
/// trusted)`. `via_lan` is not one of its inputs and never reaches it: a local
/// operation on an untrusted repository is `Tier::Strict` on *both* routers, a
/// remote one is `Tier::Network` on both, and only per-repository operator
/// trust yields `Tier::Unsandboxed`. So the session-level answer for both
/// routers today is `HookPolicy::Strict`, and the loopback value here
/// over-warns (the banner shows when it need not) rather than under-warns —
/// the safe direction to be wrong in, which is the only reason leaving it is
/// tolerable at all.
///
/// **Why it was not corrected here.** Two blockers, both outside this change's
/// file ownership:
///
/// * `crate::state` has no `sandbox_verdict()`. Plan Task 16.5 reads the boot
///   probe's verdict from process state; `probe::run_at_startup` computes one
///   but nothing stores it, and `state.rs` was not in scope.
/// * `security.rs`'s `hook_policy_is_disclosed_over_the_wire_and_differs_by_router`
///   asserts this exact stale mapping through the real router. Its *premise* —
///   that the two routers disclose different policies — is what Task 8 falsified,
///   so any honest correction here turns that test red. Rewriting it belongs
///   with the correction, in one edit, not split across two lanes.
///
/// **And plan Task 16.5's own replacement is wrong too — do not paste it.** It
/// maps `Contained && via_lan` to `HookPolicy::Network`, calling that
/// "narrowed... for reduced trust." `Network` is the *weaker* tier: it is
/// Landlock + seccomp with no network namespace and outbound TCP permitted on
/// `DEFAULT_GIT_PORTS`, whereas `Strict` adds pid/net/ipc/uts/cgroup namespaces
/// and no network at all. Handing the less-trusted LAN session the tier with
/// egress inverts the intent it cites ADR 0025 for.
///
/// The correct replacement, once a `sandbox_verdict()` exists and `security.rs`
/// can be edited in the same change: `Contained` → `HookPolicy::Strict` for
/// both routers (the session *floor* — what a local operation would get),
/// leaving genuine divergence to the per-repository value
/// (`sandbox::hook_policy::hook_policy_for_repo`), which is where INV-15 says
/// disclosure actually belongs; and a non-`Contained` verdict → no session at
/// all, because the boot gate already exited the process (INV-13 / ADR 0029).
fn hook_policy_for(via_lan: bool) -> HookPolicy {
    if via_lan {
        HookPolicy::Restricted
    } else {
        HookPolicy::Allow
    }
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
                    hook_policy: hook_policy_for(state.via_lan),
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
        hook_policy: hook_policy_for(state.via_lan),
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
            hook_policy: hook_policy_for(state.via_lan),
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
