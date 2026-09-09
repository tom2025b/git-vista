# #765 — delete-tag selector implementation

Closes #765.

## Decision and scope

Follow PR #769's identity shape and ADR 0140's destructive-write semantics:
`DeleteTagRequest` requires `repo`, the opaque `WorktreeId` of the reading
that supplied the tag. The handler parses it and calls
`plan_and_execute_matching`. The comparison uses the same resolved target
that planning and execution use. A mismatch refuses the operation with HTTP
412 and `precondition_failed`; it does not redirect execution to the named
worktree. Omitted/null selectors return 422; malformed ids return 400.
The route census now requires the matching planner entry point.

The production delete affordance is the graph context menu. Activity's
`GET /api/tags` list is display-only. The graph frame supplies both tag badges
and `worktree_id`; `render/nodes.rs` captures that id into `MenuData.tag_repo`.
The menu copies it into `DeleteLocalTag`, and confirmation and dispatch retain
it unchanged. Entry points without a tag-bearing frame cannot offer a delete.
This implements the handoff's selector scope without adding an Activity action.

Git's `refs/tags/*` are shared across linked worktrees. The issue's premise
that sibling worktrees can simultaneously hold different values for the same
tag name is incorrect. The regression fixture asserts identical tag listings,
including target oids, along with equal `RepositoryId`s and distinct
`WorktreeId`s. Refusing a stale worktree selection is still the requested
precondition. A successful delete removes the shared ref for both worktrees.

A client-observed oid witness is outside this handoff's selector-only scope.
The existing planner pins the unpeeled tag object at planning time for its CAS
and recovery contract; that does not prove the tag stayed unchanged since
the earlier client read.

## Tests

`handlers::tags::delete_selector_tests` drives JSON extraction, the actual
handler, the planner, and the API error wrapper. It checks lightweight and
annotated tags, preservation of the full shared listing on refusal, typed
412 errors, non-disclosure of repository ids, successful matching deletes,
and missing, null, malformed, and unknown selectors.

`ci/browser/tests/tag-selector.spec.mjs` drives the compiled client. A second
tab switches the shared server session to a sibling after the first tab opens
its confirmation. The test checks the exact captured request body, typed
refusal, and unchanged tag ref in both worktrees, followed by a fresh matching
delete. This is a reachable browser scenario even if switching repositories
within the same tab tears down its confirmation.

The ignored entry point
`captured_tag_selector_browser_contract_for_mutation_check` builds wasm and
runs this browser contract for future mutation checks.

## Verification record

Passed:

- Protocol library: 224 tests.
- Frontend host binary: 1,091 tests; 2 existing ignored tests.
- Server selector, argument-boundary, and planner contract suites: 159 tests.
- Restored server selector baseline after mutation checks: 3 tests.
- Trunk wasm build.
- Browser selector contract: 1 test, including refusal and successful deletion.

Server mutation checks (all caught by
`a_tag_from_a_sibling_worktree_is_refused_with_a_typed_412`):

| Mutation | Observed failure |
| --- | --- |
| Handler calls ordinary `plan_and_execute` | The shared tag was deleted; full-list preservation failed. |
| Planner checks `resolve_worktree(expected).is_none()` instead of worktree equality | The registered sibling passed the weaker membership check; shared-tag preservation failed. |
| `ErrorCode::from_status(412)` returns `BadRequest` | The tag survived and HTTP status stayed 412, but the typed envelope assertion failed: `BadRequest` versus `PreconditionFailed`. |

These are manual mutation runs against the uncommitted worktree, following
ADR 0140's documented exception: a tool cloning HEAD would not test this
change. Each edit required exactly one match, ran the actual compiled test,
and restored its source file from a byte-exact backup in a `finally` block.
The runner checks for both a nonzero test exit and the expected assertion
failure, so a compile error cannot count as a caught mutation. Server logs
are `/tmp/gv-765-server-{bypass,membership,error-code}.log`.

Client mutation checks (both compiled and caught by the browser contract):

| Mutation | Observed failure |
| --- | --- |
| API dispatch replaces the captured id with `fetch_frame().await`'s current `worktree_id` | The POST body carries the sibling's id instead of the original read's id; exact-body assertion fails. |
| Frame capture discards the parsed id with `.filter(|_| false)` | The graph still shows the tag, but its delete menu item is absent; the visible-action assertion fails before confirmation. |

Client source files were also restored byte-for-byte. Logs are
`/tmp/gv-765-client-{live-selector,missing-capture}.log`, with successful
wasm compilation recorded in the corresponding `-build.log` files.
An initial live-selector mutation failed to compile because `fetch_frame`
returns `HistoryFetchError`; it was not counted. The actual proof explicitly
converts that error and compiles before running the browser.

Host lint passed for the three changed crates and all their targets:
`cargo clippy -p git-vista-protocol -p git-vista-server -p git-vista --all-targets -- -D warnings`.
Formatting and `git diff --check` passed.

Final restored wasm build and browser regression passed (1 browser test).
Wasm lint also passed:
`cargo clippy -p git-vista --target wasm32-unknown-unknown --all-targets -- -D warnings`.
The untouched planner and error-code files have no diff after restoration.
No mutation remains in the source or built browser bundle.

## Review and integration

Worktree: `/home/tom/projects/gv-765`.
Branch: `fix/765-delete-tag-request-selector` (KEEP; never delete).
Cargo target: `/home/tom/.cargo-targets/gv-765`.

The user's follow-up authorizes committing, pushing, and opening a draft PR.
Merge remains subject to the separate review requested in the original
handoff. Preserve the branch after integration.

Signed: codex · 2026-09-09
