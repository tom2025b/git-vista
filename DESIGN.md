# git-vista — Design & Development Roadmap

A Rust desktop app that visualizes git history as a clean, **zoomable vertical
graph**. Stack: **Tauri v2** (native shell) + **Leptos** (Rust→wasm UI, drawn as
SVG in the webview) on top of a pure-logic **git-vista-core** crate.

## Architecture recap

```
git-vista-core  (no UI deps)        git-vista (Leptos/wasm)      src-tauri (native)
  model   ── serde types ───────────────► invoke() ◄── IPC ──── commands
  repo    ── walk history (gix)                                    │
  layout  ── commits → lanes/edges  ◄───────────────────────── run() builder
```

Data flows one way: `repo` reads commits → `layout` positions them into a
`Graph` → a `#[tauri::command]` serializes it → Leptos deserializes the same
types and renders SVG. The core never knows the UI exists, so it stays fast to
build and trivial to unit-test.

## Principles

- **Small, shippable phases.** Each phase compiles, has a visible/testable result,
  and is logged in `PROJECT_MEMORY.md` before moving on.
- **Core-first.** The risky logic (history walking, lane assignment) lands in
  `git-vista-core` with unit tests before any pixels.
- **Test the algorithm headlessly.** Lane layout is verified against hand-built
  histories — no GUI in the loop.

---

## Phases

### Phase 0 — Scaffold ✅ (done 2026-06-28)
Workspace, three crates, build pipeline (core tests, Trunk wasm build, Tauri
shell compile). `repo`/`layout` are stubs.

### Phase 1 — Read real commits
- **Goal:** `repo::walk_history(path, limit)` returns real `CommitSummary`s.
- **Do:** enable `gix`; open repo at HEAD; walk newest-first; map id/parents/
  summary/author/time.
- **Done when:** a unit test over a temp fixture repo returns the expected
  commits in order; merges report 2+ parents.

### Phase 2 — Vertical lane layout (the core algorithm)
- **Goal:** `layout()` assigns real lanes/columns, not all-lane-0.
- **Do:** active-lane tracking; route first parent down the same lane, branch/
  merge into allocated lanes; emit edges with correct lane transitions.
- **Done when:** tests cover linear, one branch+merge, and an octopus merge; lane
  count and edge endpoints match expected fixtures.

### Phase 3 — Wire IPC end-to-end
- **Goal:** real graph crosses the boundary.
- **Do:** `list_commits(path)` calls `repo`+`layout`; frontend `invoke`s it on
  load and logs the row/edge counts to the console.
- **Done when:** opening the app on this repo prints a non-empty Graph.

### Phase 4 — Render the static graph
- **Goal:** see the vertical graph (no interaction yet).
- **Do:** Leptos draws an SVG — a circle per `GraphRow` (y=row, x=lane), a line/
  bézier per `Edge`. Newest at top.
- **Done when:** the graph shape of a small repo is visually correct.

### Phase 5 — Commit rows
- **Goal:** readable rows beside the nodes.
- **Do:** to the right of the gutter, render short-hash + summary + author per row.
- **Done when:** rows align with their nodes and read cleanly.

### Phase 6 — Pan
- **Goal:** move around a tall graph.
- **Do:** a `camera { offset }`; drag / scroll translates the SVG viewport.
- **Done when:** dragging moves the graph smoothly; nothing drifts.

### Phase 7 — Zoom
- **Goal:** continuous zoom.
- **Do:** add `camera.scale`; wheel zooms toward the cursor; screen↔graph transforms.
- **Done when:** zoom in/out keeps the point under the cursor fixed.

### Phase 8 — Viewport virtualization
- **Goal:** large repos stay responsive.
- **Do:** render only rows/edges intersecting the viewport.
- **Done when:** a 50k-commit repo scrolls without frame drops.

### Phase 9 — Level of detail (overview ↔ detail)
- **Goal:** "overview + detailed view" from one zoom.
- **Do:** LOD thresholds on `scale` — hide text when tiny, collapse to lane
  ribbons at overview zoom.
- **Done when:** zooming far out shows topology only; zooming in restores rows.

### Phase 10 — Refs & colors
- **Goal:** orient the graph.
- **Do:** branch/tag/HEAD badges; stable per-branch lane colors.
- **Done when:** branches keep a consistent color; tips are labeled.

### Phase 11 — Commit detail panel
- **Goal:** inspect a commit.
- **Do:** click a node → side panel with full hash, author/date, body, parents.
- **Done when:** selection + panel update correctly; click-through works.

### Phase 12 — Search & filter
- **Goal:** find commits.
- **Do:** search by message/author/hash; highlight and scroll-to matches.
- **Done when:** typing filters/jumps to matches live.

### Phase 13 — Packaging & polish
- **Goal:** a real installable app.
- **Do:** generate real icons (`cargo tauri icon`); `tauri build`; large-repo perf
  pass; keyboard shortcuts; theming.
- **Done when:** `tauri build` produces a working bundle that opens a repo picked
  at runtime.

---

## Backlog / later
Open-a-repo dialog, remember recent repos, diff view on a commit, follow-file
history, blame, dark/light toggle, minimap.
