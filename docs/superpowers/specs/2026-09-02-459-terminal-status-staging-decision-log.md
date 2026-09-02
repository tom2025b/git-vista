# #459 — terminal working-tree status and staging decision log

**Status:** Running implementation log  
**Base:** `99d203faeeb11391db62036219673a0123eacd80`  
**Branch:** `feature/m10.04-459-terminal-status-staging`

## 2026-09-02 — existing boundaries verified

- The status read is `GET /api/status/v2`, returning the shared
  `WorktreeStatus` vocabulary: changed entries with staged/unstaged/both
  sides, rename-with-optional-further-edit, untracked, ignored, conflicted,
  submodule flags, and binary flags.
- A failed status read is a third state, not “clean”. The terminal keeps the
  last successful rows and reports the refusal separately, matching the
  shell's existing stale-data posture and the repository's `third-state-*`
  fixes (failed observation must not collapse into an affirmative answer).
- Activating a repository must first `POST /api/select` in Active mode.
  History reads can address a `?repo=` directly, but all staging and generic
  plan routes intentionally use the authenticated session's selection.
- Whole-tree stage/unstage and guarded discard use the generic pair:
  `POST /api/plan` with a typed `GitOperation`, show the returned `Plan`, then
  submit that exact value to `POST /api/execute-plan` only after approval.
- File/hunk/line staging uses the already-shared partial-staging path:
  `GET /api/staging/diff`, construct a `PatchPlan` from the served generation
  and parsed coordinates, `POST /api/staging/preview`, show its exact patch
  and whole-file pathspecs, then submit the unchanged `PatchPlan` to
  `POST /api/staging/apply` only after approval. The apply handler builds
  `GitOperation::StageSelection` and enters the shared planner; the TUI never
  constructs git argv.
- `git diff` does not contain untracked paths, so the existing partial-staging
  vocabulary cannot truthfully preview a single untracked file. This slice
  will offer untracked content through the existing whole-tree `StageAll`
  operation and will not invent a second staging implementation.

## 2026-09-02 — terminal interaction

- The unused `Branches` placeholder becomes `Working Tree`, the pane #459
  was reserved to fill. It shows the same five browser sections in the same
  priority order: conflicted, staged, unstaged, untracked, ignored.
- `Enter` on a staged/unstaged tracked row opens the corresponding shared
  staging diff (`index → HEAD` for unstage, `worktree → index` for stage).
- `Space` previews the selected file in Working Tree, or the selected
  file/hunk/changed line in the staging diff. `a` previews stage-all or
  unstage-all according to the selected status section.
- `d` on an eligible tracked row first opens a destructive confirmation whose
  copy names the path and says its uncommitted work will be permanently lost.
  Confirming that guard only asks the server to build a plan; the returned
  destructive plan still requires its own explicit approval before execution.
- `y` approves the currently visible confirmation/review, `n` or `Esc`
  refuses it. No write key directly emits an execution request.

## Acceptance ledger (running)

- Status list with browser-equivalent states: **IN PROGRESS**
- Stage/unstage at whole-tree, file, hunk, and line granularities through the
  shared planner: **IN PROGRESS**
- Guarded discard confirmation says what is lost: **IN PROGRESS**
- Every write plan is visible before execution: **IN PROGRESS**

Final entries will replace each status with exact `file:line` evidence or an
honest **NOT MET**.
