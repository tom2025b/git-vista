# #461 — terminal writes decision log

Date: 2026-09-02
Branch: `feature/m10.06-461-terminal-writes`
Base: `2dbd57ad`

This is the running implementation and review record for M10.06. It is
updated as decisions are made and will be posted on the pull request.

## 1. One write path: typed operation -> `/api/plan` -> existing review pane -> `/api/execute-plan`

The terminal will not call the operation-specific execution endpoints and it
will not construct argv. Its command surface constructs one existing
`GitOperation`, POSTs that value to the shared build-only `/api/plan` endpoint,
and gives the exact returned bytes to M10.05's plan-review pane. Approval sends
those same bytes to `/api/execute-plan`; refusal sends nothing.

Why: this is the planner path the browser and MCP already share, and the review
pane is already the sole authority capable of minting a `PlanApproval`. A
second confirmation or direct-execution path would make it possible for the
terminal's account of risk to drift from the server's.

## 2. The existing vocabulary covers the requested operations

The closed `GitOperation` enum already represents branch create, checkout,
merge, safe delete and force delete; commit and amend; local tag create/delete;
remote tag push/delete; fetch, pull and push. `PushBranch` carries
`ForcePublish::WithLease`, and M4.32's advisories are fields of the resulting
`Plan`, so M10.05 renders them without terminal-specific logic.

Commit hooks and commit signing are deliberately not caller-selectable fields.
The server applies its disclosed sandbox hook policy, reads the repository's
effective `commit.gpgsign`, and returns typed commit/amend failure kinds. The
terminal must not add `--no-verify`, suppress configured signing, or claim it
can request signing when the wire cannot say that. Tag signing *is* represented
by `TagAnnotation { sign: true }` and will be exposed.

## 3. Repository activation must select before planning

History/detail reads address a worktree with `?repo=<opaque worktree id>`, but
`POST /api/plan` intentionally takes only a bare `GitOperation` and plans
against the authenticated session's selected worktree. Therefore activating a
catalog row must POST the existing `SelectRequest { worktree, mode: Active }`
before that row becomes writable. A failed selection leaves no active write
target. Selection changes server session state, not Git state, and has no
`GitOperation`; it is not disguised as a planned Git write.

## 4. Commands are an operation builder, not an argv language

The terminal will expose a `:` command palette with a documented, closed set
of verbs. Parsing yields `GitOperation` directly and rejects every unknown
verb/flag before network I/O. It is not a shell, never forwards arbitrary
tokens, and has no escape hatch. Messages consume the remainder of the input so
ordinary spaces do not require a shell parser.

## 5. Progress and cancellation use operation identity already on the wire

`/api/execute-plan` is operation-tracked but its HTTP response arrives only
after the operation is terminal. The approval's existing idempotency key is the
early handle: while the approved POST runs, the terminal polls
`GET /api/operations/by-key/{key}` until it receives the server's `OperationId`,
then polls `GET /api/operations/{id}` for `OperationStatus`. Its typed
`TransferProgress` drives the visible phase/percentage. Cancel posts to
`/api/operations/{id}/cancel`; the server's typed operation determines whether
the cancellation latch is supported. No client-side claim is made that a
cancelled push published nothing.

Polling rather than SSE is deliberate for this synchronous loopback client:
the status DTO carries the same latest typed transfer progress, avoids adding a
second streaming HTTP parser, and remains bounded by the terminal event tick.

## 6. Refusal reasons are never inferred from English prose

M10.05 currently distinguishes `Expired` from `Stale` by matching the server's
exact sentence. This branch will remove that distinction: the execute-plan wire
does not carry a typed refusal reason, so a 409 can honestly establish only
that the reviewed plan cannot execute. The terminal will say that and ask for
a fresh plan. Operation-status, fetch/pull and signing DTOs may be parsed where
the wire actually carries typed reason fields; prose remains display text.

## 7. First wired checkpoint — selection, command grammar, plan review

- Activating a writable catalog row now issues the existing Active-mode
  `/api/select` request. The command palette remains unavailable until that
  exact worktree is acknowledged; read-only catalog rows remain readable but
  cannot open the palette.
- `:` owns raw character input until Enter or Esc. Its closed grammar maps
  branch create/checkout/merge/delete/force-delete, commit, amend, every local
  and remote tag write, fetch, pull, and push directly into `GitOperation`.
  Unknown verbs and flags (including bare `--force`) produce no request.
- `/api/plan` receives the serialized typed operation through the ordinary
  authenticated POST seam. The exact response bytes go to the existing
  `PlanReviewPane`; approval still mints the only `PlanApproval` and submits
  those bytes unchanged through the idempotent execution transport.
- `tag list` is deliberately a scoped `GET /api/tags?repo=<opaque id>`, not a
  fake GitOperation and not a write wearing a plan.

Checkpoint verification: `cargo test -p gv-tui --all-targets` counted 99 unit
tests plus 1 write-boundary integration test, all passing; clippy over all
targets with warnings denied is clean.

## Acceptance evidence

Populated with final file:line locations and an explicit `NOT MET` for any gap
before merge.

## Mutation evidence

Each invariant test will be mutation-proved in two distinct ways and recorded
here with the changed line, failing test and different failure mode.
