# LAN View Mode (feature ③, #122) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `gv --lan-view [path]` starts the existing loopback server plus a second,
LAN-facing listener that is structurally read-only — a backup path to the iPad
when the SSH tunnel is unavailable, without reintroducing the plain-HTTP LAN
write surface M1.05 deleted.

**Architecture:** One process, two `axum::serve` tasks sharing one `SessionManager`
and one repository `CURRENT`/`CATALOG`. The loopback listener keeps today's full
router. The LAN listener gets a second router built from the *same* handler
functions but with every write/select/rescan/clone route left unregistered
(structural absence, not a mode check), a `HostPolicy` pinned to the one
sanctioned LAN IP:port (no `localhost` alias), and a per-IP rate limiter on
`POST /api/session`. `gv` gains `--lan-view`/`--lan-ip`, auto-detects the LAN IP
when exactly one candidate exists, and teaches `doctor`/the exposed-listener
kill-check to expect the second sanctioned socket instead of treating it as a
security breach.

**Tech Stack:** Rust (axum 0.8.9, tokio), Leptos/wasm frontend, bash (`gv`).

## Global Constraints

- `0.0.0.0` is never accepted as a bind address, on either listener.
- The LAN router registers **no** write/select/rescan/clone route — absence, not
  a gate. (ADR 0005 alternatives explicitly rejects gating a single shared
  router: "a check can regress; an unregistered route cannot.")
- Plain `gv --lan` (no `-view`) remains a hard rejection, unchanged from `ae28093`.
- Sign-in on the LAN listener is rate-limited; loopback sign-in stays unlimited
  (unchanged behavior).
- Governance: ADR 0005 (already Accepted, implementation pending) flips to
  "implemented" at the end; SECURITY_MODEL.md gets an amendment distinguishing
  this profile from the existing aspirational HTTPS-paired "LAN Mode" section.
- **Two-account rule:** issue #122 is already assigned to `tom2025b` for this
  session (done before this plan was written).
- **Server-restart rule (project memory):** every live-verification step in
  Task 7 that requires restarting the running `git-vista-server` needs Tom's
  explicit go-ahead first — port 8080 may be his live iPad session. Ask before
  restarting; never do it silently.
- Never delete branches. `./dev gate` green before every commit that claims it.

---

### Task 1: LAN bind-address resolution

**Files:**
- Modify: `crates/git-vista-server/src/state.rs`

**Interfaces:**
- Produces: `pub(crate) fn lan_bind_addr() -> Option<Result<SocketAddr, String>>`
  — `None` when `GIT_VISTA_LAN_IP` is unset/empty (feature off); `Some(Ok(addr))`
  when it's a valid, non-loopback, non-unspecified IP on the fixed `PORT`;
  `Some(Err(reason))` otherwise. Consumed by `main.rs` in Task 4.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `state.rs` (near the existing
`bind_address_*` tests):

```rust
    #[test]
    fn lan_ip_is_none_when_unset() {
        assert!(parse_lan_ip_env(None).is_none());
    }

    #[test]
    fn lan_ip_is_none_when_empty() {
        assert!(parse_lan_ip_env(Some("")).is_none());
    }

    #[test]
    fn lan_ip_accepts_an_explicit_lan_address() {
        let addr = parse_lan_ip_env(Some("192.168.1.42")).unwrap().unwrap();
        assert_eq!(addr, SocketAddr::new("192.168.1.42".parse().unwrap(), PORT));
    }

    #[test]
    fn lan_ip_rejects_loopback() {
        let error = parse_lan_ip_env(Some("127.0.0.1")).unwrap().unwrap_err();
        assert!(error.contains("loopback"));
    }

    #[test]
    fn lan_ip_rejects_unspecified() {
        let error = parse_lan_ip_env(Some("0.0.0.0")).unwrap().unwrap_err();
        assert!(error.contains("0.0.0.0"));
    }

    #[test]
    fn lan_ip_rejects_invalid_input() {
        let error = parse_lan_ip_env(Some("not-an-address")).unwrap().unwrap_err();
        assert!(error.contains("invalid GIT_VISTA_LAN_IP"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p git-vista-server lan_ip -- --nocapture`
Expected: FAIL — `parse_lan_ip_env` not defined.

- [ ] **Step 3: Implement**

Add to `state.rs`, directly below `parse_bind_addr`/`bind_addr`:

```rust
/// The optional second, LAN-facing listener (ADR 0005, `gv --lan-view`). `None`
/// means the feature isn't requested — the server then behaves exactly as
/// before this feature landed. `gv` is responsible for auto-detecting the LAN
/// IP or requiring `--lan-ip` before it ever sets this variable, so a parse
/// failure here means the launcher passed something bad — still handled as a
/// clean startup error, never a panic.
pub(crate) fn lan_bind_addr() -> Option<Result<SocketAddr, String>> {
    parse_lan_ip_env(std::env::var("GIT_VISTA_LAN_IP").ok().as_deref())
}

/// The pure resolution behind [`lan_bind_addr`], parameterised so tests never
/// read or write process env — the same pattern as `parse_bind_addr`. An empty
/// value counts as unset, matching `resolve_clones_root`'s convention (a
/// systemd unit with `Environment=X=` must not silently enable the feature).
fn parse_lan_ip_env(value: Option<&str>) -> Option<Result<SocketAddr, String>> {
    let value = value.filter(|v| !v.trim().is_empty())?;
    let ip: IpAddr = match value.trim().parse() {
        Ok(ip) => ip,
        Err(error) => return Some(Err(format!("invalid GIT_VISTA_LAN_IP '{value}': {error}"))),
    };
    if ip.is_loopback() {
        return Some(Err(format!(
            "refusing GIT_VISTA_LAN_IP '{value}': that is a loopback address, not a LAN interface"
        )));
    }
    if ip.is_unspecified() {
        return Some(Err(format!(
            "refusing GIT_VISTA_LAN_IP '{value}': 0.0.0.0 is never accepted — pass one explicit interface address"
        )));
    }
    Some(Ok(SocketAddr::new(ip, PORT)))
}
```

