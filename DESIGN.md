# git-vista — Design & Development Roadmap

A clean, zoomable vertical git history visualizer — browser-first, built in Rust
with a Leptos → WebAssembly UI served over HTTP.

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

### Phase 8 — Viewport virtualization ✅ (done)
Only render commits currently visible in the viewport for performance. (Done
after Phase 9, at the user's request. A pure `viewport::visible_row_range(camera,
viewport_h, row_count, overscan)` inverts the row→y mapping to the window of rows
on screen, padded by an overscan margin; the frontend renders nodes, edges and
both label tiers through keyed `<For>`s over that window instead of building every
row eagerly. The range is a `Memo`, so a sub-row pan doesn't churn the DOM — only
rows crossing the viewport edge are added/removed. Edges are kept whenever their
row span intersects the window, so a long merge line passing through never
disappears; stubs stay eager (only a handful, and they fan upward).)

### Phase 9 — Level of detail ✅ (done)
Change level of detail based on zoom level (hide text when zoomed out, etc.).
(Done: a pure `lod::detail_for(scale)` maps the camera zoom to a `Detail` level;
the view hides the message tier below 0.5× and the dimmed meta line below 0.8×,
so a zoomed-out graph reads as structure, not a smear. Phase 8 — viewport
virtualization — was done afterwards; see above.)

### Phase 10 — Commit detail panel ✅ (done)
Clicking a commit's dot opens its context menu; "View details" opens a side panel
docked to the right with the commit's full detail — the whole message body and
both the author and committer signatures (name, email, own time). The panel
fetches lazily from a new `GET /api/commit/<id>` (a fresh `git_vista_git::
read_commit` via gix), so the graph payload stays lean; parent hashes in the panel
re-point it at that parent so you can walk up the history, and it links out to
GitHub when the commit is pushed.

### Phase 11 — Search & filter
Search commits by message, author, or hash.

### Phase 12 — Open repository UX ✅ (done)
"Open URL": paste a public `https://`/`http://`/`git://` URL, the server clones it
into a throwaway temp dir and switches to viewing it **read-only** (all write
actions hidden in the UI and refused server-side with 403). At most one clone is
kept — opening another deletes the previous. Scoped down at the user's request to
"look at a complex public history a few times to learn git", so no local-path
picker, discovery, recents, or auth. `gv <path>` still sets the writable starting
repo. The mutable current-repo (`OnceLock` → `RwLock`) is the reusable foundation
if a local-path picker is ever wanted.

### Phase 13 — Packaging & polish (in progress)
Icons, performance tuning, keyboard shortcuts, build process, and final cleanup.

Done so far:
- **Icons:** an inline SVG favicon (a small vertical branch graph) plus mobile meta
  (`theme-color`, `apple-mobile-web-app-*`, `viewport-fit=cover`) in `index.html`.
- **Keyboard shortcuts:** a window keydown listener — `Esc` backs out of the open
  overlay (menu → modal → detail panel), `+`/`-` zoom, `0` resets the camera, `r`
  refreshes. `Esc` is only a bonus: every overlay also closes via its Cancel button
  or a backdrop tap, since some iPad Magic Keyboards have no physical `Esc` key.
- **Reset view:** a floating button recenters pan/zoom, so a touch/trackpad user who
  pans the graph off-screen has a one-tap way back (no keyboard needed).
- **Build / packaging:** the server now defaults to the current working directory
  (was a hardcoded absolute path), and clears any throwaway clones left by a prior
  run on startup (the launcher SIGKILLs the old server, so its last clone leaked).
- **Final cleanup:** removed the legacy Tauri desktop shell — a Phase-4 stub that
  never read real data yet cost a whole CI job plus WebKitGTK/GTK system deps. The
  workspace is now four crates; the app is purely browser-first.

Still open: an optional performance pass (e.g. open the gix repo once per
`/api/commits` rather than ~5×).

## Backlog
- Dark/light theme toggle
- Minimap
- Diff view
- File history / blame
