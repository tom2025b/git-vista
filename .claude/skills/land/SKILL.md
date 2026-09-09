---
name: land
description: Land a green PR into main the safe serial way - merge main down into the branch, wait for all seven required checks to EXIST and pass, merge, refresh the app mirror. Use when a PR is ready to merge, when the user says "land PR N", or after CI goes green on a reviewed branch. Never lands a red or conflicted PR - it stops and reports instead.
---

# Landing a PR

The ruleset requires branches be up to date with main, and children never
auto-retarget (branches are never deleted here), so landing is strictly serial.
This ritual was proven across ~10 merges on 2026-08-05.

## Procedure (per PR, in order)

1. `git fetch --force origin '+refs/heads/*:refs/remotes/origin/*'` — plain
   fetch does not correct remote-tracking refs after force-pushes.
2. Merge `origin/main` into the branch **in a detached scratch worktree**
   (never the primary checkout; local branch refs are stale and pinned by
   agent worktrees). Push with `HEAD:<branch>`.
3. On conflict: STOP and report. Known funnel files (lib.rs exports,
   docs/adr/README.md, planner.rs match arms, route_authz.rs) resolve
   additive-union — but resolution is a decision, not part of this ritual.
   `EXPECTED_ROUTE_COUNT` is NEVER taken from either side: derive it by
   running `every_registered_route_is_classified`.
4. Wait for checks: they must EXIST (>=7 reported) AND none pending — an
   empty check list right after a push is "not started", never "passed"
   (this exact race produced a false BLOCKED verdict once).
5. Any failing check: STOP and report. Never merge around a red gate.
6. `gh pr merge <n> --merge --subject "<conventional title> (#<n>)"` —
   avoid a bare `(#NNN)` for any issue that must stay open.
7. Refresh the checkout the running `git-vista-server` actually serves. Read
   the repository argument from the server process's argv, require that checkout
   to be on `main`, then run `git -C "$served_repo" pull --ff-only origin main`.
   If no server is running, print an explicit (successful) skip: there is no app
   view to refresh. If a server is running but its repository cannot be resolved
   or refreshed, stop and report the failure loudly. Never restart the server;
   the owner reloads the existing browser session.
8. If a tracked PDF conflicted anywhere: re-render from the merged .md with
   render-md-pdf; never side-pick a binary.

`land.sh` beside this file is the reference implementation of steps 1-7 for
non-conflicting PRs; edit its `land` lines rather than rewriting the loop.

Existing per-issue worktrees retain the tracked skill version from their branch.
They receive this corrected step only after main is merged down or the worktree
is recreated; do not assume an older worktree refreshes the live app checkout.
