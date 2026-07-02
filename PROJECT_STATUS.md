# PROJECT_STATUS — Activity Log + Contextual Undo

Status as of this checkpoint. Read this first when resuming.

## TL;DR

Building two big features on top of the existing branch-ops app: an **Activity
Log / Journal view** and **Contextual Undo**, plus **commit diffs** and a
**working-tree status** chip along the way.

- Branch: `feature/activity-log-undo` (pushed to origin).
- Safety net: `v1-stable` (= `main` @ `a0715e0`, pushed) — the known-good
  fallback. Untouched by any of this work.
- **Steps 1–5 are complete, tested, and green** (114 tests; native + wasm
  builds clean; steps 4 and 5 verified live + headless-CDP). Only Step 6
  (final verify pass + docs + PR) remains.
- On top of Step 5: a **stub-branch fix batch** (`035c75d`) — see "Post-Step-5
  fix batch" below.

## Commit history on the branch

Newest first:

| Commit    | State        | What |
|-----------|--------------|------|
| `035c75d` | ✅ green      | Post-Step-5 fix batch — lane-0 trunk reservation, empty commit on stubs, menu-on-stub fixes, merge no-op message, stub z-order, camera headroom |
| `170c5e3` | ✅ green      | Step 5 — contextual undo: `/api/undoables/{id}`, `POST /api/undo`, menu undo section, row Undo buttons, confirm-modal arm |
| `a8af52a` | ✅ green      | Step 4 finish — `mod activity;` wiring + the `.act-*` panel CSS |
| `175e948` | WIP (broken) | Step 4 Activity panel UI checkpoint — superseded by `a8af52a` |
| `c421faf` | ✅ green      | Step 3 — activity backend: journal, reflog reader, `/api/activity` |
| `aa0bbdb` | ✅ green      | Step 2 — commit diffs: `/api/diff/{id}`, Changes section, "Show diff" menu item |
| `1cbb03e` | ✅ green      | Step 1 — working-tree status: parser, `/api/status`, topbar chip |

## The overall plan (6 steps)

1. ✅ **Git status** — porcelain-v2 parser + `/api/status` + topbar chip.
2. ✅ **Diff** — `/api/diff/{id}` + Changes section in the detail panel + "Show
   diff" menu item.
3. ✅ **Activity backend** — app journal + gix reflog reader + snapshot diffing
   + `/api/activity` (merge/dedupe/coalesce/attribute).
4. ✅ **Activity panel UI** — topbar button + right-docked panel (status on
   top, feed below); tapping a row opens the shared context menu.
5. ✅ **Contextual undo** — `/api/undoables/{id}` + `POST /api/undo` +
   `PendingOp::Undo` confirm arm, wired into the graph menu AND activity rows.
6. ⬜ **Verify + docs** — end-to-end test pass, PROJECT_MEMORY/README updates.

Architecture principle throughout: **maximum reuse**. The context menu
(`menu.rs`) is the single menu for both graph dots and activity rows; the
confirm modal (`dialogs.rs`) gets one new arm for every undo; the diff renders
inside the existing detail panel; all parsing lives in `git-vista-core`
(pure, unit-tested, shared with wasm).

---

## What's DONE and verified (Steps 1–3)

### Step 1 — Working-tree status (`1cbb03e`)
- `crates/git-vista-core/src/status.rs` — **new.** `RepoStatus` type +
  `parse_porcelain_v2` (parses `git status --porcelain=v2 --branch`). Pure,
  7 unit tests (branch/ahead-behind headers, staged/unstaged split, renames,
  spaces in paths, C-quoted paths, untracked, conflicts).
- `crates/git-vista-server/src/main.rs` — `GET /api/status` handler
  (`worktree_status`), shells out to git, `no-store`.
- `crates/git-vista/src/api.rs` — `fetch_status()`.
- `crates/git-vista/src/app.rs` — live status chip in the topbar (green
  clean / yellow change-count / red conflict, plus ↑ahead ↓behind), keyed on
  the shared `reload` counter.
- `crates/git-vista/styles.css` — `.status-chip` styles.
- Verified live against the real repo: reports branch + change list correctly.

### Step 2 — Commit diffs (`aa0bbdb`)
- `crates/git-vista-core/src/diff.rs` — **new.** `CommitDiff` / `DiffFile`
  types + `parse_name_status_z` / `fold_numstat_z` (both `-z` NUL-separated).
  Pure, 7 unit tests (renames consume two paths, binary → None counts,
  totals, spaces/specials verbatim).
- `crates/git-vista-server/src/main.rs` — `GET /api/diff/{id}` (`commit_diff`).
  Validates the id is hex; ordinary commits via `git show`, **merges diffed
  against their first parent** (`against_first_parent` flag); patch capped at
  `DIFF_PATCH_CAP` (200 KB) at a line boundary with a `truncated` flag. Added
  a shared `git_stdout` helper.
