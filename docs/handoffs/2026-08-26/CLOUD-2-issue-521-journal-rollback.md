# CLOUD-2 — #521: the batched journal is not backward-readable — decide the rollback story, honestly

**Batch of 2026-08-26 · merge order 2 of 5 (before CLOUD-3 — you share the activity/journal subsystem; your format decision lands first).**

```yaml
task_id: gv-521-cloud-2
issue: 521
branch: cloud/521-journal-rollback
base: main            # 682f3061 or later
adr_number: 0085      # ASSIGNED — whether you amend ADR 0080 or write fresh, the number is reserved either way
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
  # per-commit, ALWAYS: git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com commit ...
allowed_paths:
  - crates/git-vista-server/src/journal.rs
  - crates/git-vista-core/src/activity.rs
  - crates/git-vista-server/src/**        # reader/writer seams as needed
  - docs/adr/
forbidden_paths:
  - crates/git-vista/src/**
  - ci/browser/**
deliverables:
  - branch pushed, PR opened with "Closes #521"
  - ADR 0085 (or an explicit amendment to 0080's consequences carrying the 0085 number in the index)
```

## Environment truths — read before your first test run

- **~320 server tests fail in your container on unmodified `main`** (missing
  Landlock; sandbox tier refuses). Baseline on unmodified main FIRST; only
  the diff is yours; both counts in the PR body.
- **`cargo build -p git-vista-server --bin gv-sandbox` before any server
  test run** — the helper is not built by `--bin git-vista-server` selection
  and hundreds of tests exec it at runtime.
- **Browser leg unrun — say so in the PR body.** Owner runs it before merge.

## Truth-checked state (verified against main 682f3061, 2026-08-26)

`RefsAtEvent` is an enum in `crates/git-vista-core/src/activity.rs:175`, with
an `InBatch { batch }` variant resolved at `activity.rs:274-276`. The #485
batching (ADR 0080, PR #498) made a 500-ref fetch journal one full snapshot
plus N−1 `in_batch` referrers. #519 (merged 2026-08-25) put the writing pid
into batch ids — read its commit before touching id semantics.

## The defect, plainly

A pre-#485 binary's `RefsAtEvent` has no catch-all variant, and its
`read_all` skips unparseable lines. **After a rollback, every
`{"status":"in_batch"}` referrer is an unknown variant and vanishes**: a
100-ref fetch renders as one event until re-upgrade. Bytes on disk are
intact — rendering loss, not data loss — but it is a persisted-format break
that ADR 0080 never weighed.

## The job — a decision first, then the smallest honest mechanism

ADR 0080 skipped an alternative: a versioned batch envelope in the same
JSONL stream, where an old reader drops ONE unsupported record instead of
N−1. Your job:

1. **Analyze both options against the actual reader code** (old readers are
   shipped binaries — nothing you write fixes THEM; be precise about what
   each option buys for the past vs the future):
   - (a) accept the rollback cost and document it in ADR 0080's
     consequences, verbatim about the N−1 loss;
   - (b) additionally stamp a **journal format version** on new writes with
     a tolerant reader, so the NEXT format change has an answer at
     read-time instead of a guess (this is the same lesson #509 is teaching
     the durable operation store today, and the M5-family reviews flagged
     both).
2. **Recommend and implement.** The house prior is (a)+(b): document the
   cost that is already sunk, and version what is still cheap to version.
   If your analysis lands elsewhere, the ADR argues it — a well-argued (a)
   alone beats a mechanical (b) nobody can state the benefit of.
3. Whatever ships: the version stamp (if any) must not break the CURRENT
   reader on files written by main's binary — mixed-line files are the
   normal case, not an edge. Pin that with a fixture holding pre-batch,
   batch, and (if introduced) stamped lines in one file.
4. **Mutation-prove two different ways** any invariant you pin (e.g. drop
   the tolerant-read arm; then mis-stamp the version on write), red at
   different assertions, byte-identical restore verified.

## Acceptance

1. ADR 0085 records the decision, the rejected alternative, and the honest
   statement of what rollback still costs after your change.
2. Mixed-line fixture test exists and passes; mutation evidence in the PR
   body (two red assertion lines, verbatim).
3. `cargo fmt --all` · `clippy --all-targets -- -D warnings` · server suite
   zero new failures vs your recorded baseline.
4. PR body: baseline counts, browser-leg-unrun line, ADR summary, your
   session tag.

**Written by fable · 2026-08-26 · truth-checked against 682f3061 the same morning.**
