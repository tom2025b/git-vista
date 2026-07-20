# ADR 0013 — Protocol v2 for the clone response; `set_current` returns its own handle

- **Status:** Accepted — implementation pending (`feature/persistent-clones`)
- **Date:** 2026-07-20
- **Milestone / issue:** #121 persistent-clones — surfaced during adversarial
  review of the implementation plan
  `docs/superpowers/plans/2026-07-19-persistent-clones.md`
- **Supersedes / superseded by:** — (extends ADR 0002's negotiation mechanism;
  does not supersede ADR 0008)

## Context

ADR 0008 moves `/api/clone` from a single throwaway clone to a persistent,
named one, and has the handler return the fresh clone's `RepositoryDescriptor`
so the frontend can jump straight to the mode picker (instead of the old
plain-text `"Cloned <url>"` body). That reshapes an `/api/*` response body
from text to JSON — under ADR 0002/M1.02 that is a wire-contract change an
older peer can misread, not an additive one, and the implementation plan for
#121 shipped it without touching `PROTOCOL_VERSION`.

Two problems surfaced during the plan's adversarial review, both in the same
handler:

1. **Silent contract skew.** A client built against the old contract
   (`resp.text()`-only, ignores the body) is unaffected talking to the new
   server. But a client built against the *new* contract
   (`resp.json::<RepositoryDescriptor>()`) hitting a server that hasn't yet
   picked up the change — the exact "long-lived cached tab across a redeploy"
   scenario ADR 0002 exists to catch — gets a JSON-parse error and reports the
   clone as failed even though it succeeded on disk. `PROTOCOL_VERSION` was
   never bumped, so the existing negotiation check can't see this contract
   moved.

2. **A selection race in the handler itself.** The handler called
   `state::set_current(&dest, Visualize)` (which returns nothing) and then
   separately called `state::current_handle()` to read back what it had just
   set, to build the response descriptor. `set_current` mutates a
   process-global (`CURRENT`); a `POST /api/select` (or a second `/api/clone`)
   landing between those two calls can move `CURRENT` before the second read,
   so `/api/clone` would return a *different* repository's descriptor than
   the one it just cloned.

## Decision

1. **Bump the wire-protocol version.** `PROTOCOL_VERSION`,
   `MIN_CLIENT_PROTOCOL`, and `MAX_CLIENT_PROTOCOL` in
   `git-vista-protocol/src/version.rs` move from `1` to `2`, together (no
   window widening — this server build supports exactly one contract). This
   is the exact mechanism ADR 0002 built for a contract-shape change: a
   mismatched peer is refused at the middleware with the existing
   "Update Required" screen, rather than misreading a response body it
   doesn't recognize.
2. **`state::set_current` returns `Option<RepositoryHandle>`** (the handle it
   just registered; `None` in degraded mode) instead of `()`. `/api/clone`
   builds its response descriptor from that return value directly — the
   handle it already computed — instead of a second, independent read of
   `CURRENT`. This closes the race: the response is always the clone this
   request just made, never whatever `CURRENT` happens to hold a moment
   later.

## Alternatives considered

- **Leave `PROTOCOL_VERSION` at 1; document the skew as an accepted risk**
  (single-operator, one binary serves its own static frontend, so the skew
  window is narrow — see the plan's design-decision log). Rejected: the tool
  built to close this exact gap (ADR 0002) already exists and costs three
  constant edits to use; there's no reason to ship a documented hole when
  fixing it is nearly free.
- **Detect the selection race with a version counter or a re-check inside the
  handler**, leaving `set_current`'s signature untouched. Rejected: more
  moving parts than necessary — `set_current` already computes the handle
  before it returns; discarding it and re-deriving it via a second global
  read is the actual bug, not something to work around.
- **A separate `set_current_and_return_handle` function**, so the two other
  call sites (`main.rs` startup, one `state.rs` test) don't need to change.
  Rejected: two near-identical functions invite drift, and both existing
  callers already discard the return value for free — `Option` is not
  `#[must_use]`, so the signature change is a no-op for them.

## Consequences

- `/api/clone`, `/api/select`, and any future response-shape change share the
  same graceful-refusal path for a stale client — no new client-facing test
  surface for #121; the existing `update_required.rs`/middleware coverage
  already exercises it.
- Every `/api/*` request must now carry protocol header `2`. A client still
  sending `1` (a tab that loaded before this deploy) gets the Update Required
  screen on its next request instead of a confusing clone failure.
- `set_current`'s signature change is internal (`pub(crate)`) — no wire
  impact, and its two existing callers are unaffected.
- Recorded as its own ADR rather than folded into ADR 0008: this is a
  protocol-versioning decision under ADR 0002's mechanism, not a
  persistent-clones-storage decision under ADR 0008's. Keeping them separate
  keeps each ADR about one concern.
