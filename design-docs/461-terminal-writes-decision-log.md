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

## 8. Transfer progress and cancellation checkpoint

- The authenticated client now shares its in-memory session across clones.
  A generation-aware 401 refresh prevents two concurrent requests from both
  consuming bootstrap tokens: if another request already replaced the stale
  cookie, the retry reuses that fresh session.
- Only approved execution moves to a second worker thread. Catalog/history,
  selection, planning, tags, and polling retain their ordered worker. This is
  the minimum concurrency needed for `GET /api/operations/by-key/<key>` to
  answer while `/api/execute-plan` is still blocked on the remote operation.
- The review pane derives a non-authoritative `cancellable` display fact from
  the same three typed variants the server supports: FetchRemote, PullBranch,
  PushBranch. It retains no editable Plan. Approval starts by-key lookup with
  the exact idempotency key, then bounded 500 ms status polling renders the
  typed stage, transfer phase, and optional percent.
- `c` queues cancellation even before the operation id is discoverable, then
  posts to the typed id's cancel route once admitted. Push copy deliberately
  warns that cancellation cannot establish that nothing was published.
- The server already globally sets `GIT_TERMINAL_PROMPT=0`, forces
  `-c core.askpass=` for Remote network need, retains sanctioned credential
  helpers/SSH agents, redacts remote output, and returns actionable typed
  authentication failures. The TUI neither prompts nor interprets prose; its
  bounded HTTP transport surfaces the server response.

Checkpoint verification: 104 TUI unit tests plus 1 write-boundary integration
test pass. The concurrency test blocks execute-plan deliberately and proves
by-key lookup answers before releasing it. Clippy over all targets with
warnings denied is clean.

## Acceptance evidence

- **Branch create/checkout/merge/delete; commit, amend, hooks and signing; tag
  create/list/delete; fetch, pull, push — MET.** The closed grammar builds the
  branch operations at `crates/gv-tui/src/commands.rs:52-76`, commit/amend at
  `:79-99`, every tag form (including signed annotated tags and scoped listing)
  at `:101-142`, and fetch/pull/push at `:42-47` and `:144-191`. Planning is
  one typed POST at `crates/gv-tui/src/data.rs:150-153`; tag listing is the
  read-only scoped GET at `:154-157`. Commit and amend deliberately retain the
  server's existing policy: hooks run inside the same sealed commit spawn
  (`crates/git-vista-server/src/planner/commit_exec.rs:349-360`), effective
  `commit.gpgsign` is read at `:542-558`, and hook/signing failures remain typed
  at `:637-654` and `:657-704`. There is no unsupported caller-side promise to
  toggle commit signing or bypass hooks.
- **Force-with-lease advisories before approval — MET.** Only the explicit
  `--force-with-lease=<OID>` form can build `ForcePublish::WithLease`
  (`crates/gv-tui/src/commands.rs:163-190`); bare `--force` is rejected. The
  existing pane renders every server `ExplainMode` section
  (`crates/gv-tui/src/panes/plan_review.rs:67-77`) and maps every typed advisory
  to an advisory row at `:109-133`, before its sole approval method at
  `:368-378`.
- **Network progress and supported cancellation — MET.** Approval uses its
  exact idempotency key to discover the typed operation id; status and cancel
  answers are scoped to that id (`crates/gv-tui/src/app.rs:536-615`). The 500 ms
  bounded poll and queued-cancel path are at `:638-689`; only the server-backed
  FetchRemote/PullBranch/PushBranch set advertises cancellation at
  `crates/gv-tui/src/panes/plan_review.rs:341-348`. The corresponding by-key,
  status and cancel endpoints are `crates/gv-tui/src/data.rs:159-176`. Approved
  execution alone is concurrent, leaving those reads serviceable while the
  execute POST runs (`:316-345`).
- **Credentials never silently hang — MET.** The server globally disables
  terminal prompting (`crates/git-vista-server/src/main.rs:185-194`) and the
  remote sandbox forces an empty askpass configuration through the real spawn
  seam (`crates/git-vista-server/src/git_cmd.rs:1588-1625`). The wire retains
  distinct actionable authentication and blocked-helper outcomes
  (`crates/git-vista-protocol/src/dto.rs:605-651`); the terminal displays the
  server response and does not infer a refusal kind from its prose.
