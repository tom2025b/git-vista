# git-vista

A **browser-first** app that visualizes git history as a clean, zoomable
**vertical** graph — branches in stable colours, HEAD/branch/tag badges, commit
labels, and a per-commit detail panel. Built in Rust: a pure-logic core, a native
git reader (gix), a small HTTP server, and a **Leptos → WebAssembly** UI.

It's designed to be run on a machine and opened from a browser — including
**Safari on an iPad over the LAN**, which is why the UI is served over HTTP rather
than driven through a native shell (a browser can't read a git repo itself).

## Workspace layout

Four crates, each with one job:

```
git-vista/
├── Cargo.toml                    # workspace root
├── rust-toolchain.toml           # stable toolchain + wasm32 target
├── gv                            # launcher: rebuild the SPA + serve a repo
└── crates/
    ├── git-vista-core/           # pure logic — NO UI, NO filesystem, wasm-safe
    │   └── src/
    │       ├── model.rs          # serializable data types (server ⇄ UI)
    │       └── layout.rs         # vertical lane assignment + per-branch colour
    ├── git-vista-git/            # native git reading via gix (native-only)
    │   └── src/
    │       ├── history.rs        # walk_history, read_commit, read_remote_commits
    │       ├── refs.rs           # HEAD, branches, tags, checked-out branch
    │       └── github.rs         # origin URL → GitHub web base
    ├── git-vista-server/         # axum HTTP backend
    │   └── src/main.rs           # serves the SPA + the /api/* endpoints
    └── git-vista/                # the Leptos wasm UI (bin: git-vista-ui)
        ├── index.html            # Trunk entry point
        ├── styles.css
        └── src/                  # app.rs (view), camera, geometry, color, …
```

`git-vista-git` is kept **separate** from `git-vista-core` on purpose: gix reads a
filesystem repo and can't compile for wasm, so keeping it out of `core` lets the
browser frontend depend on a clean, wasm-safe core. Both the server and the UI
share `git-vista-core`'s types, so the same structs flow from the git walker
through JSON into the UI with no duplication.

## Architecture

```
  browser (SPA, wasm)                    git-vista-server (native)
  ────────────────────      HTTP         ─────────────────────────
  fetch /api/commits   ───────────────▶  walk_history + layout  ─┐
  fetch /api/commit/id ───────────────▶  read_commit            ─┤ gix reads
  POST  /api/branch    ───────────────▶  git branch  (shell)    ─┤ the repo on
  POST  /api/commit    ───────────────▶  git commit  (shell)    ─┤ the filesystem
  POST  /api/merge|push|delete-branch ▶  git … (shell)          ─┤
  POST  /api/clone     ───────────────▶  git clone → temp dir   ─┘
```

Everything is same-origin, so there's no CORS and no hardcoded host — the server
serves both the wasm bundle and the API on `:8080`.

## Features (through Phase 12)

- Vertical commit graph with robust lane assignment (branches, merges, octopus).
- Pan & zoom via **Pointer Events** — drag to pan, wheel to zoom on desktop,
  one-finger drag + two-finger pinch on iPad/Safari.
- Stable **per-branch colours**, and HEAD / branch / tag badges beside commits.
- Commit labels (message · short hash · author · local date), with **level of
  detail** (text hidden when zoomed out) and **viewport virtualization** (only
  on-screen rows are rendered, for large histories).
- **GitHub links** on commits/refs when the repo has a `github.com` origin — only
  for pushed objects, so a link never 404s.
- Write actions from the graph's context menu: create branch, commit, merge, push,
  delete branch (each confirmed in an iPad-safe in-app modal).
- **Commit detail panel** (Phase 10): "View details" opens a side panel with the
  full message body and both author & committer signatures; parent hashes are
  clickable to walk up the history.
- **Open URL** (Phase 12): paste a public `https://`/`http://`/`git://` URL to
  clone and view any repo **read-only** (all write actions hidden + refused).
- **Controls & shortcuts** (Phase 13): drag/one-finger to pan, wheel/pinch to zoom,
  plus keyboard shortcuts on desktop and the iPad Magic Keyboard — `+`/`-` zoom, `0`
  resets the view, `r` refreshes, `Esc` closes the open menu/panel. A **Reset view**
  button recenters the camera for pure touch/trackpad use (no keyboard needed).

See `DESIGN.md` for the phased roadmap and `PROJECT_MEMORY.md` for the running
per-phase handoff notes.

## Prerequisites

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk
```

A working `git` on `PATH` (the server shells out to it for writes and clones); the
history read itself uses `gix`'s pure-Rust reader.

## Running

The normal path is the `gv` launcher: it does a clean rebuild of the wasm SPA,
then starts the server pointed at a repo.

```sh
./gv                  # visualise the CURRENT directory's repo
./gv ~/code/myproj    # visualise another repo by path
```

Then open the URL it prints:

- on this machine: `http://localhost:8080/`
- from an iPad on the same Wi-Fi: `http://<this-machine-LAN-IP>:8080/`

Under the hood that's just:

```sh
( cd crates/git-vista && trunk build )        # build the wasm bundle into dist/
cargo run -p git-vista-server -- <repo-path>  # serve SPA + API on :8080
```

Frontend-only iteration (no API, no real data) still works with
`cd crates/git-vista && trunk serve`.

## Tests

```sh
cargo test -p git-vista-core     # layout, model
cargo test -p git-vista-git      # history/refs/github readers against fixtures
```

The core and git crates are headlessly unit-tested against fixture repositories;
the frontend is view-assembly over those tested pieces.

## Status

Working browser-first git visualizer, complete through **Phase 12** (and the
Phase 10 commit detail panel). **Phase 13** — packaging & polish is **in progress**
(icons, keyboard shortcuts, reset-view, shippable server defaults). Remaining:
**Phase 11** — search & filter, and the rest of Phase 13. See `DESIGN.md`.
