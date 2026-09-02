# #460 plan-review pane — decision log

Date: 2026-09-02

## Decisions

1. The TUI consumes Explain Mode rather than building a second semantic model.
   `panes::plan_review::project(&Plan)` calls
   `git_vista_protocol::explain(plan)` and translates its six typed topics and
   exhaustive `ExplanationFact` vocabulary into terminal rows. The terminal
   mapping is intentionally dense; operation effects remain derived by Explain
   Mode.

2. The approval authority is the original `/api/plan` response, not a mutable
   Rust `Plan`. `PlanReviewPane::from_wire` parses once, derives the projection
   and retry key, then discards the parsed `Plan`. The pane retains only the
   original byte buffer. `PlanApproval` has private fields and can only be
   minted by the pane, so no edit surface exists between review and POST.

3. Review is modal. While it is open, ordinary focus, activation, and refresh
   actions are ignored. `a` approves; `Esc` refuses locally. Once approval is
   submitted, a second key press cannot mint another request and local refusal
   waits for the server's answer rather than pretending an in-flight write was
   cancelled.

4. A 409 never inherits a guessed cause from response prose. The one exact
   expiry sentence the server owns becomes `Plan expired`; every other 409
   becomes `Plan is stale`. In particular, the server's current “repository
   changed” generation message is not repeated in the pane. This is the #444
   lesson: a conflict proves the reviewed plan can no longer execute, not why
   the mismatch arose.

5. Staleness and expiry are terminal review outcomes. Neither triggers a
   refresh, rebuild, or retry. Only HTTP 401 receives the existing persistent
   client's one reauthentication retry, reusing the exact body and exact
   idempotency key.

6. The idempotency key is deterministic per plan:
   `tui-{operation_hash}-{issued_at}`. This mirrors the existing MCP execute
   bridge's reasoning while giving the TUI its own prefix.

## Acceptance evidence

| Criterion | Evidence |
|---|---|
| Preconditions, before → after ref changes, risk, advisories, recovery | `crates/gv-tui/src/panes/plan_review.rs:51`, `:67`, `:90`; exhaustive projection tests at `:577` and `:625`; visible modal test at `crates/gv-tui/src/ui.rs:819` |
| Honest 409 refusal, no invented cause | Classification/message at `crates/gv-tui/src/panes/plan_review.rs:294-318`; tests at `:664`, `:686`; rendered proof at `crates/gv-tui/src/ui.rs:843` |
| Expiry and generation mismatch surfaced, never silently retried | Expiry/stale outcomes at `crates/gv-tui/src/panes/plan_review.rs:294-318`; one-POST test at `crates/gv-tui/src/data.rs:458`; reducer does not refresh at `crates/gv-tui/src/app.rs:1048` |
| Approval submits the received plan unmodified | Immutable wire holder at `crates/gv-tui/src/panes/plan_review.rs:262`, `:339-378`; byte-identity test at `:639`; transport assertion at `crates/gv-tui/src/data.rs:458` |
| Rendering is a host-tested pure function of `Plan` | `project(&Plan)` at `crates/gv-tui/src/panes/plan_review.rs:51`; 46-variant host corpus at `:625`; no terminal, HTTP, clock, or app state enters that function |

## Adversarial mutation record

Each mutation below was applied independently, its named test was observed red,
and the production code was restored before the final green run.

| Invariant | Mutation A (caught) | Mutation B (caught) |
|---|---|---|
| Honest staleness | Echo the 409 body as an ordinary refusal | Claim “because the repository changed” in the stale message |
| Exact approval | Re-serialize the parsed `Plan`, losing response whitespace | Submit an empty body instead of the retained bytes |
| Complete/accurate fact projection | Drop the Preconditions topic | Render `after → after` instead of `before → after` |
| No silent replay | Retry 409 like 401 | Leave review in `AwaitingDecision`, allowing a second approval |

The final focused run is 90/90: 89 `gv-tui` unit tests plus the crate's
server-dependency boundary integration test. Workspace clippy is green with
warnings denied. `cargo test --workspace` compiled the workspace and then
failed only in the server binary's host sandbox battery: 765 passed, 340
failed, 6 ignored, with the failures rooted in this managed environment's
`bwrap: No permissions to create a new namespace`. The changed crate remained
fully green in that run.
