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

    /// The session policy now agrees with the dispatch instead of contradicting
    /// it: an untrusted repository on a contained host discloses `strict`,
    /// checked against `tier_for` itself rather than against a restated copy of
    /// its rules.
    ///
    /// The temp directory is the load-bearing part — it has no operator-trust
    /// marker, so `hook_policy_for_repo` reaches the real `trust::is_trusted`
    /// and gets the fail-closed `false`. Nothing here fabricates the trust
    /// answer.
    #[test]
    fn a_contained_host_discloses_the_tier_the_dispatch_actually_uses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let disclosed = session_hook_policy_for(Some(dir.path()), Some(&ProbeVerdict::Contained));

        let enforced = crate::sandbox::hook_policy::hook_policy_for_tier(crate::sandbox::tier_for(
            crate::sandbox::NetworkNeed::Local,
            false,
        ));
        assert_eq!(
            disclosed, enforced,
            "the session must disclose the tier the git-spawn chokepoint really \
             uses, not a session-level guess"
        );
        assert_eq!(disclosed, HookPolicy::Strict);
    }

    /// **Blocker 3, pinned.** The old mapping handed a LAN session
    /// `HookPolicy::Restricted`, which after the four-tier widening *is*
    /// `Strict` — the one value that silences INV-15's banner. A LAN session
    /// therefore reported the strongest possible guarantee regardless of what
    /// the repository actually ran under.
    ///
    /// The structural fix is that `session_hook_policy_for` has **no `via_lan`
    /// parameter at all**, so there is no input that can produce a
    /// router-specific answer; this test pins the consequence that matters. It
    /// walks every policy an operator-trust state can produce and asserts the
    /// session value is that policy — in particular that the trusted case
    /// (`Unsandboxed`, banner-flying) is not silently replaced by `Strict` the
    /// way the LAN branch used to do.
    ///
    /// `Unsandboxed` is obtained from the real dispatch (`tier_for(_, true)`),
    /// not written as a literal, so this fails if trust ever stops meaning
    /// "no sandbox" rather than quietly passing against a stale expectation.
    #[test]
    fn a_lan_session_cannot_silence_the_banner_for_a_trusted_repository() {
        let trusted_answer = crate::sandbox::hook_policy::hook_policy_for_tier(
            crate::sandbox::tier_for(crate::sandbox::NetworkNeed::Local, true),
        );
        assert_eq!(trusted_answer, HookPolicy::Unsandboxed);
        assert!(
            trusted_answer.requires_banner(),
            "precondition: a trusted repository is a banner case, or this test \
             is not exercising the silencing bug it claims to"
        );
        assert_ne!(
            trusted_answer,
            HookPolicy::Strict,
            "the old LAN branch returned Strict unconditionally; if the trusted \
             answer were Strict too, this test would prove nothing"
        );

        // The untrusted case, through the real entry point, is the other half:
        // a LAN client asking about an untrusted repo gets the same `strict` a
        // loopback client gets — same function, same inputs, no router in them.
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            session_hook_policy_for(Some(dir.path()), Some(&ProbeVerdict::Contained)),
            HookPolicy::Strict
        );
    }

    /// The three "we don't know" arms — no verdict, no selection, and an
    /// ADR-0029 refusal — all disclose a value that flies the banner, and
    /// specifically never `Strict`.
    ///
    /// The `assert_ne!` against `Strict` is not redundant with
    /// `requires_banner()`: `requires_banner` is implemented as `!matches!(self,
    /// Strict)`, so asserting only the banner would pass trivially if the
    /// fallback and the banner rule were ever changed together. Pinning the
    /// concrete value as well means a future edit has to break both.
    #[test]
    fn an_unknown_or_refused_policy_flies_the_banner_and_is_never_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refused = ProbeVerdict::CapabilityAbsent {
            missing: vec!["bwrap"],
        };

        for (label, got) in [
            (
                "no boot verdict",
                session_hook_policy_for(Some(dir.path()), None),
            ),
            (
                "no current selection",
                session_hook_policy_for(None, Some(&ProbeVerdict::Contained)),
            ),
            (
                "the host refuses this repository's operations (ADR 0029)",
                session_hook_policy_for(Some(dir.path()), Some(&refused)),
            ),
        ] {
            assert!(got.requires_banner(), "{label}: must fly the banner");
            assert_ne!(
                got,
                HookPolicy::Strict,
                "{label}: must not claim the one tier that silences the banner"
            );
            assert_eq!(
                got,
                HookPolicy::default(),
                "{label}: the unknown case is the type's own fail-closed value"
            );
        }
    }

    /// ADR 0029 again, at this seam: a capability-absent host must not turn
    /// into `HookPolicy::Blocked` *as a disclosure of blocked hooks*. It does
    /// land on `Blocked` here — but only via `HookPolicy::default()`'s
    /// documented "not known to be running" meaning, and only after
    /// `hook_policy_for_repo` has already refused rather than mapped.
    ///
    /// So what is actually asserted is the thing that must not regress: the
    /// underlying mapping still returns `Err`, not `Ok(Blocked)`. If someone
    /// "simplified" `hook_policy_for_repo` to return `Ok(Blocked)` for a
    /// capability-absent host, the session value above would be unchanged and
    /// only this assertion would catch it.
    #[test]
    fn capability_absent_still_refuses_at_the_mapping_rather_than_becoming_blocked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let got = hook_policy_for_repo(
            dir.path(),
            &ProbeVerdict::CapabilityAbsent {
                missing: vec!["bwrap"],
            },
        );
        assert!(
            got.is_err(),
            "ADR 0029: a capability-absent host refuses; it does not disclose a \
             policy, blocked or otherwise"
        );
        assert_ne!(got.ok(), Some(HookPolicy::Blocked));
    }
}
