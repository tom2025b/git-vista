# CLOUD-3 — #487: Push becomes a fold candidate, with the same care Pull got

**Batch of 2026-08-26 · merge order 3 of 5 (after CLOUD-2 — same activity/journal subsystem; rebase on its landed format decision before you finalize).**

```yaml
task_id: gv-487-cloud-3
issue: 487
branch: cloud/487-push-fold
base: main            # rebase onto main AFTER CLOUD-2 lands, before opening your PR
adr_number: 0086      # RESERVED — use only if your change turns contract-shaped; a fold-candidate
                      # addition with tests is routine and needs NO ADR. If unused, say so in the
                      # PR body; the number stays burned rather than reassigned.
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
  # per-commit, ALWAYS: git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com commit ...
allowed_paths:
  - crates/git-vista-core/src/activity.rs
  - crates/git-vista-server/src/planner/push.rs
  - docs/adr/           # only if 0086 turns out needed
forbidden_paths:
  - crates/git-vista/src/**
  - ci/browser/**
deliverables:
  - branch pushed, PR opened with "Closes #487"
```

## Environment truths — read before your first test run

- **~320 server tests fail in your container on unmodified `main`** (no
  Landlock; sandbox tier refuses). Baseline first; only the diff is yours;
  both counts in the PR body.
- **`cargo build -p git-vista-server --bin gv-sandbox` before any server
  test run.**
- **Browser leg unrun — say so in the PR body.**

## Truth-checked state (verified against main 682f3061, 2026-08-26)

- `fold_ref_update_bursts` at `crates/git-vista-core/src/activity.rs:775`,
  with the candidate gate at `:780`:
  `matches!(e.kind, ActivityKind::Fetch | ActivityKind::Pull)`.
- `push::journal_updates` is defined at **`planner/push.rs:684`** with its
  per-ref loop at **`:693`** — **the issue's line numbers are correct and
  current** (an earlier draft of this handoff claimed they had drifted; that
  claim was wrong and is retracted here). Its three call sites are at
  `:489`, `:505` and `:510`.
- The issue's survey stands: exactly two per-item journal loops exist
  (fetch — folded since #329 — and push), and the synthesized
  `BranchDeleted` loop is CORRECTLY per-item (N distinct user actions; its
  `old_oid` is what undo needs). Do not touch it.

## The defect, plainly — and why it is latent

Push journals one entry per remote-tracking ref it moved, structurally
identical to fetch — but `ActivityKind::Push` is not a fold candidate, so a
push moving 4 refs renders 4 unfolded feed rows. Today the push endpoint
pushes one named branch, so N=1 and nothing is wrong for the user. The fix
is filed so the day a multi-ref push path arrives (`--all`, matching
refspec, tags alongside), it does not become #329 with a different kind.

## The job

1. Add `ActivityKind::Push` to the fold gate — **with the asymmetry Pull
   already carries**: `names_a_local_branch` exists because a pull's own
   branch movement is the row the user wants and must never be folded away.
   A push has the same asymmetry: remote-tracking bookkeeping is noise;
   anything naming a local branch is not. Read how Pull threads that
   exemption and give Push exactly the same treatment.
2. The issue's own caution: **"a pin would be asserting against a
   hypothetical" applied to the OLD state.** Once you make Push a
   candidate, the behavior is real and MUST be pinned: a multi-ref push
   burst folds; a push row naming a local branch survives folding. Build
   the multi-ref case in the test fixture even though production cannot
   emit it yet — the fold code cannot know that, and the test is about the
   fold.
3. The #485 per-ref `capture_refs` cost note: fetch's fix flows through the
   same `journal_app_event` → `journal::append` path. Confirm whether push
   already inherits the #485 batching on that path (read the landed #485
   change); if it does, say so in the PR body; if it does not, that is a
   finding to REPORT in the PR body, not scope to absorb.
4. **Mutation-prove two different ways**: remove Push from the gate; then
   break the local-branch exemption. Red at different assertions,
   byte-identical restore verified.

## Acceptance

1. Fold gate includes Push with the local-branch exemption, both pinned.
2. Mutation evidence in the PR body (two red assertion lines, verbatim).
3. `cargo fmt --all` · `clippy --all-targets -- -D warnings` · core + server
   suites zero new failures vs baseline (`cargo test -p git-vista-core` runs
   clean in a container — no sandbox dependency).
4. PR body: baseline counts, browser-leg-unrun line, the #485-inheritance
   answer, ADR 0086 used-or-not statement, your session tag.

**Written by fable · 2026-08-26 · truth-checked against 682f3061 the same morning.**
