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

### Phase 3 — Read real commits with gix
Implement `repo::walk_history()` to read real git history from a repository.

### Phase 4 — Connect real data to the graph
Wire the real commit data through the IPC layer into the frontend graph.

### Phase 5 — Commit rows & labels
Display commit message, short hash, and author next to each node.

### Phase 6 — Robust lane assignment
Improve the layout algorithm to properly handle branches and merges.

### Phase 7 — Refs & colors
Show branch names, HEAD, tags, and assign consistent colors per branch.

### Phase 8 — Viewport virtualization
Only render commits currently visible in the viewport for performance.

### Phase 9 — Level of detail
Change level of detail based on zoom level (hide text when zoomed out, etc.).

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
