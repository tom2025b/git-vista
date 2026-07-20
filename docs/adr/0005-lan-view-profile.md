# ADR 0005 — LAN view profile: a read-only second listener

- **Status:** Accepted — implemented 2026-07-20 (`feature/lan-view-mode`, #122)
- **Date:** 2026-07-19
- **Milestone / issue:** Post-M1.05 feature set; design spec
  `docs/superpowers/specs/2026-07-19-repo-modes-lan-visualizer-design.md`
  (lands with implementation branch ①)
- **Supersedes / superseded by:** Amends the scope of the M1.05 loopback-only
  enforcement (`ae28093`); the SECURITY_MODEL plain-HTTP-LAN **write** non-goal
  is untouched.

## Context

M1.05 removed direct LAN serving entirely: the server refuses to bind anything
but `127.0.0.1:8080`, and the SSH tunnel is the only remote path. In practice
the tunnel has repeatedly failed for the operator (wrong-machine invocation,
iOS suspending the SSH client), so a **backup remote path** is wanted for the
iPad — without reintroducing the plain-HTTP LAN write surface that M1.05
deliberately deleted.

## Decision

A new opt-in launch profile, `gv --lan-view [path]`, starts the normal
loopback server **plus a second listener** with these properties:

- Bound to **one explicit LAN IP** — auto-detected only when the machine has
  exactly one candidate, otherwise `--lan-ip <addr>` is required. `0.0.0.0`
  is never accepted; the non-strict Host escape removed in M1.05 does not
  return.
- The LAN listener serves a **separate router**: GET read routes plus
  `POST /api/session` / `DELETE /api/session` only. Write routes are **not
  registered** on it — structurally absent, not gated by a flag a bug could
  flip.
- Auth is still required: the same single-use bootstrap flow
  (`gv --token --lan-view` prints `http://<lan-ip>:8080/#s=…`). Sessions
  created via the LAN listener are **view-scoped**. Host header must exactly
  match the pinned LAN IP and port. LAN sign-in is rate-limited (the
  SECURITY_MODEL requirement for any beyond-loopback exposure). Plain
  `gv --lan` remains a hard rejection.
- `gv doctor` and the exposed-listener kill-check learn the sanctioned
  socket: a LAN listener is expected under `--lan-view` and still a SECURITY
  ERROR otherwise.
- Accepted, documented risk: plain-HTTP transport means repo contents and the
  view-scoped cookie are readable on the local network. Suitable for a
  trusted home LAN, not guest/shared networks; the startup banner and docs
  say so. A SECURITY_MODEL amendment defines the profile.

## Alternatives considered

- **Full active mode over plain-HTTP LAN.** Rejected permanently — it is an
  explicit SECURITY_MODEL non-goal; nothing here relaxes it.
- **Paired HTTPS for the LAN now.** Deferred, not rejected: it remains the
  sanctioned future path for LAN *writes*; the view profile does not depend
  on it and must not block it.
- **No LAN at all (status quo).** Rejected as the sole option: the tunnel's
  real-world failure rate on the iPad is what motivated a backup path.
- **Gating writes on the existing single listener** (one router, mode
  checks). Rejected: a check can regress; an unregistered route cannot.

## Consequences

- Two listeners with different capability sets; the session response tells
  the client which listener served it, and the client's mode screen offers
  Visualize only on LAN.
- New wire tests: write route on the LAN listener never reaches a handler
  (proven directly against the route table: `main.rs::the_lan_router_has_no_write_routes`);
  wrong Host → 403; repeated LAN sign-ins hit the rate limit.
- **Implementation note (live-verified 2026-07-20):** hitting an absent write
  path through the real server (not the bare route table) surfaces as `405
  Method Not Allowed` for a `POST`, not `404` — the request falls through to
  the static SPA fallback (`ServeDir`), which rejects non-GET/HEAD methods
  before it would even 404 on a missing file. The security property is
  identical either way (no handler runs, nothing mutates); only the exact
  status code differs from what this ADR originally predicted.
- The startup banner must display bound interfaces (model requirement).
- Loopback behavior without `--lan-view` is exactly M1.05's: nothing changes
  for existing users.
