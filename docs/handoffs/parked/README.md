# Parked handoffs — written, then not sent, and why

A handoff in here was drafted for a cloud session and **withdrawn before it was
dispatched**. It is kept rather than deleted for the same reason branches are
never deleted in this repository: the reasoning is the teaching material, and a
plan that turned out to be wrong is more instructive than one that happened to
be right.

Each one names, at the top of this file, what killed it.

## `CLOUD-X-issue-326-planner-shape.md` — withdrawn 2026-08-25

Drafted as CLOUD-4 of the 25 August batch. Withdrawn after the batch's own
truth-check fan-out refuted three of its claims:

1. **Its measurements were eighteen days stale.** It cited `planner.rs` at
   6,244 lines and `shape()` at 614 lines holding 22 match arms, from the
   issue's 2026-08-05 measurement. Actual state on 25 August: `planner.rs` is
   **3,376** lines — commit `50350e5b` extracted 23 local executors into seven
   domain modules on 23 August — and `shape()` is **~798 lines holding 31
   arms**, having grown as new operations landed. Both numbers moved, in
   opposite directions, and the handoff was confident about both.

2. **Issue #326 explicitly forbids the approach the handoff instructed.** Its
   own words: *"Do NOT do this as a single large refactor… `planner.rs` is the
   most contended file in the repo… a big-bang move maximises conflict against
   in-flight branches."* Its recommended approach is milestone-tied and
   incremental — *"when a milestone touches an operation, move that operation's
   `shape()` arm into its per-operation module at the same time"* — with no
   deadline. Splitting the sweep into per-arm commits, as the handoff proposed,
   does not answer that objection: it is still one dedicated sweep landing in
   one PR.

3. **The contention the issue warned about was live inside this very batch.**
   CLOUD-2 (#493/#494) also edits `planner.rs` and `planner/stash.rs`. The one
   argument that might have excused a big-bang move for a cloud batch — that
   cloud sessions do not collide with in-flight local branches — did not hold,
   because the collision was with another member of the same batch.

**What replaced it:** #438, the process-global test race, which had failed CI
that same morning and collides with nothing.

**What #326 needs instead:** nothing, for now. It is working as designed — a
standing instruction to move an arm when a milestone touches its operation.
The corrected measurements were posted to the issue so the next reader does not
inherit the stale ones.