- `crates/git-vista/src/api.rs` — `fetch_diff()`.
- `crates/git-vista/src/detail.rs` — a **Changes section**: per-file stat rows
  (using the added/modified/deleted/renamed glyphs) + colour-coded unified
  patch. Lazily fetched, keyed on the open commit like the detail body.
- `crates/git-vista/src/menu.rs` — **"Show diff"** item; opens the detail
  panel and scrolls the Changes section into view (one-shot `scroll_diff`
  flag in `Overlays`, consumed on next render via `request_animation_frame`).
- `crates/git-vista/Cargo.toml` — added web-sys `Document` feature.
- `crates/git-vista/styles.css` — `.detail-diff`, `.diff-*`, `.detail-file*`.
- Verified live: ordinary commit, merge (first-parent), bad ids → 400/404.

### Step 3 — Activity backend (`c421faf`)
- `crates/git-vista-core/src/activity.rs` — **new.** The heart of both
  features. Types: `ActivityKind`, `ActivitySource` (App/External),
  `ActivityEvent`, `UndoAction` (RestoreBranch / ResetBranch / RevertCommit),
  `Undoable`, `ReflogEntry`. Logic: `parse_reflog_message` (git reflog line →
  kind + summary) and `assemble_feed`, which:
  - coalesces a rebase's per-pick reflog burst into ONE event,
  - drops the HEAD copy of a branch movement (a commit logs on both),
  - folds an app op's reflog echo into its journal entry (App attribution),
  - attaches undo hints computed against the repo's **current** tips
    (compare-and-swap `expected_tip` so a stale menu can't reset moved work;
    `warn_pushed` when the discarded tip is on the remote),
  - sorts newest-first, caps.
  9 unit tests covering every rule above.
- `crates/git-vista-git/src/reflog.rs` — **new.** `read_reflogs` via gix:
  HEAD + local + remote-tracking reflogs, newest-first per ref, capped.
  5 tests against real fixture repos (fixture events, chain integrity,
  per-ref cap, push updates, no-reflog repo degrades gracefully).
- `crates/git-vista-server/src/journal.rs` — **new.** JSONL journal +
  branch-tip snapshots under `.git/git-vista/`. `append` / `read_all` /
  `read_snapshot` / `write_snapshot` / `remove_from_snapshot`. All
  best-effort (never breaks the git op). 4 tests.
- `crates/git-vista-server/src/activity.rs` — **new.** `GET /api/activity`:
  reads current branches, diffs against the snapshot to synthesize
  "deleted outside git-vista" events (carrying the last-known tip → still
  restorable), rewrites the snapshot, then calls `assemble_feed`.
- `crates/git-vista-server/src/main.rs` — journal hooks in **every** write
  handler: branch-create, commit, merge, push, delete, force-delete, rebase.
  Delete handlers capture the doomed tip with `rev_parse` **before** deleting
  (git erases a branch's reflog with the branch — the journal is the only
  record of where it pointed). Added `rev_parse` + `journal_app_event`
  helpers; registered the route; added `mod activity; mod journal;`.
- `crates/git-vista-server/Cargo.toml` — serde/serde_json deps + tempfile
  dev-dep.
- **Verified live end-to-end** on a throwaway repo: app-API deletion attributed
  `App`, terminal deletion caught `External` via snapshot diff, both with
  correct restore tips; merge and tip-commit events carry reset-style undo
  hints only while still at the branch tip.

---

### Step 4 — Activity panel UI (`175e948` + `a8af52a`)
- `crates/git-vista/src/activity.rs` — **new.** The right-docked panel:
  status section on top (headline + capped dirty-file list), event feed below
  (per-kind glyph, summary, ref pill, app/terminal pill, relative time).
  Tapping a row builds the SAME `MenuData` the graph dots use and opens
  `menu.rs`'s menu at the tap point (clamped near the right edge). Explicit ✕
  close (iPad rule); both fetches re-fire on open and on `reload`.
- Supporting pieces: `icons.rs` glyphs (history/undo/push/checkout),
  `datetime.rs` `ago_label`/`time_ago`, `api.rs` `fetch_activity`,
  `state.rs` `Overlays.activity_open`, topbar button in `app.rs`, right-edge
  exclusivity with the detail panel in `menu.rs`, the `.act-*` CSS family,
  and the `mod activity;` (wasm cfg) declaration in `main.rs`.
- Verified headless via CDP: status + feed render, a row tap opens the shared
  context menu.