`IpAddr` is already reachable via the existing `use std::net::{IpAddr, Ipv4Addr, SocketAddr};` at the top of the file.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p git-vista-server lan_ip -- --nocapture`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista-server/src/state.rs
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "feat(server): LAN bind-address resolution (ADR 0005)"
```

---

### Task 2: `HostPolicy` for the LAN listener

**Files:**
- Modify: `crates/git-vista-server/src/security.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub(crate) fn HostPolicy::lan(ip: IpAddr, port: u16) -> HostPolicy`.
  Consumed by `main.rs` in Task 4.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `security.rs`:

```rust
    #[test]
    fn lan_host_pins_to_the_exact_ip_and_port() {
        let p = HostPolicy::lan("192.168.1.42".parse().unwrap(), 8080);
        assert!(p.host_allowed("192.168.1.42:8080"));
        assert!(!p.host_allowed("192.168.1.42:9999")); // wrong port
        assert!(!p.host_allowed("192.168.1.99:8080")); // different LAN ip
        assert!(!p.host_allowed("localhost:8080")); // loopback names refused here
        assert!(!p.host_allowed("127.0.0.1:8080"));
        assert!(!p.host_allowed("evil.example.com"));
    }

    #[test]
    fn lan_origin_must_match_the_pinned_ip_and_not_be_null() {
        let p = HostPolicy::lan("192.168.1.42".parse().unwrap(), 8080);
        assert!(p.origin_allowed("http://192.168.1.42:8080"));
        assert!(!p.origin_allowed("null"));
        assert!(!p.origin_allowed("http://localhost:8080"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p git-vista-server lan_host -- --nocapture`
Expected: FAIL — `HostPolicy::lan` not defined.

- [ ] **Step 3: Implement**

Change the `HostPolicy` struct and its impls in `security.rs`:

```rust
#[derive(Clone)]
pub(crate) struct HostPolicy {
    /// The port the service is bound to; a `Host`/`Origin` naming a different port
    /// is rejected.
    port: u16,
    /// `None` (loopback listener): only the loopback name literals pass. `Some(ip)`
    /// (LAN listener, ADR 0005): only that exact IP literal passes — not
    /// `localhost`, not any other address the machine might also answer on.
    pinned_ip: Option<IpAddr>,
}

impl HostPolicy {
    /// Create the strict loopback policy for the listener's fixed port.
    pub(crate) fn loopback(port: u16) -> Self {
        Self { port, pinned_ip: None }
    }

    /// Create the policy for the LAN listener (ADR 0005): only the one
    /// sanctioned LAN IP at the fixed port is an acceptable Host. Narrower than
    /// [`Self::loopback`] on purpose — nothing routes to this listener except a
    /// request that already knows the exact sanctioned socket.
    pub(crate) fn lan(ip: IpAddr, port: u16) -> Self {
        Self { port, pinned_ip: Some(ip) }
    }

    /// Whether a raw `Host` header value is acceptable. The host must match this
    /// policy's identity (loopback literal, or the one pinned LAN IP) and any
    /// supplied port must match the bind port.
    fn host_allowed(&self, host: &str) -> bool {
        let (name, port) = split_host_port(host);
        if let Some(port) = port {
            if port != self.port {
                return false;
            }
        }
        match self.pinned_ip {
            Some(ip) => name.parse::<IpAddr>().map(|parsed| parsed == ip).unwrap_or(false),
            None => is_loopback_name(name),
        }
    }

    // origin_allowed is unchanged — it already delegates to host_allowed.
```

