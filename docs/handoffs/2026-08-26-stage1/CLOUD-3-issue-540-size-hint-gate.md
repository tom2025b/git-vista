# CLOUD-3 · #540 — An exact size states a byte count, not readiness

**Stage 1 of 4.** The smallest lane, and the one with a diagnosis already paid
for: codex found this on 26 August, verified it has no current trigger, and
deliberately deferred it rather than bolt a large speculative change onto a PR
that was otherwise ready.

```yaml
task_id: gv-cloud-3-540
issue: 540
branch: claude/cloud-3-540-size-hint-gate
base: main
kind: FIX — server middleware, no wire change expected
prior_diagnosis: design-docs/reviews/2026-08-26-codex-cloud-batch-review.md   # UNTRACKED — see below
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
allowed_paths:
  - crates/git-vista-server/src/middleware.rs
  - crates/git-vista-server/src/            # only if the fix genuinely needs a sibling
  - docs/adr/
forbidden_paths:
  - crates/git-vista/
  - ci/browser/
github_writes: open the PR. Do not merge.
```

## The prior diagnosis is UNTRACKED — read the issue instead

`design-docs/` is gitignored, so a cloud container cannot see codex's original
report. **#540's body carries the whole finding**, including the file:line and
the reason for deferral. Read the issue. Do not go looking for the review file;
it is not in your checkout and its absence is not a problem to solve.

## The defect, verbatim from the issue

`relabel_json_success` uses `size_hint().exact()` as its buffering gate
(`crates/git-vista-server/src/middleware.rs:449`), then drains the body (`:458`).

**An exact size states the remaining byte count — not readiness.** A custom body
that reports exactly two bytes but yields them later is fully awaited before the
response returns, delaying headers and defeating streaming for that route.

Verify those line numbers against the source before you touch anything. #536
merged since the finding was written and the file has moved under it.

## The judgement call that is yours, and the honest answer may be "no"

**No current endpoint triggers this.** The SSE route reports no exact size and
held; no exact-sized streaming route exists in the tree today. Codex verified
that rather than assuming it.

So this is a latent defect, and a repair that is large or risky relative to a
defect nothing can currently hit is a bad trade. The suggested direction is to
stop deciding from `SizeHint` alone and instead carry **explicit provenance for
in-memory hand-serialised responses**, so the buffering decision is made from
what the body *is* rather than from what it claims about its length.

If implementing that turns out to touch route-global body handling broadly —
the exact shape most able to break something no test covers — **stop and say so
in the PR body.** A small honest fix plus a recorded gap beats a large
speculative one. Reporting "this needs more than one PR, here is why" is a
successful outcome for this lane, not a failure.

## Build the sandbox helper before running server tests

```
cargo build -p git-vista-server --bin gv-sandbox
```

Otherwise ~323 server tests fail at spawn and look like a real regression.

## You cannot run the browser leg

`ci/browser/` needs a display and a live server; a cloud container cannot run it
(#503). This issue should have no browser surface. If it grows one, stop.

## Mutation-prove, two different ways

The issue names the essential one: **a test that fails if the gate goes back to
trusting `size_hint().exact()`.** Add a second break that fails *differently* —
weaken the mechanism rather than removing it — and quote both red assertions
verbatim.

Restore byte-identically and confirm with `diff -q`.

## Acceptance

1. A body reporting an exact size but yielding late does not delay headers.
2. A test that goes red if the gate returns to trusting `size_hint().exact()`.
3. Mutation-proven two ways, red at different assertions where the code allows.
4. `cargo fmt --all` · `cargo clippy --all-targets -- -D warnings` · full server
   suite green, **count stated**.
5. If ADR 0084's claims about `rewrap_error` change, **fix the ADR in the same
   commit.** An ADR that overstates what a test catches is how finding 3 in the
   same review happened.
6. Or: a PR that explains, with evidence, why the safe repair is larger than
   this issue should carry — and what it would take.

## What you must NOT do

- No viewer changes, no browser suite.
- Do not weaken an assertion to make something pass.
- Do not merge your own PR.