- **No new git spawn sites — MET.** The TUI's complete production-source census
  rejects process, file and environment write APIs
  (`crates/gv-tui/src/main.rs:378-403`), and its dependency boundary keeps the
  server out of the client. The repository-wide native spawn allowlist remains
  unchanged and is checked at
  `crates/git-vista-server/src/argv_boundary.rs:484-539`; its reverse/live-entry
  check is at `:541-603`.

## Mutation evidence

Every temporary mutation below was restored before the green run.

| Invariant | Mutation A -> observed RED | Mutation B -> different observed RED |
|---|---|---|
| Closed grammar; no argv language | Unknown top-level input returned `Help` -> `unknown_or_malformed_input_never_becomes_an_operation` reported `git status unexpectedly parsed`. | Bare `--force` was accepted -> the same matrix reported `push main origin --force unexpectedly parsed`. |
| Writable selection precedes the shared review pane | Removed the acknowledged-worktree gate -> `writable_selection_precedes_a_closed_command_and_shared_plan_review` reported `selection was not acknowledged`. | Replaced the parsed delete operation with `StageAll` -> typed equality reported `DeleteBranch` versus `StageAll`. |
| Exact typed plan transport | Changed `PLAN_PATH` to `/api/execute-plan` -> `selection_and_planning_use_plain_posts_with_typed_exact_bodies` panicked `unexpected POST /api/execute-plan`. This initially **survived** because the test reused the production constant; the test now pins literal wire paths. | Serialized `StageAll` instead of the requested delete -> the strengthened test reached the right endpoint but failed typed body equality (`StageAll` versus `DeleteBranch { topic }`). |
| Typed progress and supported cancellation | Rendered `percent - 1` -> `remote_execution_lookup_drives_typed_progress_and_cancellation` observed `receiving 41%`, not 42%. | Removed `FetchRemote` from the supported cancel set -> the same flow produced no `CancelOperation` request for the typed id. |
| Execution concurrency without reordering ordinary reads | Served execute-plan inline -> `operation_lookup_is_served_while_the_approved_execution_is_still_running` timed out after five seconds with `lookup was blocked behind the running execution`. | Spawned every ordinary read concurrently -> `the_worker_answers_every_request_in_order_without_blocking_the_caller` received `[beta, alpha]`, not `[alpha, beta]`. |
| Tag listing is a scoped read, never a disguised write | Dropped `?repo=w1` -> `tag_listing_is_a_scoped_read_not_a_write_disguised_as_a_plan` failed endpoint equality. | Parsed `tag list` as `Plan(StageAll)` -> `all_tag_forms_are_closed_and_signing_is_never_silently_dropped` failed `ListTags` equality. |
| No client-side process/file write seam | Inserted a production `Command::new` probe -> `production_code_never_writes_files_env_or_spawns_processes` named the forbidden spawn token and file. | Replaced it with an `fs::write` probe -> the same complete-source census failed through its distinct file-write arm. |
| 409 classification uses wire status, never English prose | Classified 409 as generic `Refused` -> `a_generation_conflict_says_only_that_the_plan_is_stale` failed `Refused` versus `Stale`. | Reintroduced an `expired` substring branch -> `every_409_is_stale_regardless_of_english_prose` failed on the different body `refs/heads/main moved`. |

## Final verification

- `cargo test -p gv-tui --all-targets`: **104** bin-unit tests plus **1**
  dependency-boundary integration test passed; zero failed.
- `cargo clippy -p gv-tui --all-targets -- -D warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `cargo test -p git-vista --bins`: **806 discovered; 804 passed, 2 ignored**;
  zero failed. This is the real bin target, not the one-byte `lib.rs` false
  green.
- `cargo test -p git-vista-server argv_boundary::`: **11 passed**; zero failed.
- `sandboxed_forces_askpass_hardening_for_remote_network_need`: **1 passed**.
- `network_command_prepends_forced_askpass_hardening_before_user_args`:
  **1 passed** when rerun with the required user-namespace permission (the
  restricted first run stopped in bubblewrap fixture setup before the product
  assertion).
