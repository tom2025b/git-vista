# PROJECT_STATUS — Activity Log + Contextual Undo

Status as of this checkpoint. Read this first when resuming.

## TL;DR

Building two big features on top of the existing branch-ops app: an **Activity
Log / Journal view** and **Contextual Undo**, plus **commit diffs** and a
**working-tree status** chip along the way.

- Branch: `feature/activity-log-undo` (pushed to origin).
- Safety net: `v1-stable` (= `main` @ `a0715e0`, pushed) — the known-good
  fallback. Untouched by any of this work.
- **The branch tip does NOT compile right now.** The last commit is an
  intentional WIP checkpoint. One missing line + CSS + a build pass finish it.
- Steps 1–3 are complete, tested, and green. Step 4 is ~80% done. Steps 5–6
  not started.

## Commit history on the branch

Newest first:

| Commit    | State        | What |
|-----------|--------------|------|
| `175e948` | **WIP, broken** | Step 4 Activity panel UI — does not compile (see below) |
| `c421faf` | ✅ green      | Step 3 — activity backend: journal, reflog reader, `/api/activity` |
| `aa0bbdb` | ✅ green      | Step 2 — commit diffs: `/api/diff/{id}`, Changes section, "Show diff" menu item |
| `1cbb03e` | ✅ green      | Step 1 — working-tree status: parser, `/api/status`, topbar chip |

Everything through `c421faf` builds clean (native + wasm) and passes the full
test suite (109 tests). To get back to a compiling tree at any point:
`git checkout c421faf` (or reset the branch there if you want to redo Step 4).

## The overall plan (6 steps)

1. ✅ **Git status** — porcelain-v2 parser + `/api/status` + topbar chip.
2. ✅ **Diff** — `/api/diff/{id}` + Changes section in the detail panel + "Show
   diff" menu item.
3. ✅ **Activity backend** — app journal + gix reflog reader + snapshot diffing
   + `/api/activity` (merge/dedupe/coalesce/attribute).
4. 🚧 **Activity panel UI** — topbar button + right-docked panel (status on
   top, feed below); tapping a row opens the shared context menu.
5. ⬜ **Contextual undo** — `/api/undoables` + `POST /api/undo` +
   `PendingOp::Undo` confirm arm + wiring into the graph menu AND activity rows.
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

## What's IN FLIGHT (Step 4 — the reason it doesn't compile)

Commit `175e948`. The Activity panel is written but **not wired in**.

### Written in this commit
- `crates/git-vista/src/icons.rs` — `history` / `undo` / `push` / `checkout`
  glyphs added to both icon sets and the exhaustive test list.
- `crates/git-vista/src/datetime.rs` — `ago_label` (pure, tested: just now /
  Nm / Nh / Nd / None past a week) + `time_ago` wasm wrapper.
- `crates/git-vista/src/api.rs` — `fetch_activity(limit)`.
- `crates/git-vista/src/state.rs` — `Overlays.activity_open: RwSignal<bool>`.
- `crates/git-vista/src/app.rs` — `activity_open` signal created in `App`;
  topbar **"Activity"** button toggles it; `graph_canvas` takes it as a param
  and threads it into the `Overlays` bundle; panel mounted in the overlays
  wrapper via `activity::activity_panel_view(...)`; `use crate::{activity, …}`.
- `crates/git-vista/src/menu.rs` — destructures `activity_open`; "View
  details" and "Show diff" now close the Activity panel (right-edge
  exclusivity, since both panels dock right).
- `crates/git-vista/src/activity.rs` — **new**, the panel itself: status
  section on top (headline + capped dirty-file list), event feed below
  (per-kind glyph, summary, ref pill, app/terminal pill, relative time).
  Tapping a row builds the SAME `MenuData` the graph dots use and opens
  `menu.rs`'s menu at the tap point (clamped to stay on-screen near the right
  edge). Reuses the `.detail-panel` CSS family; explicit ✕ close (no Esc
  dependency — iPad rule); both fetches re-fire on open and on `reload`.

### ⛔ Why it doesn't compile — the exact remaining wiring
1. **`crates/git-vista/src/main.rs` is missing the module declaration.**
   `activity.rs` exists but is never declared, so `crate::activity` (referenced
   from `app.rs`) doesn't resolve. Add, in the wasm-cfg module block (next to
   `mod app;` etc.):
   ```rust
   #[cfg(target_arch = "wasm32")]
   mod activity;
   ```
   (It's the frontend `activity.rs`; unrelated to the server module of the
   same name.)
2. **Panel CSS not written.** `activity.rs` uses these classes, none styled
   yet: `.activity-panel`, `.act-head-buttons`, `.act-refresh`, `.act-status`
   (+ `.clean/.dirty/.conflict`), `.act-file`, `.act-file-path`, `.act-pill`
   (+ `.act-app`, `.act-terminal`, `.act-ref`), `.act-feed-title`, `.act-row`,
   `.act-row-static`, `.act-glyph`, `.act-main`, `.act-summary`, `.act-meta`,
   `.act-when`. Add to `crates/git-vista/styles.css`. (It reuses `.detail-panel`
   for the frame, so it renders even without these — just unstyled.)

### Not started for Step 4
- `cargo check` (native + wasm) after the two fixes above.
- Build (`trunk build`) + a headless-CDP render check of the panel
  (see the "Headless UI verification" note in memory).

---

## Remaining steps (not started)

### Step 5 — Contextual undo
- Server: `GET /api/undoables` (undo actions for a tapped commit/branch,
  computed live) + `POST /api/undo` executing `UndoAction`:
  - `RestoreBranch` → `git branch <name> <tip>`.
  - `ResetBranch` → checked-out branch: `git reset --hard` **only after**
    `git status --porcelain` confirms a clean tree (never eat uncommitted
    work); other branches: `git branch -f`. Honour the CAS `expected_tip`.
  - `RevertCommit` → `git revert --no-edit` (auto-abort on conflict, like
    rebase does).
  - Every undo is itself journaled. Read-only clones: 403 + hidden in UI.
- Frontend: one new `PendingOp::Undo(UndoAction)` arm in `state.rs` +
  `dialogs.rs` (reuse the confirm modal wholesale, incl. `warn_pushed` text).
  Graph menu fetches `/api/undoables` async and shows an undo section when it
  lands. Activity rows already carry `event.undo` from `/api/activity` — surface
  it directly on the row / in its menu.
- "Undo a branch creation" = reuse the existing Delete flow (`PendingOp::Delete`
  → ForceDelete path) verbatim.

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
# finish Step 4: add `mod activity;` (wasm cfg) to crates/git-vista/src/main.rs,
# write the .act-* CSS in crates/git-vista/styles.css, then:
cargo test --workspace
cargo check -p git-vista --target wasm32-unknown-unknown
```