Add `use std::net::IpAddr;` to `security.rs`'s imports.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p git-vista-server -- --nocapture`
Expected: PASS — all existing `security.rs` tests plus the two new ones (the
existing `loopback()` tests must still pass unchanged, since `pinned_ip: None`
reproduces today's behavior exactly).

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista-server/src/security.rs
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "feat(server): pin the LAN listener's Host policy to one explicit IP (ADR 0005)"
```

---

### Task 3: `via_lan` session flag + LAN sign-in rate limiter

**Files:**
- Modify: `crates/git-vista-protocol/src/dto.rs` (`SessionInfo`)
- Create: `crates/git-vista-server/src/ratelimit.rs`
- Modify: `crates/git-vista-server/src/main.rs` (add `mod ratelimit;`)
- Modify: `crates/git-vista-server/src/handlers/session.rs`
- Modify: `crates/git-vista-server/src/security.rs` (wire-test fixtures only)

**Interfaces:**
- Consumes: nothing new from earlier tasks.
- Produces: `SessionInfo.via_lan: bool` (wire field); `pub(crate) struct
  SessionState { manager: Arc<SessionManager>, via_lan: bool, rate_limiter:
  Option<Arc<ratelimit::SignInLimiter>> }` and `SignInLimiter::new()` /
  `.check(IpAddr) -> bool`. Both consumed by `main.rs` in Task 4 and by the
  frontend in Task 5 (`SessionInfo.via_lan`).

- [ ] **Step 1: Write the failing tests**

`crates/git-vista-server/src/ratelimit.rs` (new file):

```rust
//! A minimal fixed-window rate limiter for LAN sign-in attempts (ADR 0005).
//! SECURITY_MODEL.md requires rate-limiting for any beyond-loopback exposure.
//! Wired only into the LAN listener's `create_session` handler — loopback
//! sign-in is unaffected, matching today's behavior.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many sign-in attempts one source IP gets per window before a `429`.
const MAX_ATTEMPTS: u32 = 5;
/// The fixed window's length. Resets fully once elapsed rather than sliding —
/// this only needs to blunt brute-forcing a stolen/guessed bootstrap token, not
/// meter traffic precisely.
const WINDOW: Duration = Duration::from_secs(60);

struct Bucket {
    count: u32,
    window_started: Instant,
}

/// Per-source-IP sign-in attempt counter, shared by every request the LAN
/// listener's `create_session` handler serves.
pub(crate) struct SignInLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl SignInLimiter {
    pub(crate) fn new() -> Self {
        Self { buckets: Mutex::new(HashMap::new()) }
    }

    /// Record one attempt from `addr`; `true` = allowed, `false` = rate-limited.
    pub(crate) fn check(&self, addr: IpAddr) -> bool {
        let mut buckets = self.buckets.lock().expect("ratelimit lock");
        let now = Instant::now();
        let bucket = buckets
            .entry(addr)
            .or_insert_with(|| Bucket { count: 0, window_started: now });
        if now.duration_since(bucket.window_started) >= WINDOW {
            bucket.count = 0;
            bucket.window_started = now;
        }
        bucket.count += 1;
        bucket.count <= MAX_ATTEMPTS
    }

    /// Test-only: force an IP's window to have started at `when`, so a test can
    /// simulate window expiry without sleeping. Mirrors the pattern
    /// `session.rs`'s tests use on `Bootstrap::expires_at`.
    #[cfg(test)]
    fn force_window_start(&self, addr: IpAddr, when: Instant) {
        self.buckets.lock().unwrap().get_mut(&addr).unwrap().window_started = when;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "192.168.1.42".parse().unwrap()
    }

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let l = SignInLimiter::new();
        for _ in 0..MAX_ATTEMPTS {
            assert!(l.check(ip()));
        }
        assert!(!l.check(ip()), "the attempt past the limit is refused");
    }

    #[test]
    fn different_ips_get_independent_buckets() {
        let l = SignInLimiter::new();
        let other: IpAddr = "192.168.1.99".parse().unwrap();
        for _ in 0..MAX_ATTEMPTS {
            assert!(l.check(ip()));
        }
        assert!(!l.check(ip()));
        assert!(other != ip());
        assert!(l.check(other), "a different source IP is unaffected");
    }

    #[test]
    fn the_window_resets_after_it_elapses() {
        let l = SignInLimiter::new();
        assert!(l.check(ip()));
        l.force_window_start(ip(), Instant::now() - WINDOW - Duration::from_secs(1));
        for _ in 0..MAX_ATTEMPTS {
            assert!(l.check(ip()), "a fresh window allows a fresh batch of attempts");
        }
    }
}
```

`crates/git-vista-protocol/src/dto.rs` — extend `SessionInfo`:

```rust
pub struct SessionInfo {
    pub authenticated: bool,
    #[serde(default)]
    pub csrf: Option<String>,
    /// Whether this session was established through the LAN listener (ADR
    /// 0005). Additive field (M1.02 rule: new fields are `#[serde(default)]`,
    /// no protocol bump) — an older client ignores it. Purely a UI signal: the
    /// LAN listener's write routes are structurally absent regardless of what
    /// a client does with this flag.
    #[serde(default)]
    pub via_lan: bool,
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p git-vista-server ratelimit -- --nocapture`
Expected: FAIL — module doesn't exist / isn't wired into `main.rs` yet.

- [ ] **Step 3: Implement — wire the module and rewrite the session handlers**

Add `mod ratelimit;` to `main.rs`'s module list (next to `mod security;`):

```rust
mod ratelimit;
```

Rewrite `crates/git-vista-server/src/handlers/session.rs` in full:

```rust
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

/// `DELETE /api/session`: revoke the current session and clear the cookie.
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
```

- [ ] **Step 4: Fix the now-broken wire-test fixtures in `security.rs`**

The `require_auth` wire tests build a router directly. Update the `app()` helper
and the `req()` helper in `security.rs`'s `mod wire_tests`:

```rust
    /// The wired router plus the session store it shares, so a test can read the
    /// current bootstrap token to establish a session.
    fn app() -> (Router, Arc<SessionManager>) {
        app_with_limiter(None)
    }

    /// Same as `app()`, but lets a test install a rate limiter — used by the new
    /// LAN sign-in rate-limit test below.
    fn app_with_limiter(rate_limiter: Option<Arc<crate::ratelimit::SignInLimiter>>) -> (Router, Arc<SessionManager>) {
        let sessions = Arc::new(SessionManager::new(None));
        let session_state = crate::handlers::session::SessionState {
            manager: sessions.clone(),
            via_lan: rate_limiter.is_some(),
            rate_limiter,
        };
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
            .with_state(session_state);
        (router, sessions)
    }

    /// A request builder pre-loaded with a valid loopback `Host` and connect
    /// info — `create_session` extracts `ConnectInfo<SocketAddr>` now that
    /// sign-in can be rate-limited, and `oneshot()` skips the real listener
    /// that would normally supply it in production.
    fn req(method: &str, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:8080")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                55000,
            ))))
    }
