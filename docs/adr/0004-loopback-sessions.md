# ADR 0004 — Protecting loopback sessions and mutating requests

- **Status:** Accepted
- **Date:** 2026-07-15
- **Milestone / issue:** M1.04 — Protect Loopback Sessions and Mutating Requests (#57)
- **Supersedes / superseded by:** —

## Context

Until this milestone git-vista had **no authentication at all**. It bound to
loopback and leaned on that plus the SSH tunnel as its only protection. But
`docs/SECURITY_MODEL.md` ("Local and SSH Session Design") is explicit that
loopback is *necessary but not sufficient*:

- A malicious webpage open in the same browser can `fetch('http://localhost:8080/…')`
  and drive git write operations (CSRF).
- A **DNS-rebinding** attack points an attacker-controlled hostname at `127.0.0.1`,
  so the browser connects to us while believing it's talking to the attacker's origin.
- Over the SSH tunnel *every* request arrives as loopback, so "is this loopback?"
  cannot stand in for "is this the operator?".

The M1.02 protocol header gave incidental CSRF resistance (a custom header a simple
cross-origin form can't set), but that was a side effect, not a control. This ADR
records the real identity + request-integrity layer.

## Decision

A high-entropy secret is minted at **every service start** and exchanged, once, for
a session cookie; every `/api/*` request is then gated on session, CSRF, Origin,
Host, content type, and method. Two `tower` layers implement it
(`git-vista-server::security`), sitting *inside* the M1.02 contract layer so their
refusals come back as the same structured error envelope.

### 1. Bootstrap token → session cookie, over the URL fragment

The server mints a 256-bit bootstrap token (`getrandom`) and writes it `0600` to
`$XDG_STATE_HOME/git-vista/bootstrap.token`. It is **never** printed to a log or
placed in a request URL. The `gv` launcher — the only process that, as the same
user, can read that file — prints a setup link `http://localhost:8080/#s=<token>`.

The token rides in the URL **fragment**, which the browser never transmits to the
server, so it can't land in an access log. The SPA reads it on load, `POST`s it in
the JSON body to `/api/session`, and immediately strips the fragment with
`history.replaceState`. The server returns an **HttpOnly, `SameSite=Strict`**
session cookie plus a CSRF token. This reconciles the issue's "secrets never in
URLs or logs" with the security model's "print a bootstrap URL, remove the secret
from the visible URL immediately".

The token is **single-use**: a successful exchange rotates a fresh token into the
file, so the redeemed one can never be replayed, while a second device can still
`gv --token` for a new link. Both the token and the session **expire** (1 h unused
bootstrap; 12 h idle session, refreshed on use). Sessions are revocable
(`DELETE /api/session`), and a restart drops every in-memory session.

### 2. The request gate (`require_auth`), in order

1. **Method** allowlist — `GET/HEAD/POST/PUT/PATCH/DELETE`; anything else (incl.
   `OPTIONS`, since we serve no CORS) is `405`.
2. **Host** — must be a loopback literal (`localhost`, `127.0.0.1`, `::1`), or the
   explicit LAN bind IP. This is the anti-DNS-rebinding control: a rebinding
   attacker's `Host: evil.example` is refused. A `0.0.0.0` bind can't enumerate
   its hostnames, so host-pinning relaxes there (documented, warned at startup).
3. **Origin** — when present it must be same-origin; `Origin: null` is always
   refused. Absent Origin (same-origin GETs often omit it) is allowed, with CSRF +
   `SameSite=Strict` carrying the load on writes.
4. **Content type** — a *present* content type on a write must be
   `application/json`, which blocks the form-encoded CSRF vector while still
   allowing the app's bodyless `POST`s (which send no content type).
5. **Session + CSRF** — every route except `GET /api/protocol` and
   `GET`/`POST /api/session` needs a live session cookie; state-changing methods
   additionally need the session's CSRF token echoed in `x-git-vista-csrf`
   (compared constant-time). No session → `401` (the SPA's re-connect trigger);
   bad/absent CSRF on a live session → `403`.

Reads require a session too — the security model lists Authentication under the
Read risk class — so the SPA bootstraps before the graph loads.

### 3. Browser hardening headers (`security_headers`), on every response

CSP (`default-src 'self'`, `frame-ancestors 'none'`, `connect-src 'self'`, …),
`Cross-Origin-Opener-Policy`/`-Resource-Policy: same-origin`, `Referrer-Policy:
no-referrer`, `X-Content-Type-Options: nosniff`, `Permissions-Policy`, and
`X-Frame-Options: DENY`. API responses also carry `Cache-Control: no-store`.

Two deliberate CSP relaxations: `'wasm-unsafe-eval'` (the WebAssembly runtime needs
it) and `'unsafe-inline'` for script/style. Trunk injects an **inline** module
script to boot the wasm whose content hash changes every build, and the server
sets a *static* header — so a stable nonce/hash can't pin it. The residual XSS
surface is bounded by Leptos's default output escaping and the loopback + session
model. A later milestone can move to a build-time-computed hash or a per-response
nonce if the SPA shell is served dynamically.

## Alternatives considered

- **Token in the query string (Jupyter-style `?token=`).** Rejected: query strings
  land in access logs and browser history — the exact "secrets in URLs/logs" the
  issue forbids. The fragment avoids both.
- **Trust loopback, no session.** Rejected outright by the security model: it's the
  CSRF / DNS-rebinding hole this milestone exists to close.
- **Double-submit CSRF cookie** (no server-side token store). Rejected: we already
  hold sessions server-side, so a per-session token compared server-side is
  stronger than a cookie the page can read, and needs no extra readable cookie.
- **A `Secure` cookie.** Omitted: Local and SSH modes serve plain HTTP on loopback,
  where a `Secure` cookie is dropped. It must be added when an HTTPS LAN/paired mode
  arrives (see the security model's LAN section).
- **A cookie/crypto crate (`tower-sessions`, `cookie`, `axum-extra`).** Rejected for
  now: the store is a small in-memory map, the cookie is one line, and the token
  compare is a few lines — matching the codebase's "no dependency for a trivial
  format" posture (cf. the M1.02 request-id).

## Consequences

- The frontend must hold a session before any `/api/*` call succeeds; the SPA gains
  a one-shot bootstrap and a blocking "Connect to git-vista" screen keyed on the
  `unauthenticated` error code (added to the protocol crate).
- `gv` gains `--token` and prints the setup link after startup; its `--lan` warning
  is corrected (a session is now required; only TLS and strict host-pinning are the
  LAN gaps).
- A new `getrandom` dependency on the **server** crate only (never the wasm build).
- Sessions are per-process and in-memory by design: a restart is a full revocation.
  Durable/paired-device sessions and an HTTPS LAN mode are explicitly out of scope
  here and tracked by later milestones.
