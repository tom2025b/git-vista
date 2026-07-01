# git-vista — Design & Development Roadmap

A clean, zoomable vertical git history visualizer built with Tauri + Leptos.

## Principles

- Show something on screen as early as possible
- Small, shippable phases with clear completion criteria
- Keep core logic separate from UI code
- Document decisions in `PROJECT_MEMORY.md` after each phase

## Phases

### Phase 0 — Scaffold ✅ (done)
Workspace setup, three-crate structure, build pipeline, and basic skeletons.

### Phase 1 — Static vertical graph (fake data) ✅ (done)
Create a vertical graph component using fake commit data. Render nodes and edges as SVG.

### Phase 2 — Interactive pan & zoom ✅ (done)
Add camera controls — drag to pan, mouse wheel to zoom, smooth viewport movement.
(Rebuilt on Pointer Events afterwards for proper **touch** support: single-finger
drag to pan, two-finger pinch to zoom on iPad/Safari — the original mouse-only
`movementX`/`wheel` approach was dead on touch. See PROJECT_MEMORY "Phase 7 fix".)

### Phase 3 — Read real commits with gix ✅ (done)
Implement `repo::walk_history()` to read real git history from a repository.

### Phase 4 — Connect real data to the graph ✅ (done)
Wire the real commit data into the frontend graph. (Done over **HTTP**, not Tauri
IPC: git-vista is browser-first, and a browser can't reach a Tauri command — so a
native `git-vista-server` (axum) serves the SPA + `/api/commits` and the frontend
`fetch`es it. Also added basic lane layout with **compact reuse**: first parent
stays in its lane; a merged-back branch frees its lane; new branches take the
leftmost free lane.)

### Phase 5 — Commit rows & labels ✅ (done)
Display commit message, short hash, and author next to each node. (Two-line SVG
labels in an aligned column right of the lanes; long messages truncated with `…`;
labels pan/zoom with the graph.)

### Phase 6 — Robust lane assignment ✅ (done)
Improve the layout algorithm to properly handle branches and merges. (Full
active-lane tracker in `git-vista-core::layout`: each commit takes the leftmost
lane reserved by a child else the leftmost free one; its first parent keeps that
lane (stable branch columns, mainline in lane 0); merge parents fan out to the
leftmost free lane **to the right** so they never cross back over the mainline.
Handles octopus merges, reuses freed lanes for sequential branches, and keeps
concurrent branches in distinct lanes. 8 layout tests cover linear, branch+merge,
octopus, sequential reuse, concurrent branches, and stable-lane continuity.)

### Phase 7 — Refs & colors ✅ (done)
Show branch names, HEAD, tags, and assign consistent colors per branch. (The
native `git-vista-git` crate gained `read_refs` — HEAD + local/remote branches +
tags, each peeled to a commit. `git-vista-core::layout::layout_with_refs` attaches
each ref as a badge on its commit and colours every commit by the branch that owns
its first-parent chain — a **stable per-branch colour** that's the same wherever
the branch appears, independent of lane reuse (HEAD's branch takes the trunk
colour; un-branched side lines fall back to a synthetic colour so every commit is
coloured). The frontend renders branch/tag/HEAD pills beside each commit and
colours nodes/edges by branch.)

### Phase 8 — Viewport virtualization
Only render commits currently visible in the viewport for performance.

### Phase 9 — Level of detail ✅ (done)
Change level of detail based on zoom level (hide text when zoomed out, etc.).
(Done: a pure `lod::detail_for(scale)` maps the camera zoom to a `Detail` level;
the view hides the message tier below 0.5× and the dimmed meta line below 0.8×,
so a zoomed-out graph reads as structure, not a smear. Phase 8 — viewport
virtualization — was skipped for now and is still open.)

### Phase 10 — Commit detail panel
Clicking a commit opens a side panel with full details.

### Phase 11 — Search & filter
Search commits by message, author, or hash.

### Phase 12 — Open repository UX
Add a proper way to open any local repository.

### Phase 13 — Packaging & polish
Icons, performance tuning, keyboard shortcuts, build process, and final cleanup.

## Backlog
- Dark/light theme toggle
- Minimap
- Diff view
- File history / blame
