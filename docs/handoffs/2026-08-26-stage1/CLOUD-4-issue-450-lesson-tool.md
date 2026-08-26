# CLOUD-4 · #450 — The MCP lesson tool: structured teaching data from the live repository

**Stage 1 of 4.** Newly unblocked: this issue waited on #92, whose core merged
today (PR #544, ADR 0091). The thing it was waiting for — `explain(&Plan)` —
now lives in `git-vista-protocol`, which `git-vista-mcp` can already see.

```yaml
task_id: gv-cloud-4-450
issue: 450
branch: claude/cloud-4-450-lesson-tool
base: main
kind: FEATURE — a read tool on the MCP server
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
allowed_paths:
  - crates/git-vista-mcp/
  - crates/git-vista-protocol/src/          # only if a shared type genuinely belongs there
  - docs/adr/
forbidden_paths:
  - crates/git-vista/
  - ci/browser/
  - crates/git-vista-server/src/handlers/   # this is an MCP tool, not a new HTTP route
github_writes: open the PR. Do not merge.
```

## What it is

A **read** tool on `git-vista-mcp` that, given the current repository state — a
conflict on disk, a sequence mid-flight, a plan that would run — emits a
**structured lesson document**: the real state, the typed facts about it, and
explanation blocks.

## The placement decision is settled, and it is the point of the issue

**Structured lesson DATA, not HTML.** Tom's stated preference was the tool
living in `git-vista-mcp`; fable's review agreed on the home and not the
payload, for the repository's own stated reason: *transport is not domain*, and
rendering taste does not belong in a Rust MCP server whose every other tool
returns typed DTOs.

The artifact/board pipeline renders. teacher-thing stores. decksmith drills.
Each stays what it is. **Emitting HTML from this tool is a failure of the
issue, not a shortcut.**

## What changed today, and why it un-blocks you

#450 says this "composes with, and depends on, #92: Explain Mode and this tool
must derive their sentences from ONE source, so the lesson a page shows and the
explanation the app shows cannot drift."

That is now satisfiable, and **the resolution is cleaner than it sounds**:

- `git_vista_protocol::explain(&Plan) -> Explanation` is the shared source. It
  is in the protocol crate, it is `pub`, and `git-vista-mcp` already depends on
  that crate.
- An `ExplanationFact` carries the plan's **own typed value** and contains no
  English at all. The English lives in
  `crates/git-vista/src/features/explain/core.rs` — the *viewer* — and you
  cannot use it and must not try.
- **That is correct, not a limitation.** This tool emits data, not prose, so it
  wants exactly the typed `Explanation` and nothing else. The two surfaces
  cannot drift because they share the facts; they differ only in rendering,
  which is what "structured data, not HTML" means in practice.

Read `crates/git-vista-protocol/src/explain.rs` and ADR 0091 before designing
the payload. Verify the claims above against the source rather than trusting
this paragraph — spec citations in this repository have been wrong seven times.

## Guardrails carried forward from the issue, restated deliberately

- **Read-only.** The ADR 0064 d7 / ADR 0069 d7 exclusions — no whole-side and no
  content resolution on the MCP surface — are untouched and restated here on
  purpose. Do not add a write.
- Composes with #448's broken-repo fixture catalogue: a lesson must generate
  from a fixture repo as easily as from a real one, and a fixture's doc comment
  is the lesson's seed text. Use `git-vista-fixtures` in the tests.
- **A known gap, found while assessing feasibility:** the MCP feed read is
  capped at 500 with no paging (`tools.rs:214`), so a long journal cannot be
  walked. Verify that line still says what this claims. Fixing it belongs here
  **or** in a sibling issue you file — your call, argued in the PR body. Do not
  silently work around it.

## Build the sandbox helper before running tests that spawn git

```
cargo build -p git-vista-server --bin gv-sandbox
```

## You cannot run the browser leg

`ci/browser/` cannot run in a cloud container (#503). This issue has no browser
surface. If it grows one, stop and say so.

## Mutation-prove, two different ways

The invariant most worth pinning: **a lesson never contains a fact the
repository did not carry.** That is #92's criterion 1 aimed at this surface, and
it is mechanically checkable exactly the way
`crates/git-vista-protocol/tests/explain_parity.rs` checks it — read that test
before writing yours, including its module doc on why half of it is anchored on
a hand-written table.

Two breaks, failing differently. Restore byte-identically, `diff -q`.

## Acceptance

1. A read tool on `git-vista-mcp` emitting typed lesson data from live
   repository state.
2. **No HTML, no rendering taste, no prose the protocol did not supply.**
3. Sentences derive from `explain(&Plan)` — one source with the app, provably.
4. Generates from a `git-vista-fixtures` broken repo as readily as a real one.
5. The 500-cap gap either fixed here or filed as a sibling issue, with the
   reasoning in the PR body.
6. Mutation-proven two ways, red assertions quoted.
7. `cargo fmt --all` · `cargo clippy --all-targets -- -D warnings` · full
   workspace suite green, **counts stated**.
8. An ADR if the payload shape is a contract others will build on — it probably
   is. Next free number is **0092**; check `docs/adr/` and coordinate, because
   CLOUD-2 may also want one.

## What you must NOT do

- No writes on the MCP surface. None.
- No HTML.
- No viewer changes, no new HTTP route.
- Do not merge your own PR.
