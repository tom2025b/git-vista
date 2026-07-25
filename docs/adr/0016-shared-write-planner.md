# ADR 0016 — Every write action executes through one shared planner

- **Status:** Accepted
- **Date:** 2026-07-24
- **Milestone / issue:** M1.06b — Migrate existing write actions onto the
  shared planner (#143, child of #59)
- **Supersedes / superseded by:** — (builds on ADR 0015's vocabulary/Plan
  schema; execution-time enforcement is #145)

## Context

ADR 0015 (#142) defined the closed `GitOperation` vocabulary and the
reviewable `Plan` schema, but every write handler still constructed and ran
its own git argv ad hoc — fifteen mutations across five handler files, each
with its own copy of validation, spawning, error forwarding and journaling.
The Foundation exit criterion needs one seam where #144 can close the
browser's escape hatch and #145 can enforce staleness/generation/expiry; that
seam cannot exist while execution is scattered.

## Decision

### One module, one entry point

`git-vista-server::planner` owns the whole write path. Its single entry,
`plan_and_execute(GitOperation)`, is what every write handler calls; the
handlers keep only their request validation (unchanged wording and status
codes) and the construction of the typed operation. The pipeline inside is
**build → validate → execute**:

- **Build** assembles the full `Plan` for the operation against the live
  repository: the selection's opaque repository/worktree tokens, the live
  generation (ADR 0001, computed from HEAD + refs today; index/worktree
  digests join when #145 makes generation checks load-bearing), the
  operation's SHA-256 hash (`sha2`, server-side), a 300-second expiry window,
  and the per-operation risk / preconditions / expected-ref-changes /
  recovery, following exactly the per-variant patterns ADR 0015's golden
  fixture pinned. Observation is **best-effort by design**: a failed read
  thins the plan instead of refusing the operation, so execution still
  surfaces git's own error exactly as before.
- **Validate** runs the structural checks — the hash matches the operation,
  the plan hasn't expired. With plans built and executed in one request these
  can only fail on a server bug; they exist as the exact seam #145 widens
  into generation-equality, precondition and staleness enforcement for
  client-reviewed plans.
- **Execute** is the **only place in the server that constructs a mutating
  git argv**. The per-operation execution bodies moved here verbatim from the
  handlers: same git commands, same journal events, same success/failure
  texts and status codes. Pre-mutation observations (the journal's "before"
  oids, delete's restore-point tip, reset's compare-and-swap tip) are
  captured once during plan building and reused by the executor, so nothing
  is read after the mutation that needed the before-state.

### Exact commit ids at the boundary

The vocabulary pins exact oids (`CommitOid`, 40/64 lowercase hex). The UI
always sends full ids, which are taken as-is — identical argv to before. A
hand-crafted symbolic or abbreviated id is resolved through `git rev-parse`
while building the operation; one that doesn't resolve is refused at the
boundary with a git-shaped message instead of git's own later refusal. That
narrow error-text delta (hand-crafted requests only) is the accepted cost of
operations that carry exact, reviewable oids.

## Alternatives considered

- **Enforcing preconditions at execution time now.** Rejected: that is
  #145's scope, and evaluating them here would change refusal texts and
  ordering in what #143 requires to be a behavior-preserving refactor. The
  preconditions ride the plan as reviewable data until #145 arms them.
- **Placing the planner in `git-vista-git` or `git-vista-protocol`.**
  Rejected: execution needs the journal, the selection state and the catalog
  — server concerns; the protocol crate stays pure/wasm-safe (ADR 0002/0015).
- **Each handler building and validating its own plan.** Rejected: keeps N
  copies of the shape logic and leaves no single choke point — the exact
  drift this milestone exists to close.

## Consequences

- "No write handler constructs git argv" is now a grep-able invariant:
  outside `planner.rs`, the only `git` spawns in the server are the read
  helpers (`git_cmd`), the clone endpoint (not an operation, per ADR 0015's
  scope decision) and test fixtures.
- #144 (close the arbitrary-argv escape hatch) and #145 (staleness/
  generation/expiry enforcement) attach to one seam instead of fifteen
  endpoints.
- The security model's "Build argv only from typed operation planners and
  validated domain values" (Command Execution) is now implemented for every
  served-repository mutation.
- Every write request now also performs the plan-building reads (HEAD, refs,
  the odd tip resolution) — a few extra read-only git/gix calls per mutation,
  accepted as the cost of an always-present reviewable plan.
