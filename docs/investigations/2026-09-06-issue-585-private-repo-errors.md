# Issue #585 — private repository failures

The credential-bearing entry point is `handlers/clone.rs`: it runs Git's
smart-HTTP transport, not the GitHub REST API. Git returns an exit status and
redacted stderr; its fatal exit code alone cannot distinguish authentication,
access refusal, and network failure.

Keep the decision native and framework-free. Classify only a failed GitHub
HTTP transfer, using token presence plus Git's authentication/access markers.
Preserve ordinary network errors, other hosts, successful public/empty clones,
and errors from the later credentialless checkout. A 404 cannot establish that
a repository exists or that a token was accepted; do not invent either fact.

Required messages:

- private repo — no token configured
- private repo — token invalid or expired
- private repo — token lacks `repo` scope, or this account cannot see it

Tests inject transport outcomes and pin exact messages, HTTP 400 responses,
partial-clone cleanup, failure status polling, and idempotent error replay.
The existing credential redaction and two-phase clone boundary stay in place.
Token resolution order is outside this change. The browser already propagates
the clone response and failed poll message; no wasm decision or wire type changes.

No new ADR planned: this applies the existing absence/failure and credential
containment contracts to the clone error response without a new wire type.

Signed: codex
