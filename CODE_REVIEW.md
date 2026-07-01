# git-vista — Code Review

_Date: 2026-07-01 · Scope: full workspace at the Phase 10 tip, reviewed while
preparing Phase 13. Items marked **FIXED** were addressed in the Phase 13
packaging-&-polish change set; the rest are recorded for later._

## Overall assessment

This is a strong, mature codebase. The four-crate split is clean and principled
(pure wasm-safe `core`, native-only `git` reader, `server`, Leptos `ui`), the
layout engine is pure and thoroughly unit-tested, and the "why" comments are
unusually good — they capture the reasoning behind non-obvious decisions
(lane recycling, stable per-branch colour, iOS WebKit quirks, 404-avoidance on
GitHub links). At review time: **73 tests pass, clippy is clean on both the host
and `wasm32-unknown-unknown` targets**, and the wasm bundle builds. There are few
real bugs; most findings below are polish, robustness, or conscious trade-offs.

## Correctness

### 1. Event-listener leak on graph reload — **FIXED**
`graph_canvas` is re-invoked on every graph reload (each Refresh / clone), and the
window `resize` listener was registered with `Closure::forget()`. So each reload
stacked another live listener bound to the *new* signals on top of the old,
now-disposed ones. Harmless-ish for `resize` (a stale write), but it would have
been a real bug for the new `keydown` handler (duplicate refresh/zoom).
**Fix:** both the `resize` and `keydown` listeners are now removed via
`on_cleanup` + `remove_event_listener_with_callback` instead of `forget()`, so the
old handler is dropped when the reactive owner is disposed.
_File:_ `crates/git-vista/src/app.rs` (`graph_canvas`).

### 2. Hardcoded personal default repo — **FIXED**
`DEFAULT_REPO` was `"/home/tom/projects/git-vista"` — not shippable. The `gv`
launcher always passes a path, so it only bit when the server was run directly
with no argument, but a hardcoded personal absolute path shouldn't ship.
**Fix:** defaults to `"."` (current working directory, canonicalised at startup).
_File:_ `crates/git-vista-server/src/main.rs`.

### 3. Leaked throwaway clones across runs — **FIXED**
The `gv` launcher `pkill -9`s the previous server on restart, so its last
Phase-12 clone was never cleaned up and accumulated under the OS temp dir over
time (each `git-vista-clones/clone-*`).
**Fix:** the server now `remove_dir_all`s `clones_root()` on startup (guarded by
`.exists()`). Nothing is served from there yet at startup, so it's safe; the next
clone recreates it.
_File:_ `crates/git-vista-server/src/main.rs`.

### 4. `Oid::short()` slices by byte index — latent, not a live bug
`&self.0[..self.0.len().min(7)]` is a byte slice. It's safe here only because git
object ids are always hex ASCII (one byte per char), so `[..7]` can never split a
codepoint. Worth a comment or a `char`-based slice if `Oid` ever holds non-hex
text. _File:_ `crates/git-vista-core/src/model.rs`.

## Security posture (acceptable under the documented threat model)

The stated model is "a personal viewer on a trusted home LAN." Under that model
these are fine; they'd matter on an untrusted network.

- **No auth, binds `0.0.0.0:8080`.** The write endpoints (branch / commit / merge
  / **push** / delete-branch) and the clone endpoint are reachable by anyone on
  the LAN. `push` in particular reaches the network. DNS-rebinding is the realistic
  cross-origin vector (a JSON-body POST triggers a CORS preflight the server
  doesn't answer, so plain cross-site JS can't complete it).
- **`/api/clone` fetches arbitrary URLs.** `validate_clone_url` correctly gates to
  `http(s)://` / `git://`, blocks option-injection (leading `-`) and embedded
  whitespace, and the URL is passed as its own argv entry after `--`. But it can
  still be pointed at internal hosts (a mild SSRF), and `git://`/`http://` are
  cleartext. `git clone` of a non-repo just fails, limiting the blast radius.
- _If this ever leaves a trusted LAN:_ add a localhost-bind option and/or a shared
  token, and consider an allowlist of clone hosts.

## Performance (all fine at personal scale)

- **`/api/commits` opens the gix repository ~5×** — `walk_history`, `read_refs`,
  `read_head_branch`, `github_web_base`, and `read_remote_commits` each call
  `gix::open_opts` — and walks history **twice** (once for the graph, once for the
  remote-commit set). Sharing one opened repo would roughly halve the work. Only
  worth doing if large repos feel slow. _File:_ `crates/git-vista-server/src/main.rs`
  (`commits`), `crates/git-vista-git/src/history.rs`.
- **Blocking gix reads run on the async handler thread** (no `spawn_blocking`). The
  default multi-thread runtime + single user means this can't deadlock, but a huge
  repo read ties up a worker for the duration.
- **The trunk-colour "extend upward" loop** in `assign_branch_colors` does a linear
  `rows.iter().find(...)` per step. In practice it only runs for commits sitting
  *above* main's tip in the trunk lane (usually a handful), so it's cheap — but it's
  O(ahead × rows) in the worst case; a child index would make it O(ahead).
  _File:_ `crates/git-vista-core/src/layout.rs`.
- **Micro:** the node / message / meta `<For>`s each allocate a `Vec<usize>` from
  the same visible range every render. Negligible (only the on-screen rows).
  _File:_ `crates/git-vista/src/app.rs`.

## Improvement recommendations

- **Remove the legacy Tauri shell — DONE (Phase 13).** It was a Phase-4 stub
  (`list_commits` returned an empty graph; nothing used it) that still cost a whole
  CI job plus WebKitGTK/GTK/appindicator/librsvg system deps and a 5th workspace
  member. Removed the crate, the workspace member, the CI `tauri` job, the
  `src-tauri/gen` gitignore entry, and every Tauri mention in docs/comments. The
  workspace is now four crates and the app is purely browser-first.
- **Phase 11 — search & filter** is the main missing user-facing feature.
- **Optional perf pass:** open the gix repo once per `/api/commits` and thread it
  through the readers (see Performance above).
- **Backlog polish** (already tracked in `DESIGN.md`): dark/light theme toggle,
  minimap, diff view, file history / blame.

## Fixed as part of Phase 13

Keyboard shortcuts (`+`/`-` zoom, `0` reset, `r` refresh, `Esc` closes the open
overlay — Esc bonus-only, since the target iPad Magic Keyboard has no Esc key), a
floating **Reset view** button for touch/trackpad use, an inline SVG favicon plus
mobile meta tags, the shippable server default and startup clone cleanup (items 2
and 3), the listener-leak fix (item 1), and removal of the legacy Tauri shell.
See `DESIGN.md` and the Phase 13 entry in `PROJECT_MEMORY.md`.
