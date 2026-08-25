# Cloud handoff — #326, move `shape()`'s remaining match arms into their modules

**Written:** 2026-08-25 · **By:** max (CLI session on Tom's box) · **For:** a cloud Claude Code session on `tom2025b/git-vista`

> The safest of the five. Mechanical, well-scoped, and the extraction it
> finishes is already half done. **The issue itself says it is not a
> stop-the-world refactor** — honour that: if it stops being mechanical, stop
> and report rather than widening.

---

```yaml
task_id: gv-326-planner-shape
issue: 326
milestone: —
repo: tom2025b/git-vista
base: main
branch: refactor/326-planner-shape-arms
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
adr_number: none          # mechanical; no contract changes. If you find you
                          # need one, that means the change stopped being
                          # mechanical -- stop and say so instead.
allowed_paths:
  - crates/git-vista-server/src/planner.rs
  - crates/git-vista-server/src/planner/**
forbidden_paths:
  - design-docs/
  - ci/browser/**
  - crates/git-vista/src/**
  - crates/git-vista-protocol/src/**   # the vocabulary does not change
  - handoff.md
merge_order: independent, but see "Merge order" -- it collides with CLOUD-11.
```

---

## The measured state (from the issue, 2026-08-05)

`planner.rs` is 6,244 lines, but that number is misleading:

| | Lines |
|---|---|
| Inline `#[cfg(test)]` blocks | 2,343 |
| Comments (31% of the file) | ~1,200 |
| **Actual production code** | **~2,700** across ~50 functions |

Every production function is under 200 lines **except one**: `shape()`, at 614
lines, holding **22 `GitOperation::` match arms** of roughly 20 lines each.

So this is not "split a god file". It is: the per-operation modules already
exist, most of the extraction is already done, and `shape()` is where the last
22 arms still live centrally.

---

## How to do it without breaking anything

**One arm per commit, or one small group per commit.** A 22-arm move in a single
commit is unreviewable and un-bisectable, and this repo's history is teaching
material — Tom re-reads it. Small commits are the deliverable, not an overhead.

**Move the arm's comments with it.** 152 of `shape()`'s 614 lines are comments,
and they carry the reasoning for individual operations. A move that leaves the
prose behind in a shrinking `shape()` loses exactly the part that was worth
keeping.

**Do not change behaviour.** No renamed variants, no altered risk levels, no
"while I was here" fixes. If an arm looks wrong, open an issue and leave it.

**Keep the tests where they can still see what they test.** 2,343 lines of the
file are inline `#[cfg(test)]`. If an arm's tests move with it, say so in the
commit; if they cannot, say why.

---

## What you cannot run

**The browser leg does not run in a cloud container** — the kernel reports
`landlock_abi=-1` and INV-13 refuses a degraded tier; installing `bwrap` does
not help. A pure refactor should not need it.

Two `git-vista-server` tests flake under parallel execution because they race on
the process-global current repository (#438):
`recovery_center::tests::a_stale_claimed_undo_is_refused_and_the_branch_is_left_alone`
and `state::tests::selection_flow_carries_mode_and_gates_writes`. Re-run before
believing either is yours.

---

## Acceptance

1. `shape()` is under 200 lines, like every other production function in the
   file.
2. **No behaviour changed.** `cargo test --workspace` green with no test edited
   except by relocation. If you had to change an assertion, that is a finding —
   put it in the PR body in its own section.
3. Each arm's reasoning lives with the arm.
4. `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` green.
5. PR body says `Closes #326`. **Never delete the branch.**

---

## Merge order

This touches `planner.rs` and `planner/stash.rs`, and so does CLOUD-11
(#493/#494). **Whichever of you is ready second rebases** — but say in the PR
which you are, and do not merge on top of the other without re-running
`cargo test --workspace` against the actual merge result. Landing against a head
you reviewed rather than the head you are merging is how `main` carried a red
test for ten minutes on 2026-08-25.

---

**Signed:** max · 2026-08-25T10:28:00-04:00