### Step 5 — Contextual undo (`170c5e3`)
- `crates/git-vista-server/src/activity.rs` — `GET /api/undoables/{id}`:
  undo actions for one commit, computed live (same fold as the feed, minus
  snapshot upkeep — that invariant stays single-writer) + a revert offer for
  any non-merge commit. `POST /api/undo` executes an `UndoAction`:
  - `RestoreBranch` → `git branch <name> <tip>`;
  - `ResetBranch` → checked-out branch: `git reset --hard` only after a
    clean-tree check (`git status --porcelain` empty — even an untracked file
    could be overwritten if the target commit tracks that path); other
    branches: `git branch -f`. CAS `expected_tip` honoured (409 when moved);
  - `RevertCommit` → `git revert --no-edit`, auto-abort on conflict.
  Every undo is journaled (App-attributed in the feed; a reset gets its own
  undo hint, so undo-the-undo works). Read-only clones: 403 on undo, empty
  undoables, hints stripped from `/api/activity`.
- `git-vista-core` — reflog `"branch: Reset to …"` parses as `Reset`, so a
  `git branch -f` undo's echo folds into its journal entry (+2 tests).
- Frontend — `PendingOp::Undo(Undoable)` arm in the shared confirm modal
  (incl. `warn_pushed` text); the context menu grows an async undo section
  (`fetch_undoables`, keyed on commit + reload); Activity rows show a direct
  Undo button (rows are `<div>`s now — no button-in-button). Undoing a
  branch creation stays the existing Delete flow, as planned.
- Verified live on a throwaway repo (all three actions, both reset paths,
  CAS + dirty-tree 409s, absorption) and headless via CDP (row button, menu
  section, modal, confirmed undo refreshing the feed in place).

---

### Post-Step-5 fix batch (`035c75d`)

The "commit on a fresh branch stub made the branch disappear" investigation,
plus everything found alongside it:

- **layout.rs** — lane 0 is now *reserved* for the trunk's tip (or the
  checked-out branch's tip when its first-parent chain runs through the trunk
  tip). Previously the newest commit always took lane 0, so a side branch's
  new commit glued itself onto the trunk (same lane, then recoloured
  trunk-blue) — the branch looked like it had vanished into main. Regression
  test: `a_commit_on_a_side_branch_forks_out_instead_of_absorbing_the_trunk`.
- **Empty commit on a branch stub** — `POST /api/commit` takes an optional
  `branch`. A branch that isn't checked out gets `git commit-tree` + a
  compare-and-swap `git update-ref` (empty commits only; HEAD, index and
  worktree untouched; journaled; 409 when the branch moved). The menu enables
  "Create empty commit" on stubs; the dialog title names the target branch.
- **Menu on stubs** — no undoables fetch (the anchor commit's undo actions
  belong to other branches), no "Rebase onto main" item (nothing to replay;
  it would silently target the checked-out branch).
- **Merge no-op** — "Already up to date." is surfaced verbatim instead of
  journaling a phantom merge event; frontend alerts it and still reloads.
- **render.rs** — stub connector paths draw in a pass under all rings with
  `pointer-events: none`, so a cascade's path can't swallow taps on a ring.
- **Camera headroom** — the home view (initial, "Reset view", the `0` key)
  shifts down by `stub_headroom(...)` so a branch created on the newest
  commit isn't born clipped above the canvas. Recomputed per graph load.

---

## Remaining steps (not started)

### Step 6 — Verify + docs
- Full `cargo test` + `trunk build` + headless render pass exercising
  status/diff/activity/undo against a **throwaway** scratch repo (never a real
  one — and never delete a branch in a real repo; see below).
- Update `PROJECT_MEMORY.md` (house convention: document each phase) and
  `README.md`. Open the PR to `main`. **Do not delete any branch.**

---

## Key conventions / guardrails (don't relearn these the hard way)
- **Never delete a git branch** in this repo (local/remote/merged) — standing
  user rule. Push + PR; leave branches in place.
- **iPad is the primary device**, no Esc key — never make Esc the only way to
  close anything. All panels have a ✕ button.
- **No void `<input>`** in Leptos CSR — it panics the template walk on iOS
  WebKit. Use `<textarea>`. (No new inputs were added in this feature, so this
  isn't currently at risk, but keep it in mind for Step 5 if any field appears.)
- New shared/JSON-crossing types + all parsing go in `git-vista-core` (wasm-
  safe, unit-tested); gix / filesystem reads go in `git-vista-git` (native
  only); the browser never depends on `git-vista-git`.
- Menu handlers: **write signals BEFORE `menu.set(None)`** — closing the menu
  disposes the handler's reactive owner, after which signal writes are
  unreliable.
- Verify UI changes with chrome-headless-shell + raw CDP (Node 22); Firefox
  headless hangs. See the "Headless UI verification" memory note.

## How to resume
```bash
git checkout feature/activity-log-undo   # already the current branch
# Steps 1–5 are done and green. Only Step 6 remains: the final end-to-end
# verify pass on a throwaway repo, PROJECT_MEMORY.md + README.md updates,
# and the PR to main (push the branch; NEVER delete any branch).
cargo test --workspace
cargo check -p git-vista --target wasm32-unknown-unknown
```
