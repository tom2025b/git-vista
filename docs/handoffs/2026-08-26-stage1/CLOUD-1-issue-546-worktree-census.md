# CLOUD-1 · #546 — Enumerate the worktree siblings

**Stage 1 of 4.** Three other lanes run beside you on unrelated files; you will not
collide with them. Everything else in milestone M11 is blocked on this issue, so
the shape you choose here is the shape four more issues inherit.

```yaml
task_id: gv-cloud-1-546
issue: 546
milestone: M11 — Linked Worktrees as First-Class Workspaces
branch: claude/cloud-1-546-worktree-census
base: main
kind: FEATURE — read-only server query plus its protocol DTO
spec: docs/superpowers/specs/m3.23-worktrees.md   # §1, "Enumeration is a read, and ships first"
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
  # per-commit ALWAYS:
  # git -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com commit ...
  # NEVER a bare `git commit` — some repos here carry a personal gmail in local config.
sign_artifacts_as: max
allowed_paths:
  - crates/git-vista-protocol/src/
  - crates/git-vista-server/src/
  - docs/adr/
forbidden_paths:
  - crates/git-vista/          # the viewer — M11.03's, not yours
  - ci/browser/                # you cannot run it; see below
github_writes: open the PR. Do NOT merge it, do not edit issues, do not comment on other PRs.
```

## Read the spec first — it is tracked, and it already decided most of this

`docs/superpowers/specs/m3.23-worktrees.md` §1 carries the exact struct, the
`Serviceable` enum, and the reasoning. **Use it as the starting point, not as
orders**: it was written before anyone attempted the change, and if implementing
it teaches you a field is wrong, say so in the PR body and do the better thing.

## Two things this environment cannot do, so plan around them

**You cannot run the browser leg.** `ci/browser/` needs a display and a live
server; a cloud container cannot run that suite at all (#503 records the
investigation). This issue is deliberately scoped to have no browser surface —
if you find yourself needing one, you have drifted into M11.03 and should stop
and say so.

**Build the sandbox helper once, before running server tests:**

```
cargo build -p git-vista-server --bin gv-sandbox
```

Selecting `--bin git-vista-server` does not build its sibling helper, and about
323 server tests fail at spawn for that reason alone. It looks like a real
failure and is not.

## The distinction that is the whole issue

`locked` and `prunable` are **git's own flags**, read from
`git worktree list --porcelain`. `serviceable` is **the app's separate fence**.

Folding them into one "usable?" boolean is the failure mode. "git says this is
locked" and "this is outside the folders you allowed" need different sentences
and different offers, and a single flag makes both impossible.

**A sibling outside the allowed roots is listed and refused with its reason —
never silently dropped.** A list that quietly omits a real worktree is the same
failure class as a status that omits a file: the reader cannot tell "not there"
from "not shown". This repository has corrected that shape four times this month
(`Obs`, `HeadBranch::Unknown`, `Advisory::DefaultBranchUnknown`,
`Blame::UnknownOperation`) and every one of them is a precedent you should match.

## Parse strictly

A porcelain line the parser does not understand is an **error**, not a skipped
line. Silently skipping is how a worktree disappears from a census that claims
to be complete.

## Mutation-prove every test you add — two different ways

Two breaks that fail *differently*: remove the mechanism, then weaken it. Red at
different assertions where the code allows. Restore byte-identically and verify
with `diff -q`. Quote the failing assertion verbatim in the PR body.

One `caught` proves the test notices *that* break, not that it pins the
invariant. A test here survived one mutation and caught another on 22 August;
either alone gave the wrong verdict.

**The specific one that matters:** drop the `OutsideAllowedRoots` arm so a
refused sibling vanishes from the list. If that stays green, the census does not
actually pin the rule above.

## Acceptance

1. The typed census exists in the protocol crate and is produced from
   `git worktree list --porcelain`.
2. `locked` and `prunable` come from git's output; a test proves a locked
   worktree reads as locked without the app inspecting anything else.
3. A sibling outside the allowed roots appears with its reason. Mutation-proven.
4. A `prunable` sibling whose directory is gone reads `Missing`, distinct from
   being outside the fence.
5. `is_current` is true for **exactly one** sibling — assert exactly one, not
   "at least one".
6. No new sandbox tier and no new grant. State in the PR which existing grant
   covers this and why.
7. `cargo fmt --all` · `cargo clippy --all-targets -- -D warnings` · full server
   suite green, **count stated**.
8. If the change touches a wire shape, the golden fixture is regenerated and the
   diff explained. If it does not, say so explicitly.

## What you must NOT do

- No UI. No `crates/git-vista/` changes at all.
- No `AddWorktree`, no `RemoveWorktree`, no pruning. Those are #549, #550, and
  the spec explicitly excludes pruning from the milestone.
- No merging your own PR.
- Do not weaken an assertion to make something pass. If a test must change
  meaning, that is its own section in the PR body with the argument.

## The PR body must say

- The full server test count, before and after.
- Mutation evidence: both breaks, the red assertions verbatim, byte-identical
  restore confirmed.
- Anything in the spec you found to be wrong. **Check every claim it makes about
  existing code against the source** — seven spec citations in this repository
  have been wrong, and the most recent one turned an afternoon's work into its
  own milestone.
- A plain statement of what you did NOT do.