```

Add one new wire test at the end of `mod wire_tests`, proving the spec's
required case ("rate limit triggers on repeated LAN sign-in attempts"):

```rust
    #[tokio::test]
    async fn lan_sign_in_is_rate_limited_but_loopback_is_not() {
        let limiter = Arc::new(crate::ratelimit::SignInLimiter::new());
        let (router, sessions) = app_with_limiter(Some(limiter));
        let token = sessions.current_bootstrap();
        // The manager only holds one outstanding bootstrap token, and a
        // successful exchange rotates it — so drive the limiter with wrong
        // tokens (still counted, still 401) until the 6th attempt is 429.
        for _ in 0..5 {
            let resp = router
                .clone()
                .oneshot(
                    req("POST", "/api/session")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"token":"wrong"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let sixth = router
            .clone()
            .oneshot(
                req("POST", "/api/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sixth.status(), StatusCode::TOO_MANY_REQUESTS);

        // A router with no limiter (the loopback shape) is unaffected by volume.
        let (loopback_router, loopback_sessions) = app();
        let loopback_token = loopback_sessions.current_bootstrap();
        for _ in 0..7 {
            let resp = loopback_router
                .clone()
                .oneshot(
                    req("POST", "/api/session")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"token":"wrong"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let resp = loopback_router
            .oneshot(
                req("POST", "/api/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"token":"{loopback_token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "loopback sign-in has no rate limit");
    }
```

- [ ] **Step 5: Run the full server test suite**

Run: `cargo test -p git-vista-server`
Expected: PASS — all prior tests (updated fixtures) plus the new ratelimit and
wire tests.

- [ ] **Step 6: Commit**

```bash
git add crates/git-vista-protocol/src/dto.rs crates/git-vista-server/src/ratelimit.rs crates/git-vista-server/src/main.rs crates/git-vista-server/src/handlers/session.rs crates/git-vista-server/src/security.rs
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "feat(server): via_lan session flag + LAN sign-in rate limit (ADR 0005)"
```

---

### Task 4: Dual-listener router split in `main.rs`

**Files:**
- Modify: `crates/git-vista-server/src/main.rs`

**Interfaces:**
- Consumes: `state::lan_bind_addr()` (Task 1), `security::HostPolicy::lan`
  (Task 2), `handlers::session::SessionState` + `ratelimit::SignInLimiter`
  (Task 3).
- Produces: `fn api_router(session_state: SessionState, hosts: HostPolicy,
  full_routes: bool) -> Router` and `fn build_app(...) -> Router` — nothing
  outside `main.rs` consumes these; this task is where Tasks 1-3 get wired
  into a running server.

- [ ] **Step 1: Write the failing test**

The spec's required wire test is "LAN router serves no write route → 404".
Test it directly against the real route table (not a hand-rolled stand-in like
`security.rs`'s wire tests use), by testing `api_router` alone — no SPA
fallback, so it needs no built `DIST_DIR` and can't accidentally pass because
of the static-file fallback rather than actual route absence. Add to the
bottom of `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_lan_router_has_no_write_routes() {
        let sessions = Arc::new(SessionManager::new(None));
        let session_state = SessionState {
            manager: sessions,
            via_lan: true,
            rate_limiter: None,
        };
        let router = api_router(
            session_state,
            HostPolicy::lan("192.168.1.42".parse().unwrap(), PORT),
            false,
        );
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/commit")
                    .header(header::HOST, "192.168.1.42:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Not 401/403 (a registered-but-refused route) — 404, because the
        // route was never built on this router at all.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_loopback_router_still_has_write_routes_registered() {
        let sessions = Arc::new(SessionManager::new(None));
        let session_state = SessionState {
            manager: sessions,
            via_lan: false,
            rate_limiter: None,
        };
        let router = api_router(session_state, HostPolicy::loopback(PORT), true);
        // No session cookie, so a registered route refuses with 401 — proving
        // the route exists, in contrast to the LAN router's 404 above.
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/commit")
                    .header(header::HOST, "localhost:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p git-vista-server the_lan_router_has_no_write_routes -- --nocapture`
Expected: FAIL to compile — `api_router` doesn't exist yet.

- [ ] **Step 3: Split `build_app` into `api_router` + `build_app`, rewrite `main`**

Replace the imports block's `use state::{...}` line with:

```rust
use state::{bind_addr, bootstrap_token_path, current, lan_bind_addr, set_current, DEFAULT_REPO, DIST_DIR, PORT};
```

Add near the top-level imports:

```rust
use std::net::SocketAddr;

use handlers::session::SessionState;
use ratelimit::SignInLimiter;
```

Extract everything from `let api = Router::new()` through the `app` assembly
(currently lines ~200-307) into two functions, placed above `main`. Splitting
the route table out from the SPA/security-header wrapping is what lets the
test above exercise real route registration without needing a built frontend
bundle on disk:

```rust
/// The `/api/*` route table plus its auth/contract layers, for one listener.
/// `full_routes` selects whether the write/select/rescan/clone endpoints are
/// registered at all: `true` for the loopback listener, `false` for the LAN
/// listener (ADR 0005) — those routes are never *built* on the LAN router, not
/// merely gated, so a mode-check regression can't reopen them. Kept separate
/// from [`build_app`] so a test can exercise route registration directly,
/// without the static-file fallback (and its `DIST_DIR` dependency) in the way.
fn api_router(session_state: SessionState, hosts: HostPolicy, full_routes: bool) -> Router {
    let auth_state = AuthState {
        manager: session_state.manager.clone(),
        hosts,
    };

    let mut api = Router::new()
        .route("/api/protocol", get(protocol_info))
        .route("/api/catalog", get(handlers::catalog::catalog_list))
        .route(
            "/api/session",
            get(session_status)
                .post(create_session)
                .delete(revoke_session),
        )
        .route("/api/commits", get(commits))
        .route("/api/commit/{id}", get(commit_detail))
        .route("/api/diff/{id}", get(commit_diff))
        .route("/api/file/{id}/{*path}", get(file_at_commit))
        .route("/api/head-branch", get(head_branch))
        .route("/api/status", get(worktree_status))
        .route("/api/activity", get(activity::activity_feed))
        .route("/api/undoables/{id}", get(activity::undoables))
        .route("/api/rebase-status", get(rebase_status));

    // ADR 0005: every write / repo-selection / clone endpoint is registered
    // only when full_routes is set — the LAN router never sees these routes
    // exist at all.
    if full_routes {
        api = api
            .route("/api/clone", post(clone_repo))
            .route("/api/delete-clone", post(delete_clone_repo))
            .route("/api/select", post(select_repo))
            .route("/api/rescan", post(rescan))
            .route("/api/branch", post(create_branch))
            .route("/api/commit", post(create_commit))
            .route("/api/stage", post(stage_all))
            .route("/api/unstage", post(unstage_all))
            .route("/api/undo", post(activity::undo))
            .route("/api/merge", post(merge_branch))
            .route("/api/push", post(push_branch))
            .route("/api/delete-branch", post(delete_branch))
            .route("/api/checkout", post(checkout_branch))
            .route("/api/force-delete-branch", post(force_delete_branch))
            .route("/api/rebase", post(rebase))
            .route("/api/reset-test-repo", post(reset_test_repo));
    }

    api.layer(CatchPanicLayer::custom(panic_to_response))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            security::require_auth,
        ))
        .layer(axum::middleware::from_fn(middleware::api_contract))
        .with_state(session_state)
}

/// Assemble one full application — [`api_router`] plus the static SPA fallback
/// and the two outer layers — for one listener.
fn build_app(session_state: SessionState, hosts: HostPolicy, full_routes: bool) -> Router {
    let spa = SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    )
    .layer(ServeDir::new(DIST_DIR).append_index_html_on_directories(true));

    Router::new()
        .merge(api_router(session_state, hosts, full_routes))
        .fallback_service(spa)
        .layer(CatchPanicLayer::custom(panic_to_response))
        .layer(axum::middleware::from_fn(security::security_headers))
}
```

In `main`, after the existing loopback `listener` bind block, add the LAN
resolve-and-bind block:

```rust
    // ADR 0005: resolve the optional second, LAN-facing listener. `gv` is
    // responsible for auto-detecting the LAN IP or requiring --lan-ip before
    // ever setting GIT_VISTA_LAN_IP, so a rejection here is a clean startup
    // error, matching the loopback bind_addr() error path above.
    let lan_addr = match lan_bind_addr() {
        None => None,
        Some(Ok(addr)) => Some(addr),
        Some(Err(error)) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };
    let lan_listener = match lan_addr {
        Some(addr) => match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                eprintln!("error: could not bind LAN listener {addr}: {error}");
                std::process::exit(1);
            }
        },
        None => None,
    };
```

Replace the current `let api = Router::new()...` through `let app = Router::new()...with_state...` block entirely with:

```rust
    let loopback_session_state = SessionState {
        manager: sessions.clone(),
        via_lan: false,
        rate_limiter: None,
    };
    let loopback_app = build_app(loopback_session_state, HostPolicy::loopback(PORT), true);
```

Replace the final block (`print_startup_banner(...)` through the closing
`axum::serve(...)` and its `if let Err(e) = ...` error handling) with:

```rust
    print_startup_banner(&bootstrap_token_path(), lan_addr);

    match lan_listener {
        Some(lan_listener) => {
            let lan_ip = lan_addr.expect("lan_listener implies lan_addr").ip();
            let lan_session_state = SessionState {
                manager: sessions.clone(),
                via_lan: true,
                rate_limiter: Some(Arc::new(SignInLimiter::new())),
            };
            let lan_app = build_app(lan_session_state, HostPolicy::lan(lan_ip, PORT), false);
            let loopback_serve = axum::serve(
                listener,
                loopback_app.into_make_service_with_connect_info::<SocketAddr>(),
            );
            let lan_serve = axum::serve(
                lan_listener,
                lan_app.into_make_service_with_connect_info::<SocketAddr>(),
            );
            if let Err(e) = tokio::try_join!(loopback_serve, lan_serve) {
                eprintln!("error: server stopped: {e}");
                std::process::exit(1);
            }
        }
        None => {
            if let Err(e) = axum::serve(
                listener,
                loopback_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                eprintln!("error: server stopped: {e}");
                std::process::exit(1);
            }
        }
    }
```

Update `print_startup_banner`'s signature and body:

```rust
/// Print the supported access paths: local loopback, an SSH tunnel whose remote
/// endpoint is that same loopback listener, and — only when `lan_addr` is
/// `Some` — the LAN view profile's plain-HTTP address and its documented risk.
fn print_startup_banner(token_path: &Path, lan_addr: Option<SocketAddr>) {
    println!("git-vista server — serving {}", current().0.display());
    println!("  • on this machine: http://localhost:{PORT}/");
    println!("  • from the iPad: use an SSH local port forward to 127.0.0.1:{PORT}");
    match lan_addr {
        Some(addr) => {
            println!("  • LAN view (ADR 0005, read-only): http://{addr}/");
            println!("    WARNING: plain HTTP — repo contents and the session cookie are");
            println!("    readable by anyone on this network. Trusted home LAN only,");
            println!("    never a guest or shared network.");
        }
        None => println!("  • direct LAN access is disabled"),
    }
    println!("  • sign in: open the setup link `gv` printed (or `gv --token`).");
    println!(
        "    the one-time token lives, 0600, at {}",
        token_path.display()
    );
    println!();
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build -p git-vista-server && cargo test -p git-vista-server`
Expected: builds clean, all tests PASS — including the two new ones from
Step 1 and every test from Tasks 1-3.

- [ ] **Step 5: Commit**

```bash
git add crates/git-vista-server/src/main.rs
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "feat(server): serve a second, structurally read-only LAN listener (ADR 0005)"
```

---

### Task 5: Frontend — hide Active mode on a LAN session

**Files:**
- Modify: `crates/git-vista/src/api.rs`
- Modify: `crates/git-vista/src/session.rs`
- Modify: `crates/git-vista/src/picker.rs`

**Interfaces:**
- Consumes: `SessionInfo.via_lan` (Task 3, already wire-compatible via
  `#[serde(default)]`).
- Produces: `pub fn is_lan_session() -> bool` in `api.rs`, used by `picker.rs`.

- [ ] **Step 1: No frontend unit-test harness exists for these files** (verified:
  no `#[cfg(test)]` blocks in `api.rs`/`session.rs`/`picker.rs` today). Verify
  this task the way the persistent-clones feature's frontend changes were
  verified: `trunk build` compiles clean now, and a live Playwright pass in
  Task 7 confirms the Active button is actually absent on a LAN-established
  session.

- [ ] **Step 2: Implement**

In `crates/git-vista/src/api.rs`, add next to the existing `UI_MODE` thread-local:

```rust
// Whether the current session came through the LAN listener (ADR 0005) —
// mirrored from the session-establish/-check response. Purely a UI signal: it
// drives hiding the Active option on the mode screen. The server's own route
// absence on the LAN listener is the actual write boundary.
thread_local! {
    static VIA_LAN: RefCell<bool> = const { RefCell::new(false) };
}

/// Record whether the current session is LAN-scoped — called by
/// [`crate::session`] after establishing or checking the session.
pub fn set_via_lan(via_lan: bool) {
    VIA_LAN.with(|v| *v.borrow_mut() = via_lan);
}

/// Whether the current session came through the LAN listener (ADR 0005).
pub fn is_lan_session() -> bool {
    VIA_LAN.with(|v| *v.borrow())
}
```

In `crates/git-vista/src/session.rs`, update the import and `establish_session`:

```rust
use crate::api::{get_session, post_session, set_csrf_token, set_via_lan};

pub async fn establish_session() -> Result<bool, String> {
    if let Some(token) = take_bootstrap_token() {
        if let Ok(info) = post_session(&token).await {
            set_csrf_token(info.csrf.clone());
            set_via_lan(info.via_lan);
            return Ok(info.authenticated);
        }
    }
    let info = get_session().await?;
    set_csrf_token(info.csrf.clone());
    set_via_lan(info.via_lan);
    Ok(info.authenticated)
}
```

In `crates/git-vista/src/picker.rs`'s `mode_view`, wrap the Active button so it
renders only off a LAN session (spec: "via the LAN listener the mode screen
offers Visualize only — Active button absent"):

```rust
                        {(!crate::api::is_lan_session()).then(|| view! {
                            <button
                                style="display:block; width:100%; padding:16px; margin:8px 0; \
                                       font:inherit; font-size:1.05em; color:#fff; \
                                       background:#238636; border:1px solid #2ea043; \
                                       border-radius:8px;"
                                disabled=move || busy.get()
                                on:click=choose(RepoMode::Active)
                            >
                                "Active — full git operations"
                            </button>
                        })}
```

(Replaces the existing unconditional `<button ... on:click=choose(RepoMode::Active)>` block — same styling and click handler, now gated.)

- [ ] **Step 3: Build**

Run: `cd crates/git-vista && trunk build`
Expected: builds clean, no warnings about the new functions being unused.

- [ ] **Step 4: Commit**

```bash
git add crates/git-vista/src/api.rs crates/git-vista/src/session.rs crates/git-vista/src/picker.rs
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "feat(ui): hide Active mode on a LAN-view session (ADR 0005)"
```

---

### Task 6: `gv` launcher — `--lan-view`/`--lan-ip`, doctor, exposed-listener check

**Files:**
- Modify: `gv`

**Interfaces:**
- Consumes: nothing from earlier tasks except the env var contract
  (`GIT_VISTA_LAN_IP`) Task 1/4 already read.
- Produces: nothing consumed by later Rust/frontend tasks — this is the
  operator-facing CLI surface, consumed only by Task 7's live verification.

- [ ] **Step 1: No automated test harness exists for `gv`** (a bash script, no
  test runner in this repo). Verify by direct invocation in Task 7.

- [ ] **Step 2: Implement**

Add to the usage comment block at the top of `gv` (after the existing `--root`
line):

```
#   gv --lan-view [path]  also bind a second, read-only LAN listener (ADR
#                       0005): a backup path to the iPad when the SSH tunnel
#                       is unavailable. Auto-detects the LAN IP when the
#                       machine has exactly one candidate; otherwise pass
#                       --lan-ip. Plain HTTP — trusted home LAN only, never a
#                       guest/shared network.
#   gv --lan-ip <addr>  the explicit LAN IP for --lan-view, when auto-detect
#                       can't pick a single candidate.
```

Add near the top, with the other helper functions (`listener_address`, etc.):

```bash
list_lan_ips() {
  if command -v ip >/dev/null 2>&1; then
    ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1
  fi
}

detect_lan_ip() {
  local candidates count
  candidates="$(list_lan_ips)"
  count="$(printf '%s\n' "$candidates" | grep -c .)"
  if [[ $count -eq 1 ]]; then
    printf '%s\n' "$candidates"
    return 0
  fi
  return 1
}
```

Add `LAN_FILE="$LOG_DIR/server.lan"` next to the other `*_FILE` variable
declarations.

Leave the existing singular `listener_address()` untouched (`doctor` still uses
it for its one-line "bind: …" summary — showing the first-detected socket is
still useful there once the new explicit LAN line below is added). Add a new
plural helper next to it, and replace only `listener_mode`'s body with a
multi-socket-aware version that uses the new helper:

```bash
listener_addresses() {
  if command -v ss >/dev/null 2>&1; then
    ss -H -ltn "sport = :${PORT}" 2>/dev/null | awk '{print $4}' | sort -u
  fi
}

listener_mode() {
  local addresses expected_lan address unexpected=0 saw_loopback=0 saw_lan=0
  addresses="$(listener_addresses)"
  if [[ -z $addresses ]]; then
    if server_health localhost; then
      printf '%s\n' unknown
    else
      printf '%s\n' down
    fi
    return
  fi
  expected_lan=""
  if [[ -r "$LAN_FILE" ]]; then
    expected_lan="$(cat "$LAN_FILE"):${PORT}"
  fi
  while IFS= read -r address; do
    if [[ $address == "127.0.0.1:${PORT}" ]]; then
      saw_loopback=1
    elif [[ -n $expected_lan && $address == "$expected_lan" ]]; then
      saw_lan=1
    else
      unexpected=1
    fi
  done <<<"$addresses"
  if [[ $unexpected -eq 1 || $saw_loopback -eq 0 ]]; then
    printf '%s\n' exposed
  elif [[ -n $expected_lan && $saw_lan -eq 0 ]]; then
    # --lan-view was requested but the sanctioned socket isn't actually up —
    # a mismatch worth flagging, not a silent downgrade to "just loopback".
    printf '%s\n' exposed
  elif [[ -n $expected_lan ]]; then
    printf '%s\n' lan_view
  else
    printf '%s\n' loopback
  fi
}
```

Update every `[[ $mode == exposed ]]`/`[[ $LIVE_MODE == exposed ]]` check's
surrounding logic to also treat `lan_view` as healthy (only `exposed`/`unknown`/
`down` are problems). Concretely:

- In `print_token_links`, after `mode="$(listener_mode)"`, the existing
  `if [[ $mode == exposed ]]; then ...; fi` stays as-is (still correctly fires
  only on `exposed`, not `lan_view`) — no change needed there beyond the
  `listener_mode` rewrite above. Add, right after the existing loopback link
  line, a LAN link line:

```bash
  if [[ $mode == lan_view && -r "$LAN_FILE" ]]; then
    echo "gv:   LAN view (read-only, this network only):  http://$(cat "$LAN_FILE"):${PORT}/#s=${token}"
  fi
```

- In `doctor`, replace the existing final line:

```bash
  echo "  LAN: direct LAN access is disabled; only the SSH tunnel is supported."
```

  with:

```bash
  if [[ -r "$LAN_FILE" ]]; then
    echo "  LAN view (ADR 0005): $(cat "$LAN_FILE"):${PORT} — read-only, plain HTTP, trusted-LAN only"
  else
    echo "  LAN: direct LAN access is disabled; only the SSH tunnel is supported."
  fi
```

  And extend the `mode == exposed` check earlier in `doctor` to keep its
  existing wording (it already says "non-loopback interface"; still correct
  since `lan_view` is a distinct value from `exposed` now).

- In the main launch path's post-start check:

```bash
    LIVE_MODE="$(listener_mode)"
    if [[ $LIVE_MODE == exposed ]]; then
```

  stays as-is (already only matches `exposed`).

Add CLI parsing. Near the top of the arg-parsing section, add:

```bash
LAN_VIEW=0
LAN_IP=""
NEXT_IS_LAN_IP=0
```

Inside the `for arg in "$@"; do` loop, add right after the existing
`if [[ $NEXT_IS_ROOT -eq 1 ]]; then ...; fi` block:

```bash
  if [[ $NEXT_IS_LAN_IP -eq 1 ]]; then
    LAN_IP="$arg"
    NEXT_IS_LAN_IP=0
    continue
  fi
```

Inside the `case "$arg" in`, add two new cases (near `--root`/`--root=*`):

```bash
    --lan-view) LAN_VIEW=1 ;;
    --lan-ip) NEXT_IS_LAN_IP=1 ;;
    --lan-ip=*) LAN_IP="${arg#--lan-ip=}" ;;
```

After the existing `if [[ $NEXT_IS_ROOT -eq 1 ]]; then ... fi` post-loop check,
add:

```bash
if [[ $NEXT_IS_LAN_IP -eq 1 ]]; then
  echo "gv: --lan-ip needs an address" >&2
  exit 2
fi
if [[ $LAN_VIEW -eq 1 && -z $LAN_IP ]]; then
  LAN_IP="$(detect_lan_ip)" || {
    echo "gv: --lan-view needs --lan-ip <addr> — couldn't auto-detect a single LAN address" >&2
    echo "gv: candidates found:" >&2
    list_lan_ips >&2
    exit 2
  }
elif [[ $LAN_VIEW -eq 0 && -n $LAN_IP ]]; then
  echo "gv: --lan-ip only applies with --lan-view" >&2
  exit 2
fi
if [[ -n $LAN_IP ]]; then
  case "$LAN_IP" in
    127.*|0.0.0.0)
      echo "gv: --lan-ip must be a real LAN interface address, not loopback or 0.0.0.0" >&2
      exit 2
      ;;
  esac
fi
```

In `record_runtime_state`, add a fourth parameter and persist/clear the LAN ip:

```bash
record_runtime_state() {
  local pid="$1" mode="$2" target="$3" lan_ip="${4:-}"
  write_private_file "$PID_FILE" "$pid"
  write_private_file "$MODE_FILE" "$mode"
  write_private_file "$TARGET_FILE" "$target"
  if [[ -n $lan_ip ]]; then
    write_private_file "$LAN_FILE" "$lan_ip"
  else
    rm -f "$LAN_FILE"
  fi
}
```

Update `stop_repo_servers` to also clean up `$LAN_FILE`:

```bash
  rm -f "$PID_FILE" "$MODE_FILE" "$TARGET_FILE" "$LAN_FILE"
```

Update the launch section: export the env var and pass `LAN_IP` through to
`record_runtime_state`:

```bash
export GIT_VISTA_BIND_ADDR="127.0.0.1:${PORT}"
if [[ -n $REPO_ROOT ]]; then
  export GIT_VISTA_REPO_ROOT="$REPO_ROOT"
fi
if [[ $LAN_VIEW -eq 1 ]]; then
  export GIT_VISTA_LAN_IP="$LAN_IP"
fi

setsid "$SERVER_BIN" "$TARGET" >>"$LOG" 2>&1 </dev/null &
SERVER_PID=$!
record_runtime_state "$SERVER_PID" loopback "$TARGET" "$LAN_IP"
```

And in the post-start success message, print the LAN line when active:

```bash
    echo "gv: server up in loopback/SSH mode — http://127.0.0.1:${PORT}/"
    if [[ -n $LAN_IP ]]; then
      echo "gv: LAN view up — http://${LAN_IP}:${PORT}/ (read-only, plain HTTP, trusted LAN only)"
    fi
```

The existing `gv --lan` hard-rejection case is unchanged and stays exactly as
written (spelling `--lan-view` is required; bare `--lan` still errors).

- [ ] **Step 3: Smoke-test the argument parsing without starting a server**

Run these (no server side effects — they exit before `cd "$REPO"`):

```bash
./gv --lan-view --lan-ip 127.0.0.1 . ; echo "exit: $?"
```

Expected: `gv: --lan-ip must be a real LAN interface address...` and exit 2.

```bash
./gv --lan-ip 192.168.1.1 . ; echo "exit: $?"
```

Expected: `gv: --lan-ip only applies with --lan-view` and exit 2.

```bash
bash -n gv
```

Expected: no output (syntax OK).

- [ ] **Step 4: Commit**

```bash
git add gv
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "feat(gv): --lan-view/--lan-ip launch flags, doctor learns the sanctioned LAN socket (ADR 0005)"
```

---

### Task 7: Docs, gate, live verification, PR, merge

**Files:**
- Modify: `docs/adr/0005-lan-view-profile.md`
- Modify: `docs/adr/README.md`
- Modify: `docs/SECURITY_MODEL.md`
- Modify: `handoff.md`

- [ ] **Step 1: Flip ADR 0005's status**

Change the header line:

```
- **Status:** Accepted — implementation pending (`feature/lan-view-mode`)
```

to:

```
- **Status:** Accepted — implemented 2026-07-20 (`feature/lan-view-mode`, #122)
```

- [ ] **Step 2: Flip the ADR index row**

In `docs/adr/README.md`, change:

```
| [0005](0005-lan-view-profile.md) | LAN view profile: a read-only second listener | Accepted — implementation pending |
```

to:

```
| [0005](0005-lan-view-profile.md) | LAN view profile: a read-only second listener | Accepted — implemented |
```

- [ ] **Step 3: Amend SECURITY_MODEL.md**

In the "Operating Modes" table, split the existing "LAN paired" row so the
now-implemented view profile is distinct from the still-future paired-HTTPS
write mode:

```
| Mode | Bind | Transport | Authentication | Intended use |
|---|---|---|---|---|
| Local | `127.0.0.1` | HTTP localhost | Launch/session secret + same-origin | Browser on the Linux/macOS/Windows host |
| SSH tunnel | `127.0.0.1` on Linux | SSH encrypted forwarding | SSH plus Git-Vista session | Primary iPad-to-Linux workflow |
| LAN view | One explicit interface | Plain HTTP | Single-use bootstrap token, view-scoped read-only routes, rate-limited sign-in | Backup path when the SSH tunnel is unavailable, trusted home LAN only (ADR 0005) |
| LAN paired, future | Explicit interface | HTTPS | One-time pairing and device session | Trusted private network without SSH tunnel, full read/write |
| Team, future | Reverse proxy/private network | HTTPS | OIDC/passkeys plus RBAC | Explicit multi-user deployment, not V2 default |
```

Retitle and annotate the existing `## LAN Mode` section so it's unambiguously
the future write-capable profile, not what's now implemented:

```
## LAN Mode (future, paired HTTPS — write-capable)

The read-only LAN view profile implemented under ADR 0005 is documented
separately below ("LAN View Profile"); this section covers the *different*,
still-future write-capable LAN mode.

No current *write-capable* LAN mode exists...
```//! (keep the rest of the existing bullet list verbatim)

Add a new section immediately after `## LAN Mode (future, paired HTTPS — write-capable)`:

```
## LAN View Profile (implemented, ADR 0005)

`gv --lan-view [path]` starts the existing loopback server plus a second
listener, bound to one explicit, operator-confirmed LAN IP:

- The second listener serves a structurally reduced router: GET read routes
  plus `POST`/`DELETE /api/session` only. Every write, `/api/select`,
  `/api/rescan`, `/api/clone`, and `/api/delete-clone` route is never
  registered on it — absence, not a runtime check.
- `Host`/`Origin` on this listener are pinned to the one sanctioned LAN
  IP:port; neither `localhost` nor any other address the machine answers on
  is accepted, so a DNS-rebinding attempt against the LAN listener fails
  closed the same way the loopback listener's Host check does.
- Sign-in (`POST /api/session`) on this listener is rate-limited per source
  IP (`crates/git-vista-server/src/ratelimit.rs`); the loopback listener's
  sign-in is unaffected.
- Auth is otherwise the same single-use bootstrap-token flow as loopback,
  sharing one in-memory session store; a session established via either
  listener carries a `via_lan` flag purely so the UI can hide the Active
  option — the actual write boundary is the LAN router's absent routes.
- Accepted, documented risk: plain HTTP means repo contents and the session
  cookie are readable by anyone on the same network. Suitable for a trusted
  home LAN, never a guest or shared network — the startup banner and `gv
  doctor` say so explicitly.
- `gv doctor` and the launch-time exposed-listener kill-check learn the
  sanctioned second socket: with `--lan-view`, exactly {loopback, the
  recorded LAN ip} on port 8080 is healthy; anything else is still a
  SECURITY ERROR that stops the server. Without the flag, behavior is
  unchanged from M1.05.
```

- [ ] **Step 4: `./dev gate`**

Run: `./dev gate`
Expected: fmt clean, clippy clean (native + wasm), all tests pass, `trunk
build` succeeds. Fix anything that doesn't before proceeding.

- [ ] **Step 5: Live verification — ask Tom first**

This step requires restarting the running `git-vista-server` to pick up the
new binary, which the project memory `do-not-restart-the-running-server`
requires asking about first (port 8080 may be Tom's live iPad session). Before
running anything in this step: **ask Tom for explicit go-ahead to take port
8080**, per that standing rule — do not restart silently.

Once cleared, from the repo root:

```bash
./gv --lan-view --root ~/projects .
```

Expected: prints both the loopback and LAN view links; `gv doctor` reports
`LAN view (ADR 0005): <ip>:8080 — read-only, plain HTTP, trusted-LAN only`
with no SECURITY ERROR line.

```bash
LAN_IP=$(cat ~/.local/state/git-vista/server.lan)
curl -s -o /dev/null -w '%{http_code}\n' -X POST "http://${LAN_IP}:8080/api/commit" \
  -H "Host: ${LAN_IP}:8080"
```

Expected: `404` (route absent — no write route exists on the LAN router at all).

```bash
curl -s -o /dev/null -w '%{http_code}\n' "http://${LAN_IP}:8080/api/commits" \
  -H "Host: evil.example.com:8080"
```

Expected: `403` (Host pinning refuses anything but the sanctioned LAN IP).

```bash
for i in $(seq 1 6); do
  curl -s -o /dev/null -w '%{http_code} ' -X POST "http://${LAN_IP}:8080/api/session" \
    -H "Host: ${LAN_IP}:8080" -H 'Content-Type: application/json' -d '{"token":"wrong"}'
done; echo
```

Expected: five `401`s then a `429` on the sixth.

Real cross-device verification (opening the LAN link from a second phone/laptop
on the same network) is Tom's to do — it needs a genuinely separate device,
same as the picker/clone-delete touch check that's still outstanding from
#121.

- [ ] **Step 6: Update `handoff.md`**

Mark #122 done/merged (mirroring how #121 was marked), note the outstanding
real-device LAN verification alongside the already-outstanding iPad touch
check from #121, and note there is no next filed issue queued — ask Tom what's
next once this merges.

- [ ] **Step 7: Commit docs**

```bash
git add docs/adr/0005-lan-view-profile.md docs/adr/README.md docs/SECURITY_MODEL.md
git -c user.name=claude_2010 -c user.email=262510778+tom2025b@users.noreply.github.com commit -m "docs: ADR 0005 implemented; SECURITY_MODEL LAN view profile amendment (#122)"
```

(`handoff.md` is gitignored — no commit for it.)

- [ ] **Step 8: PR and merge**

```bash
git push -u origin feature/lan-view-mode
gh pr create --title "feature ③: LAN view profile — read-only second listener (ADR 0005)" --body "Closes #122
..."
gh pr merge --merge   # or as Tom directs; never delete the branch afterward
```

Follow the project's standing "git dance": push, PR with `Closes #122`, merge
to `main`, keep `feature/lan-view-mode` (never delete branches).
