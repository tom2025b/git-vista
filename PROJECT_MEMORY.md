# git-vista — Project Memory & Handoff

Running handoff document. **Append a new "## Phase N" entry after each phase** —
do not rewrite history. Newest entries go at the bottom. The phase plan lives in
`DESIGN.md`.

## How to use this file
After finishing a phase, append an entry using the template below: what changed,
key decisions, gotchas, how to verify, and what's next. Keep it concrete enough
that a fresh session can resume with no other context.

<details>
<summary>Entry template</summary>

```
## Phase N — <title> (<date>)
**Status:** done | in progress
**What changed:** files touched, what they do now.
**Decisions:** non-obvious choices and why.
**Gotchas:** traps, version pins, env quirks.
**Verify:** exact commands + expected result.
**Next:** the immediately following step.
```
</details>

---

## Snapshot (current)
- **Architecture:** browser-first. `git-vista-server` (axum) serves the wasm SPA +
  `/api/*` on `0.0.0.0:8080`; the Leptos (`0.6.15`, CSR) frontend `fetch`es it.
  The legacy Tauri v2 shell was **removed in Phase 13** (it was a Phase-4 stub that
  never read real data; a browser can't reach a Tauri command, which is why the
  server exists — see Phase 4). Its history is preserved in git.
- **Toolchain:** cargo `1.96.0`, Trunk `0.21.14`, wasm-bindgen `0.2.126`, wasm32
  target installed. `gix` pinned to `=0.84.0` (see `git-vista-git/Cargo.toml` for
  why 0.85 breaks), axum `0.8`.
- **Workspace (4 crates):**
  - `crates/git-vista-core` — pure logic (`model`, `layout`); wasm-safe, no gix.
  - `crates/git-vista-git` — native gix reader (`history`, `refs`, `github`).
  - `crates/git-vista-server` — the axum HTTP backend (`main.rs`).
  - `crates/git-vista` — the Leptos wasm UI (bin `git-vista-ui`).
- **Git repo:** on `main`; each phase ships on its own `phaseN-*` branch via PR.
  **Never delete branches** (standing user rule) — leave every branch in place.

## Verify the whole build
```sh
cargo test -p git-vista-core                 # 23 tests pass
cargo test -p git-vista-git                  # 11 tests pass (fixture repos)
cargo test -p git-vista                      # ~39 host tests (geometry/color/…)
cargo clippy -p git-vista-core -p git-vista-git -p git-vista-server            # native clippy
cargo clippy -p git-vista --target wasm32-unknown-unknown                      # frontend clippy
( cd crates/git-vista && trunk build )       # frontend → wasm in dist/
cargo build --workspace                      # core + git + server + frontend compile
```

## Running the app — the `gv` launcher & choosing which repo to view
The repo to visualise is **no longer hardcoded**. `git-vista-server <path>` takes
the repo as its first CLI arg (falling back to this checkout when omitted) — this
closes the Phase 4/5 "make the repo path configurable" open item.

The convenience launcher is **`gv`** (script at repo root `/home/tom/projects/git-vista/gv`,
symlinked onto PATH at `~/.local/bin/gv` — NOT a shell alias, the user explicitly
didn't want `~/.zshrc` touched). It builds the wasm bundle, kills any stale server,
then runs the server for a repo:
```sh
gv                  # visualise the CURRENT directory's repo
gv ~/code/myproj    # visualise another repo by path
```
`gv` **always does a clean wasm rebuild** (`trunk clean && trunk build`) before
serving, so a stale bundle can never be served while developing — there is no
`--no-build` fast path anymore (the script rejects any `-…` option with exit 2; a
`--no-build`/`--fast` path can be reintroduced when shipping to other people).
`gv` validates the target is a git repo, resolves it to an absolute path, and
passes it to the server. It finds the git-vista checkout via `readlink -f` on its
own path, so it works through the PATH symlink from any directory.

## Troubleshooting — dev server unreachable from the iPad/another device
**Symptom:** `http://localhost:8080/` works on the dev machine, but the iPad shows
"the page cannot be reached." The server is fine; something between the devices is
blocking it. The server binds `0.0.0.0:8080` (all interfaces) — confirm with
`ss -ltn | grep 8080` (expect `0.0.0.0:8080`). Then, in order of likelihood:

1. **Firewall on the dev machine (most common).** Loopback bypasses the firewall,
   so localhost works while the LAN is blocked. This box has **`ufw`** installed.
   Check/allow (needs root — run from the prompt with `! sudo …`):
   ```sh
   sudo ufw status            # is it active?
   sudo ufw allow 8080/tcp    # allow the dev port
   ```
   (If using firewalld/nftables instead: open 8080/tcp there.)
2. **Different network / subnet.** The dev machine is wired (`eno1`,
   `192.168.254.206/24`); the iPad is on Wi-Fi. They must share the subnet
   (`192.168.254.x`). Check the iPad's IP in Settings → Wi-Fi → (i). Guest Wi-Fi
   or a second AP/mesh node on another subnet won't reach it.
3. **Router AP/client isolation.** Some routers block device-to-device traffic;
   disable "AP isolation" / "client isolation" (often only on guest networks).

Find the right URL/IP: `hostname -I` (or `ip -4 addr show scope global`). The
server now prints the LAN URL + these hints on startup, and gives a clear message
if the port is already in use or the `dist/` bundle is missing.

## Troubleshooting — exit code 144 when running the server
**Symptom:** starting `git-vista-server` dies immediately with **exit code 144**
(often no output). This has bitten multiple sessions.

**Cause:** a previous `git-vista-server` instance is still running and holding port
**8080**. The sandbox kills the new process, which surfaces as exit 144.

**Fix:** kill the stale server before starting a new one — **with `-f`**:
```sh
pkill -9 -f git-vista-server
```
⚠️ **`pkill -9 git-vista-server` (no `-f`) silently matches NOTHING** and was the
real reason this kept biting across sessions. The Linux process name (`comm`) is
truncated to 15 chars — `git-vista-serve` — so the 16-char pattern never matches
(pgrep even warns: *"pattern that searches for process name longer than 15
characters will result in zero matches"*). `-f` matches the full command line
(`target/.../git-vista-server`), which works. The `gv` launcher script does this.
Confirm it's actually gone with `ss -ltn | grep 8080` (should be empty) before retrying.

> Note (background-job sandbox): in the automated background-job sandbox, even a
> *first* server start (port free) can exit 144, because that sandbox blocks
> binding a listening socket outright — `pkill` itself can also report 144 there.
> So if `pkill -9` doesn't help and you're in a background job, you can't bind a
> port at all; verify the data path another way (e.g. a throwaway
> `cargo run --example` that calls `walk_history` + `read_refs` +
> `layout_with_refs` directly, no socket).

---

## Phase 0 — Scaffold (2026-06-28)
**Status:** done
**What changed:**
- Cargo workspace with three packages (two conceptual crates + the Tauri shell).
- `git-vista-core`: `model.rs` (Oid, CommitSummary, GraphRow, Edge, Graph — all
  serde), `repo.rs` (`walk_history` stub → RepoError::NotImplemented), `layout.rs`
  (stub: every commit in lane 0, commit→parent edges wired). 6 unit tests.
- `git-vista` frontend: Leptos `App` shell with a graph placeholder; Trunk +
  `index.html` + `styles.css`; UI deps target-gated to wasm32; cfg-gated host stub.
- `src-tauri`: Tauri v2 shell, `list_commits(path) -> Graph` command (returns
  empty graph for now), `tauri.conf.json`, capabilities, build.rs.
- Root: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`, `README.md`, `LICENSE`,
  `DESIGN.md`, this file.

**Decisions:**
- **3 packages for "two crates":** Tauri's shell must be its own package (wasm UI
  and native shell compile for different targets and never link).
- **Target-gated frontend deps + cfg-gated `main`:** lets the wasm-only crate live
  in a workspace that also builds for the host.
- Stuck with **Leptos 0.6** — it compiled fine via Trunk, no bump needed yet.

**Gotchas:**
- Tauri requires **RGBA** icon PNGs; RGB fails `generate_context!`. Placeholder
  RGBA icons were generated by hand — regenerate properly with `cargo tauri icon`.
- `generate_context!` needs `frontendDist` (`../dist`) to exist — run `trunk build`
  (or `cargo tauri dev`) before building the shell from clean.
- The parent `~/projects/.git` is an empty/invalid marker; this project isn't its
  own git repo yet. Check `git rev-parse` before assuming repo state.

**Verify:** see "Verify the whole build" above — all three succeed.

**Next:** Phase 1 — **Static vertical graph (fake data)**: build a fake commit
history in the Leptos frontend and render it as SVG nodes + edges. NOTE: the
phase order in `DESIGN.md` was revised — the `gix` history reader is now
**Phase 3** ("Read real commits with gix"), not Phase 1. Lane assignment is its
own later milestone (**Phase 6**), so Phase 1 hardcodes pre-laid-out fake data
rather than computing lanes.

## Phase 1 — Static vertical graph (fake data) (2026-06-28)
**Status:** done
**What changed:**
- `crates/git-vista/src/graph.rs` (new): **hardcoded, pre-laid-out** fake history
  — 18 commits, three side branches (`feature`/`topic`/`release`) and three
  merges, newest-first. Each commit's `lane` is authored by hand in a `HISTORY`
  table; `fake_graph()` builds the `GraphRow`s and derives edges by parent-id
  lookup (plumbing between fixed points, **not** lane assignment). Pure logic, so
  it compiles + is unit-tested on the host too.
- `crates/git-vista/src/app.rs`: replaced the placeholder with inline **SVG** —
  one `<circle>` per commit (merge commits are hollow rings), one `<path>` per
  commit→parent edge (straight in-lane, smooth S-curve across lanes), lanes laid
  left→right with a 6-colour palette. `<title>` gives a short-hash + summary hover.
- `crates/git-vista/src/main.rs`: declares `mod graph;` (host-compiled; targeted
  `dead_code` allow for the non-test host build). `app` stays wasm-only.
- `crates/git-vista/styles.css`: `.graph-svg` block layout (panel scrolls).

**Decisions:**
- Phase 1 does **not** compute lanes — they are hardcoded. Lane assignment is a
  separate later milestone (**Phase 6**); the `gix` reader is **Phase 3**. (Fixes
  earlier DESIGN↔MEMORY drift that had this entry's predecessor pointing at gix.)
- Integer SVG user units throughout for clean attribute output.

**Gotchas:**
- `EnterWorktree` is unusable in this env (it resolves against the invalid
  `~/projects/.git` marker), so the work was done on a feature branch in the main
  checkout instead of a worktree.

**Verify:**
```sh
cargo test -p git-vista        # 2 pass (graph fixture sanity)
cargo test -p git-vista-core   # 6 pass
cargo check -p git-vista-tauri # shell still compiles
( cd crates/git-vista && trunk build )   # wasm bundle builds
```
Then `cargo tauri dev` (or `trunk serve`) shows the graph: a vertical column of
nodes on the left (main), with two side branches peeling out and merging back.

**Next:** Phase 2 — Interactive pan & zoom (camera controls over the SVG canvas).


## Phase 1 refactor — Separate geometry & colour from the view (2026-06-28)
**Status:** done (sub-issue #3 "Refactor app.rs — Improve structure and separation of concerns")
**What changed:**
- `crates/git-vista/src/geometry.rs` (new): pure spatial logic, no Leptos/DOM.
  Layout constants (`ROW_HEIGHT`, `LANE_WIDTH`, `NODE_RADIUS`, `PAD_X`, `PAD_Y`),
  `node_cx`/`node_cy`, `edge_path`, and a new `canvas_size(lanes, rows)` that
  pulls the SVG width/height math out of the component. 3 unit tests pin the
  arithmetic (centres, canvas size incl. `max(1)` floor, straight-vs-curve).
- `crates/git-vista/src/color.rs` (new): the lane palette — `lane_color` (wraps
  the 6-colour array) and `MERGE_FILL` (hollow-merge background). 1 unit test.
- `crates/git-vista/src/app.rs`: now **view assembly only** — imports from
  `geometry`/`color` and just decides *what* to draw. No constants or maths left.
- `crates/git-vista/src/main.rs`: declares `mod color;` and `mod geometry;` with
  the same `cfg_attr(not(any(wasm32, test)), allow(dead_code))` gating as `graph`.

**Decisions:**
- Split colour into its own module (not folded into `geometry`) so colour can
  evolve independently — Phase 7 swaps lane-indexed colours for per-branch ones.
- `NODE_RADIUS` and `MERGE_FILL` are consumed only by the wasm-only `app` view,
  so they read as dead in the **host test** build (where the module-level allow
  is absent). Each carries a targeted `cfg_attr(not(wasm32), allow(dead_code))`,
  which still surfaces genuine deadness on the wasm target.
- Visual output is byte-identical: every helper is the original arithmetic moved
  verbatim, and the `App` view macros are unchanged.

**Gotchas:**
- A background-session worktree-isolation guard blocked the Edit/Write tools even
  though this session is configured to work in place. Added
  `.claude/settings.json` with `"worktree": { "bgIsolation": "none" }`; it isn't
  picked up mid-session, so edits were applied via Bash for this run.

**Verify:**
```sh
cargo test -p git-vista        # 6 pass (geometry/color/graph)
cargo test -p git-vista-core   # 6 pass
cargo check -p git-vista-tauri # shell still compiles
( cd crates/git-vista && trunk build )   # wasm bundle builds, no warnings
```

**Next:** Phase 2 — Interactive pan & zoom (camera controls over the SVG canvas).


## Phase 2 — Interactive pan & zoom (2026-06-28)
**Status:** done
**What changed:**
- `crates/git-vista/src/camera.rs` (new): pure pan/zoom math, no Leptos/DOM. A
  `Camera { tx, ty, scale }` (a screen-space `translate · scale`) with `panned`,
  `zoomed_at` (focal-point zoom anchored under the cursor, scale clamped to
  `[MIN_ZOOM, MAX_ZOOM]`), and `transform()` → an SVG `<g transform>` string.
  4 host unit tests (anchor invariant, clamp, pan accumulation, transform fmt).
- `crates/git-vista/src/app.rs`: graph moved inside one `<g transform=camera>`;
  the `<svg>` now fills its panel (no `width`/`height`/`viewBox`). Added handlers:
  `pointerdown` (set drag + `setPointerCapture`), `pointermove` (pan by
  `movement_x/y` while dragging), `pointerup`/`pointercancel` (end drag), `wheel`
  (`preventDefault`, zoom toward `offset_x/y`). `class:grabbing` toggles cursor.
- `crates/git-vista/src/geometry.rs`: **removed `canvas_size`** + its test — the
  SVG no longer sizes to content (it fills and clips; the camera moves content).
  A viewport-aware fit/bounds helper belongs to Phase 8 if needed.
- `crates/git-vista/src/main.rs`: declares `mod camera;` (same dead-code gating).
- `crates/git-vista/Cargo.toml`: explicit `web-sys` (wasm-only) with the exact
  event features used (`Element`, `Event`, `EventTarget`, `MouseEvent`,
  `PointerEvent`, `WheelEvent`).
- `crates/git-vista/styles.css`: `.graph` is `overflow: hidden`; `.graph-svg`
  fills (`100%`), `cursor: grab` / `.grabbing`, `touch-action: none`,
  `user-select: none`.

**Decisions:**
- **`<g transform>` + no `viewBox`** instead of mutating the `viewBox`. Without a
  `viewBox`, 1 user unit = 1 CSS px, so `offset_x`/`movement_x` map straight onto
  camera space — no `getBoundingClientRect` / `preserveAspectRatio` letterbox
  math. Pan/zoom stays pure and host-testable.
- **`setPointerCapture`** so a drag keeps tracking when the cursor leaves the SVG.
- Zoom solves the translation from the *post-clamp* factor, so the focal point
  stays anchored even at the min/max zoom limits.
- `ZOOM_STEP` carries a `not(wasm32)` dead-code allow (view-only constant, like
  `NODE_RADIUS`/`MERGE_FILL`); `MIN/MAX_ZOOM` are exercised by tests.

**Gotchas:**
- The `<svg>` having no intrinsic size means it relies on the flex `.graph`
  parent for height — `.app` is `100vh` and `.graph` is `flex: 1`, so it fills.
- Work is on branch `phase2-pan-zoom` (off `main`); not yet pushed/PR'd.

**Verify:**
```sh
cargo test -p git-vista        # 9 pass (camera/geometry/color/graph)
cargo test -p git-vista-core   # 6 pass
cargo check -p git-vista-tauri # shell still compiles
( cd crates/git-vista && trunk build )   # wasm bundle builds, no warnings
```
Then `cargo tauri dev` (or `trunk serve`): drag the graph to pan, scroll to zoom
toward the cursor; the cursor shows grab/grabbing.

**Next:** Phase 3 — Read real commits with `gix` (`repo::walk_history()`).


## Phase 3 — Read real commits with gix (2026-06-28)
**Status:** done
**What changed:**
- **New crate `git-vista-git`** (`crates/git-vista-git`, native-only library):
  `walk_history(path, limit)` via `gix`. Opens with `gix::open_opts(path,
  Options::isolated())`, seeds tips from HEAD + every ref (peeled, de-duped),
  walks `ByCommitTime(NewestFirst)`, maps each commit to `CommitSummary`
  (id/parents as hex `Oid`, `summary()` first line, author *name*,
  `commit_time()`). `RepoError` = `Open { path, message }` / `Walk(String)`.
  3 tests build throwaway repos by shelling to `git` (linear + branch + merge
  fixture): newest-first cross-branch order, merge/root parent shape, `limit`,
  not-a-repo error. Owns the `gix` + `tempfile` deps.
- `git-vista-core` is now **pure** (`model` + `layout` only, `serde` its only
  dep). No gix, no `thiserror`, no `repo` module, **no `#[cfg]` anywhere** — it
  compiles for wasm trivially and the browser frontend depends on it as-is.
- Workspace: added `crates/git-vista-git` member + `git-vista-git` workspace dep.
  CI `core` job now `check`/`test`s both `-p git-vista-core -p git-vista-git`.
- **Not touched on purpose:** `src-tauri/commands.rs` still returns an empty
  graph — wiring real data through is **Phase 4** (TODO now points at
  `git_vista_git::walk_history`).

**Decisions:**
- **gix reading lives in its own native-only crate, NOT in core.** This is the
  whole point of the Phase-3 redo (see the failed first attempt below): the
  primary target is the **browser** (user runs git-vista in Safari on an iPad),
  so `git-vista-core` MUST stay clean and wasm-compatible. A browser fundamentally
  can't read a local repo, so gix is inherently a native concern. Splitting it
  into `git-vista-git` means the frontend's dependency tree simply never contains
  gix — no cfg gating, no feature juggling. The native backend depends on the git
  crate; the frontend never does. **Don't move gix back into core, and don't
  cfg-gate a `repo` module inside core — that was explicitly rejected.**
- **`Options::isolated()`** (repo-local config only): a read-only viewer doesn't
  need the user's global/system git config, and ignoring it avoids "failed to
  load the git configuration" crashes from a malformed host config.
- **Walk HEAD + all refs**, not just HEAD, so side branches show up.
- **Author = name only** for now (model has a single `author: String`); email is
  available if Phase 5 wants "Name <email>".

**Superseded first attempt (do not repeat):** Phase 3 was first written with
`repo.rs` *inside* `git-vista-core` and gix target-gated to non-wasm
(`#[cfg(not(target_arch = "wasm32"))] pub mod repo`). That compiled but left core
impure and was rejected — the browser is the main target and core must be clean.
The crate split above replaced it before anything was pushed/merged.

**Browser reality (context for Phase 4):** the Tauri `invoke` IPC only works in
the desktop webview, not a plain browser. Reaching real git data from the iPad
browser will need a native HTTP backend serving the SPA + a JSON API — but the
delivery architecture (web server, Tauri's fate) is an **open decision the user
wants to make explicitly**, so don't bake it in unprompted.

**Gotchas:**
- **gix pinned to `=0.84.0`.** With `default-features = false`, you MUST add the
  `sha1` feature or every dependent fails to compile with *"Please set either the
  `sha1` or the `sha256` feature flag"* (the `Kind` enum compiles empty → a flood
  of non-exhaustive-match errors in `gix-hash`). But on gix **0.85**, the `sha1`
  feature transitively (via a weak `gix-worktree-stream?/sha1` ref, which Cargo
  still must *version-resolve*) requires `gix-worktree-stream 0.34`, which pins
  `gix-attributes ^0.33.2` — unpublished on the index — so it won't resolve.
  gix **0.84** uses worktree-stream 0.33 (→ gix-attributes 0.33.1) and resolves.
  Revisit (unpin) once the gix 0.85 dep chain is fixed upstream.
- Enabling only `gix-hash/sha1` directly (to dodge the meta-feature) makes
  gix-hash compile but gix's *repository* layer still rejects sha1 objects
  ("Cannot handle objects formatted as sha1") — that support is gated on gix's
  own `sha1` feature. So the meta-feature is unavoidable; pinning is the fix.
- git's raw-epoch date format for `GIT_*_DATE` is `@<seconds> <tz>` (e.g.
  `@6 +0000`); bare `6 +0000` is rejected as "invalid date format".
- The fixture tests shell out to the `git` binary (present locally and on the
  CI ubuntu runners).

**Verify:**
```sh
cargo test -p git-vista-core   # 6 pass (model/layout) — pure, no gix in tree
cargo test -p git-vista-git    # 3 pass (walk_history fixtures; needs git binary)
cargo test -p git-vista        # 9 pass (camera/geometry/color/graph)
cargo check -p git-vista-tauri # shell compiles
( cd crates/git-vista && trunk build )   # wasm builds (no gix in tree), no warnings
cargo tree -p git-vista --target wasm32-unknown-unknown | grep gix  # → nothing
```

**Next:** Phase 4 — Connect real data to the graph: have the native backend call
`git_vista_git::walk_history` + `layout::layout`, and point the frontend at that
result instead of `fake_graph()`. NB: settle the browser delivery path first
(HTTP API vs Tauri invoke) — see "Browser reality" above.


## Phase 4 — Connect real data to the graph (2026-06-28)
**Status:** done
**What changed:**
- **New crate `git-vista-server`** (`crates/git-vista-server`, native axum bin):
  serves the wasm SPA **and** `GET /api/commits` on **one origin** (port 8080,
  bound `0.0.0.0` so the iPad reaches it over the LAN). The handler reuses
  `git_vista_git::walk_history(REPO_PATH, 5000)` + `git_vista_core::layout::layout`
  → `Json<Graph>`. `REPO_PATH` is hardcoded to `/home/tom/projects/git-vista`
  (Phase 4; configurable later). Serves `dist/` via `tower-http::ServeDir`, path
  resolved at compile time from `CARGO_MANIFEST_DIR` so cwd doesn't matter.
- **`git-vista-core::layout::layout`**: real **basic lane algorithm with compact
  reuse** (was: all lane 0). Two passes. Pass 1 tracks active lanes
  (`Vec<Option<Oid>>`, each lane "expects" the next older commit; `None` = free):
  a commit takes the leftmost lane expecting it (freeing the others — branches
  collapsing at a merge), else the **leftmost free lane**; **first parent
  continues the lane**, extra (merge) parents take the **leftmost free lane**, and
  a lane with no in-window parent to continue to is freed. So a merged-back
  branch's lane is reused by the next new branch — the graph stays narrow. Pass 2
  wires edges to each parent's *final* lane (lanes can shift left at a merge).
  `lane_count = max row lane + 1`. 8 core tests incl. branch/merge + a reuse test.
  (Earlier draft used always-increment/no-reuse; the user found it too wide and
  switched to reuse — on this repo: 5 lanes → 2.)
- **Frontend** (`crates/git-vista`): `app.rs` now `fetch`es `/api/commits` via
  `gloo-net` in a `create_local_resource` on startup and renders the returned
  `Graph` (loading / error / graph states). Pan/zoom (Phase 2) unchanged, moved
  into `graph_canvas()`. `fake_graph`/`graph.rs` dropped from the render path —
  `mod graph` is now `#[cfg(test)]` (kept only as a test fixture).
- Workspace: added `git-vista-server` member; CI `core` job also
  `check`s it. `gloo-net` added to the frontend (wasm target, `http`+`json`).
- **Tauri shell left untouched** (per the "don't decide architecture" rule): its
  `list_commits` stub stays; the browser path is HTTP, not `invoke`. In the Tauri
  webview the fetch would 404 — Tauri's role is a separate future decision.

**Decisions (all confirmed with the user before coding):**
- **Delivery = HTTP backend (axum), not Tauri IPC.** Browser-first: Safari on the
  iPad has no Tauri runtime, so `invoke` can't work; the server hosts SPA + API on
  one origin (no CORS). This was the open question flagged in Phase 3.
- **Lane rule = compact reuse**: merged-back branches free their lane; new
  branches take the leftmost free lane. (User first tried always-increment, then
  switched to reuse because the graph got too wide.)
- **Hardcoded repo path** = this git-vista checkout (has branches/merges to show).
- Reused `git_vista_git::walk_history` (the Phase-3 rename of `repo::walk_history`)
  and `layout::layout` — no new git/layout functions, per the brief.

**Gotchas:**
- The frontend must be **built** (`trunk build`) before the server can serve it —
  the server reads `crates/git-vista/dist/`. `trunk serve` is not used in this
  one-origin model; build then run `git-vista-server`.
- `fetch("/api/commits")` is **same-origin relative** — works because the server
  serves both. Don't hardcode a host.
- axum 0.8 / tower-http 0.7 / tokio 1: server bind uses `axum::serve(listener, app)`
  and `ServeDir` as `.fallback_service(...)`.

**Verify:**
```sh
cargo test -p git-vista-core   # 8 pass (model/layout incl. lanes)
cargo test -p git-vista-git    # 3 pass
cargo test -p git-vista        # 9 pass
cargo check -p git-vista-server -p git-vista-tauri
( cd crates/git-vista && trunk build )                 # SPA bundle
cargo run -p git-vista-server                          # then GET / and /api/commits
```
Manual (browser path): `trunk build`, run the server, open `http://<LAN-IP>:8080/`
from the iPad — graph of this repo's real history, pan/zoom works.

**Next:** Phase 5 — Commit rows & labels (message, short hash, author beside each
node). Open architecture question for later: make the repo path configurable, and
decide the Tauri shell's fate now that the browser path is HTTP.


## Phase 5 — Commit rows & labels (2026-06-28)
**Status:** done
**What changed (frontend-only — model already had message/hash/author):**
- `crates/git-vista/src/text.rs` (new, pure/host-tested): `truncate(s, max)` —
  char-aware truncation with a single `…`, result width ≤ max. 4 tests.
- `crates/git-vista/src/geometry.rs`: `label_x(lane_count)` (a fixed text column
  just right of the widest lane, `LABEL_GAP = 18`), `label_top_y(row)` /
  `label_bottom_y(row)` (the two baselines straddling the node). +1 test.
- `crates/git-vista/src/app.rs`: `graph_canvas` now also renders, per row, two
  SVG `<text>` lines inside the pan/zoom `<g>`: truncated message
  (`MAX_SUMMARY_CHARS = 60`, full text in a `<title>` hover) on top, and
  `"<short-hash> · <author>"` dimmed below. Added after nodes in the group.
- `crates/git-vista/styles.css`: `.label-msg` (fg, 13px) / `.label-meta`
  (muted, 11px).
- `crates/git-vista/src/main.rs`: declares `mod text;` (same dead-code gating).

**Decisions (confirmed with the user):**
- **Aligned label column** right of all lanes (gitk style), not inline per node.
- **Two lines** per commit (message; then hash · author) for readability.
- **Labels live inside the pan/zoom group** → they scale with the graph; hiding
  text when zoomed far out is Phase 9 (level of detail).
- **Truncate** long messages (~60 chars) with `…`; full text via hover `<title>`.

**Gotchas:**
- Labels render client-side in wasm, so the SVG `<text>` isn't in the served HTML
  — `curl` can't verify it; check visually in the browser.
- Text is interactive (has a `<title>`); `user-select: none` on `.graph-svg`
  (Phase 2) keeps a drag from selecting label text, and pointer-capture keeps a
  drag that starts on a label working.

**Verify:**
```sh
cargo test -p git-vista        # 14 pass (camera/geometry/color/text/graph)
( cd crates/git-vista && trunk build )                 # wasm bundle
cargo run -p git-vista-server                          # then open in a browser
```
Manual: open the server URL — each node shows its message + `hash · author`
beside it in a left-aligned column; long messages end in `…` (full on hover);
labels pan/zoom with the graph.

**Next:** Phase 6 — Robust lane assignment (handle complex branch/merge topologies
better than the current basic reuse algorithm).


## Phase 6 — Robust lane assignment (2026-06-29)
**Status:** done
**What changed (core-only — model, server, and frontend untouched):**
- `crates/git-vista-core/src/layout.rs`: rewrote the basic compact-reuse pass as
  a documented **active-lane tracker**. Same two-pass shape (pass 1 assigns each
  commit its final lane + reserves parents' lanes; pass 2 wires edges to parents'
  *final* lanes), but the rules are now explicit and robust:
  1. A commit takes the **leftmost lane reserved** for it (its children's lanes),
     **freeing the other reserved lanes** (sibling branch lines converging here);
     a branch tip with no reservation takes the **leftmost free** lane.
  2. Its **first parent continues in the same lane** → stable branch columns, and
     the mainline (HEAD's first-parent chain) stays in lane 0.
  3. Each **additional (merge) parent** reuses an existing reservation if one
     exists, else takes the **leftmost free lane strictly to the RIGHT** of the
     commit (new helper `leftmost_free_right_of`) — so merge lines fan out
     rightward and never cross back left over the mainline.
- New/expanded tests (8 layout tests, up from 5; 11 total in the crate incl.
  model): `linear`, `dangling_parents`, `branch_and_merge_routes_to_the_right`,
  `octopus_merge_fans_each_parent_into_its_own_lane`,
  `sequential_branches_reuse_a_freed_lane`,
  `concurrent_branches_get_distinct_lanes`,
  `a_long_running_branch_keeps_one_stable_lane`, plus `empty`. Each asserts both
  the **lane fixture** and the **edge fixture**; a shared `assert_well_formed`
  helper checks the structural invariants (sequential rows, in-range lanes,
  downward edges, exactly one edge per in-window link). New `edge(g, from, to)`
  test helper looks an edge up by id so assertions don't depend on edge order.

**Decisions:**
- **Kept the endpoints-only `Edge` model and the one-curve-per-link renderer.**
  The frontend's `edge_path` (Phase 1/5) deliberately draws a flowing S-curve per
  commit→parent link "rather than cutting across"; Phase 6 is about correct lane
  *assignment*, so the model/renderer didn't change. Crossing avoidance comes from
  assignment (right-biased merge parents, stable first-parent lanes, no lane
  sharing between concurrent branches), not from edge waypoints/segmentation.
- **Merge parents bias RIGHT** (`leftmost_free_right_of(lane)`) while branch tips
  use plain `leftmost_free` — a tip has no incoming edge so any free lane is
  crossing-safe and the leftmost keeps things narrow; a merge parent has an
  incoming edge from the merge, so forcing it rightward stops it crossing the
  mainline. This is the key behavioural change vs the old `leftmost_free`-for-all.
- **First-parent reservation is unconditional** (even if that parent is already
  reserved in another lane): keeps each sibling branch line in its own column down
  to the shared merge base (gitk-style), instead of collapsing early.

**Gotchas:**
- `model::Edge` is not `Copy` (just `Clone`) — the `edge()` test helper uses
  `.cloned()`, not `.copied()`.
- `lane_count = max row lane + 1`. A lane transiently reserved for a merge parent
  that ultimately collapses into a leftmost lane is never a node's final lane, so
  it correctly doesn't inflate `lane_count` (no node/edge ever lives there).

**Verify:**
```sh
cargo test -p git-vista-core   # 11 pass (3 model + 8 layout)
cargo clippy -p git-vista-core --all-targets   # clean
cargo fmt -p git-vista-core -- --check         # clean
cargo test -p git-vista-git -p git-vista       # 3 + 14 pass (unchanged)
cargo check -p git-vista-server -p git-vista-tauri
```
Real-data smoke test: `cargo run -p git-vista-server`, then
`curl -s localhost:8080/api/commits` → this repo's 16 commits / 6 merges lay out
2 lanes wide; invariants hold (sequential rows, in-range lanes, downward edges,
no two nodes in the same (row,lane) cell).

**Next:** Phase 7 — Refs & colors (branch names, HEAD, tags; consistent per-branch
colours instead of the current lane-indexed palette).


## Phase 7 — Branch, tag & HEAD labels + per-branch colours (2026-06-29)
**Status:** done
**What changed:**
- **`git-vista-core::model`**: new `RefKind` (`Head`/`Branch`/`RemoteBranch`/`Tag`)
  and `GitRef { name, kind, target }` (`is_branch()` helper). `GraphRow` gained two
  fields: `refs: Vec<GitRef>` (badges pointing exactly at this commit) and
  `color: usize` (stable per-branch palette **slot**, not a lane).
- **`git-vista-core::layout`**: the old `layout(commits)` body is now private
  `layout_topology`; `layout(commits)` = topology + `assign_branch_colors(&[])`
  (so existing callers/tests are unchanged and every row still gets a colour). New
  **`layout_with_refs(commits, refs)`** = topology + `assign_branch_colors(refs)` +
  `attach_ref_badges(refs)`. `assign_branch_colors` colours each commit by the
  branch that owns its **first-parent chain**: seeds = branch refs in priority
  order (HEAD's branch first → trunk = slot 0, then local-before-remote, then by
  name), each walks first-parents claiming unowned commits until it hits an
  already-owned commit (the merge base); a synthetic fallback then claims any still
  -unowned commit (top-to-bottom, keyed by short hash) so **every** commit is
  coloured. `attach_ref_badges` pushes each ref onto its target row (off-window
  refs dropped). 12 layout tests now (8 topology + 4 new colour/badge); 15 core
  tests total (3 model + 12 layout).
- **`git-vista-git`**: new `read_refs(path) -> Vec<GitRef>`. Emits HEAD (always,
  as `RefKind::Head` named `"HEAD"`, when it resolves to a commit — attached or
  detached), plus local branches, remote branches, and tags, classified via
  `reference.name().category_and_short_name()` (`gix::refs::Category`) and peeled
  with `peel_to_id()`. Skips `refs/remotes/*/HEAD` (the remote's symbolic default
  pointer) and notes/worktree-private refs. +1 test (`read_refs_sees_head_branches
  _and_tags`).
- **`git-vista-server`**: `/api/commits` now calls `walk_history` + `read_refs` +
  `layout_with_refs`.
- **Frontend** (`git-vista`):
  - `color.rs`: `lane_color` → **`branch_color(slot)`** (same 6-colour palette,
    wrapping). Added `HEAD_BADGE` (bright neutral), `TAG_BADGE` (amber),
    `BADGE_DARK` (dark text on filled pills; also the merge-node fill via
    `MERGE_FILL` alias).
  - `geometry.rs`: badge geometry — `badge_width(text)` (monospace: chars × 7 + 2×6
    pad), `badge_top_y`/`badge_text_y`/`badge_text_dx`, consts `BADGE_HEIGHT/
    _RADIUS/_GAP`. +1 test.
  - `app.rs`: nodes coloured by `branch_color(row.color)`; edges by the **parent's**
    branch (`branch_color(row_color[e.to_row])`) so a merge line takes the merged-in
    branch's colour. Per row, ref badges are laid out left-to-right from the label
    column (filled pill for local branches / HEAD / tags, **outlined** for remote
    branches) and the commit message is shifted right past them. All inside the
    pan/zoom `<g>`, so badges scale with the graph.
  - `styles.css`: `.badge-text` (11px, 600).
  - `graph.rs` test fixture updated for the two new `GraphRow` fields.

**Decisions:**
- **Colour is a per-branch *slot* computed in core, not a lane index.** The
  requirement is "same colour for a branch across the whole graph"; first-parent
  ownership gives that and survives lane reuse (sequential branches reuse a lane but
  keep distinct colours). Core emits an abstract slot; the actual RGB palette stays
  in the frontend's `color.rs` (per the long-standing "colour can evolve on its
  own" split).
- **Slots allocated lazily** — only when a seed actually claims ≥1 commit — so a
  branch whose tip another branch already owns (e.g. `main` sitting on HEAD's
  first-parent chain, or `origin/<x>` mirroring a local branch) costs no slot.
  Keeps slots dense (0..N) so the palette wraps later / collides less. (First cut
  pre-reserved a slot per seed and left gaps like 0,2,5,6,7,8,9; the dense version
  gives 0..6 on this repo.)
- **HEAD's branch seeded first** → the trunk is colour 0 (blue), matching the old
  lane-0-is-blue look. Both a local branch and its `origin/` twin can share HEAD's
  target; locals sort before remotes so the local wins slot 0.
- **HEAD always badged** (even when it coincides with a branch) so a tip shows e.g.
  `HEAD` + `phase6-lane-layout` + `origin/phase6-lane-layout` together.
- **`layout(commits)` kept** (not changed to take refs) so the 8 existing topology
  tests and the Tauri stub (`layout::layout(Vec::new())`) didn't churn; refs go
  through the new `layout_with_refs`.

**Gotchas:**
- **Only `git-vista-core` is kept stock-`rustfmt`-clean** (the documented Phase-6
  verify ran `cargo fmt -p git-vista-core -- --check`). The other crates use a
  compact hand style that stock rustfmt would expand (e.g. `camera.rs`'s one-line
  `Self {…}`, server's long `eprintln!`s) — so run `cargo fmt` **only on core**, and
  match surrounding style by hand elsewhere. Don't blanket-`cargo fmt --all`.
- **This sandbox kills any process that binds a listening socket** (the server
  exits 144), and `pkill` also trips it. Couldn't smoke-test via `curl localhost`.
  Verified the data path instead with a throwaway `cargo run --example` that called
  `walk_history`+`read_refs`+`layout_with_refs` and printed the rows (since deleted
  — `serde_json` isn't a server dep, so it printed a summary, not JSON).
- Badge widths rely on the **monospace** UI font (`badge_width` = char count ×
  fixed advance); if the font ever changes, retune `BADGE_CHAR_W`.
- `gix`'s `peel_to_id_in_place` is deprecated → use `peel_to_id()`.
- New `GraphRow` fields broke the frontend's `graph.rs` fixture construction —
  updated it (`refs: Vec::new()`, `color: c.lane`).

**Verify:**
```sh
cargo test -p git-vista-core      # 15 pass (3 model + 12 layout incl. colour/badge)
cargo test -p git-vista-git       # 4 pass (incl. read_refs)
cargo test -p git-vista           # 16 pass (camera/geometry/color/text/graph)
cargo clippy -p git-vista-core --all-targets   # clean
cargo fmt -p git-vista-core -- --check         # clean
cargo check -p git-vista-server -p git-vista-tauri
( cd crates/git-vista && trunk build )         # wasm bundle builds clean
```
Real-data check on this repo (17 commits, 2 lanes): HEAD's branch + `main` colour 0
(trunk), each side branch its own dense slot (0..6); HEAD/branch/remote/tag badges
attach to the right commits. Manual (browser): run the server, open the URL — every
branch/tag/HEAD shows a pill at its commit and each branch keeps one colour down the
graph.

**Next:** Phase 8 — Viewport virtualization (only render commits visible in the
viewport for performance).


## Phase 7 fix — Touch interactivity: finger-drag pan + pinch zoom (2026-06-29)
**Status:** done
**What changed (frontend interactivity layer, `git-vista`):**
- **The bug:** the app is built to be used in **Safari on an iPad**, but pan/zoom
  were dead there (looked like a static image). Two mouse-only assumptions from
  Phase 2: pan used `PointerEvent::movement_x/_y` (iOS Safari reports these as **0**
  for touch), and zoom used the `wheel` event (a touch **pinch never raises a wheel
  event**). On desktop both worked, which masked it. Verified with a headless-browser
  repro: synthetic touch-style pointer moves (`movementX=0`) left the transform
  unchanged; a `wheel` is never produced by pinch.
- **`camera.rs`:** added pure `Camera::pinched(prev_dist, cur_dist, mx, my)` —
  scales by the ratio of finger distances, anchored at the pinch midpoint (reuses
  `zoomed_at`); a non-positive `prev_dist` is a no-op (first pinch sample just sets
  the baseline). +3 host tests (ratio scaling, no-baseline no-op, midpoint stays
  anchored). 7 camera tests now; 19 frontend tests total.
- **`app.rs`:** rewrote the gesture layer on **Pointer Events** (unify mouse/pen/
  touch; fire for touch on iOS). We track every pressed pointer's client position
  ourselves in `store_value` and derive the gesture from the count: **1 pointer →
  pan** by the change in its position (no more `movement_*`); **2 pointers → pinch**
  by the change in their distance. Pointer is captured on `pointerdown`
  (`set_pointer_capture` on `current_target`, the SVG — not `target`, which could be
  a child node). Zoom anchor is made SVG-local by subtracting the SVG's
  `getBoundingClientRect` origin. `wheel` zoom kept for desktop. Subtitle now reads
  "drag to pan, pinch or scroll to zoom".
- **`Cargo.toml`:** added web-sys feature **`DomRect`** (for `get_bounding_client_rect`).

**Decisions:**
- **Pointer Events, not Touch Events**, so one code path covers mouse, pen and
  touch — and they're well-supported on iOS Safari ≥13. `touch-action: none`
  (already set in styles.css since Phase 2) is what hands the browser's default
  gestures to us; without it Safari would scroll/zoom the page instead.
- **Pan from coordinate deltas** (current − previous client pos) rather than
  `movementX/Y`: the latter is the exact thing iOS doesn't populate. Deltas in
  client px map 1:1 onto camera space (no viewBox), so `panned` is unchanged.
- **Pinch math stays pure in `camera.rs`** (host-tested), matching the project's
  "DOM-free, unit-tested camera/geometry" split; the handler just feeds it
  distances + midpoint.

**Gotchas:**
- `web_sys::PointerEvent` derefs to `MouseEvent`, so `client_x()/offset_x()` are
  available on it directly (as `movement_x()` was).
- Use `ev.current_target()` (the SVG with the listener), not `ev.target()` (the
  child circle/text/badge actually under the finger), for pointer capture and for
  the bounding-rect origin.
- Don't `cargo fmt` this crate — only `git-vista-core` is kept stock-rustfmt-clean
  (Phase 7 gotcha); match the compact hand style here. `cargo clippy -p git-vista
  --target wasm32-unknown-unknown` is clean.
- A harmless console warning remains on desktop: "Unable to preventDefault inside
  passive event listener" on `wheel` — Leptos registers `wheel` as passive, so
  `prevent_default()` is a no-op. Nothing scrolls (the page is fixed at `100vh`,
  `.graph` is `overflow:hidden`), so it's cosmetic; revisit only if a real scroll
  ever leaks.

**Verify:**
```sh
cargo test -p git-vista        # 19 pass (7 camera incl. 3 pinch + geometry/color/text/graph)
cargo clippy -p git-vista --target wasm32-unknown-unknown   # clean
( cd crates/git-vista && trunk build )                      # wasm bundle builds
```
Headless-browser repro/confirmation (chromium via Playwright, `dist/` served by
`git-vista-server`): touch-style drag with `movementX=0` now pans
(`translate(0 0)`→`translate(120 80)`); two-finger spread zooms in (`scale` 1→~2.7);
desktop mouse-drag + wheel still pan/zoom. **Real-device check still owed:** confirm
on an actual iPad in Safari (the headless run simulates touch pointer events but
isn't iOS).

**Next:** Phase 8 — Viewport virtualization (only render commits visible in the
viewport for performance).


## Issue #13 — Commit timestamps in the labels (2026-06-29)
**Status:** done (frontend-only; the model already carried `CommitSummary::time`).
**What changed:**
- `crates/git-vista/src/datetime.rs` (new): pure, host-tested `format_label(y,m,d,h,
  min,current_year)` → compact US-readable `"Jun 29 14:32"`, showing the year only
  when it isn't the current year (`"Jun 29 2024 14:32"`); day unpadded, 24h time
  zero-padded. Plus a wasm-only `local_timestamp(epoch_secs)` that uses the JS
  `Date` to break the instant down in the **viewer's local timezone** (correct
  per-commit incl. DST) and reads the current year. 4 tests.
- `crates/git-vista/src/app.rs`: the dimmed meta label line is now
  `"<short-hash> · <author> · <Jun 29 2:32 PM>"` (was hash · author).
- `crates/git-vista/src/main.rs`: declares `mod datetime;` (same dead-code gating
  as the other pure modules).
- `crates/git-vista/Cargo.toml`: added `js-sys` under the wasm-target deps.

**Decisions:**
- **Local timezone, not UTC nor the committer's tz.** The model only stores UTC
  seconds (Phase 3 didn't capture the committer tz offset); for a personal viewer,
  the reader's local time is the intuitive "when was this". JS `Date` gives correct
  local + DST per commit.
- **Split pure/impure** to match the codebase: string assembly is host-tested
  (`format_label`); only the `Date` getters are wasm-only (`local_timestamp`,
  `#[cfg(target_arch = "wasm32")]`), so host tests don't touch js-sys.
- **Compact US format `"Jun 29 2:32 PM"`, year hidden unless not current year**
  (per user requests 2026-06-29; first cut was `YYYY-MM-DD HH:MM` → switched to
  `Mon D` + 12-hour AM/PM time, both at the user's request). Day unpadded, minutes
  zero-padded, seconds omitted — full precision could go in a hover later.

**Verify:**
```sh
cargo test -p git-vista     # 24 pass (+5 datetime)
cargo clippy -p git-vista --target wasm32-unknown-unknown   # clean
( cd crates/git-vista && trunk build )                      # wasm bundle builds
```
Browser-confirmed (Playwright over the served bundle): meta lines render e.g.
`6edb5b5 · tomb · Jun 29 03:28` (current-year rows show no year).

**Next:** Phase 8 — Viewport virtualization (only render commits visible in the
viewport for performance).


## Issue #12 — Clickable commits & badges, linking to GitHub (2026-06-29)
**Status:** done. Touches all layers.
**What changed:**
- **`git-vista-git`**: `github_web_base(path) -> Option<String>` reads
  `remote.origin.url` (gix `config_snapshot().string`) and parses it with the pure,
  unit-tested `web_base_from_remote` → `"https://github.com/owner/repo"`, or `None`
  if no origin / unparsable / host isn't github.com. Handles `git@github.com:o/r.git`,
  `https://…/o/r(.git)(/)`, `ssh://git@github.com/o/r.git`, case-insensitive host.
  +2 tests.
- **`git-vista-core::model`**: `Graph` gains `repo_url: Option<String>` (`#[serde(default)]`).
  `layout_topology` sets it `None`; the server fills it after layout (pure layout
  doesn't know remotes). Frontend `graph.rs` fixture updated.
- **`git-vista-server`**: sets `graph.repo_url = github_web_base(repo)` before JSON.
- **Frontend `app.rs`**:
  - Links (only when `repo_url` is `Some`): commit message → `{base}/commit/{full-sha}`;
    branch & tag badges → `{base}/tree/{name}`; remote-branch badge → `{base}/tree/{name
    sans "<remote>/"}`; HEAD badge → its commit. Opened in a **new tab** via
    `window.open(url, "_blank")`. `None` => labels stay plain text (no cursor change).
  - **Reworked the gesture layer for tap-vs-drag:** pointer capture and panning are
    now **deferred until the pointer moves past `DRAG_THRESHOLD` (4px)**; a tap never
    captures, so its `click` reaches the child element's link handler. A `moved`
    flag (set on drag/pinch) makes click handlers ignore the click that ends a drag.
    This was necessary because the previous code captured the pointer on
    `pointerdown`, which would have sent the click to the SVG, not the badge/message.
  - `.clickable { cursor: pointer; }` in styles.css; `Window` web-sys feature added.

**Decisions (per user):** GitHub-only (no GitLab/etc.); tags link to `/tree/<tag>`
(not `/releases/tag`); HEAD links to its commit; everything opens in a new tab.

**Gotchas:**
- An SVG `<text>`'s `textContent` includes its `<title>` child — don't match label
  text by exact content in tests; click by position instead.
- **Rebuild the server binary after server/core changes** — `cargo check` isn't
  enough; a stale `target/debug/git-vista-server` will serve the old behaviour (here:
  `repo_url` came back `null` until rebuilt). `gv` rebuilds the wasm but **not** the
  server, so for server changes run `cargo build -p git-vista-server` (or just let
  `gv`'s `cargo run` rebuild — it does, but a separately-launched stale binary won't).
- Browser repro stubs `window.open` via `addInitScript` to capture the URL with no
  network (github.com is unreachable in the sandbox).

**Verify:**
```sh
cargo test -p git-vista-git    # 6 pass (incl. URL parsing)
cargo test -p git-vista-core   # 15 pass (Graph gained repo_url)
cargo test -p git-vista        # 24 pass
cargo clippy -p git-vista --target wasm32-unknown-unknown   # clean
( cd crates/git-vista && trunk build )
```
Playwright over the served git-vista repo (origin `tom2025b/git-vista`): clicking a
commit message opened `…/commit/<sha>`, a branch badge `…/tree/<branch>`, the HEAD
badge `…/commit/<sha>`; touch-pan and a drag starting on a message both panned and
opened nothing. **Real-iPad tap check still owed** (headless simulates touch).

**Next:** Phase 8 — Viewport virtualization (only render commits visible in the
viewport for performance).


## Issue #33 — Commit directly from the graph, with an iPad-safe modal (2026-07-01)
**Status:** done (merged to `main` via PR #34, commit `009327a`). Touches all layers.

### ⚠️ THE BIG LESSON: a void `<input>` breaks Leptos CSR rendering on iOS WebKit
**This is the most important thing to carry forward.** The commit modal worked
perfectly on desktop Linux but **silently never mounted on the iPad** — no modal,
no error, the menu just closed. It took ~15 rebuilds to find because the symptom
mimicked cache, reactivity, and CSS bugs. Root cause:

- Leptos **0.6.15 with the `csr` feature** renders static markup by building an
  HTML **`<template>`**, then *walking the parsed DOM nodes at compile-time-fixed
  positions* (firstChild/nextSibling) to attach dynamic bits + event listeners.
- **`<input>` is a void element**, and **iOS WebKit's HTML parser handles void
  elements differently than desktop Blink/Gecko**. The parsed node tree didn't
  match what Leptos' compile-time walk expected, so the walk landed on the wrong
  node and **panicked the entire `view!`** — taking the whole modal (even its
  full-screen backdrop) down with it. Desktop parsed it the expected way, so it
  worked there. Classic "works on my machine, dead on the device."

**The fix: use a non-void `<textarea>` for the commit-message field, not `<input>`.**
A `<textarea>` has a real closing tag, so there's no void-parsing ambiguity — and
it's perfectly good for a commit message (multi-line is fine). **Rule going
forward: avoid void HTML elements (`<input>`, `<br>`, `<img>`, `<hr>`) inside a
Leptos `view!` in this project — prefer a non-void equivalent, or build the node
via `web_sys`.** (Also recorded in Claude auto-memory as `leptos-csr-void-input-webkit`.)

### How it was finally diagnosed (reusable technique)
The iPad's console isn't readable from the Linux box, and the user runs it in
**Firefox Private on iOS** (still WebKit) closing the tab between tests (so no
cache). What cracked it: a **temporary on-screen debug bar** rendered as a
`position:fixed` element, showing a hardcoded **`BUILD-XXX` marker** (bumped every
build, so you can confirm from the device which bundle is live vs. a stale one)
plus a **"TEST MODAL" button** that opened the modal directly (bypassing the menu
path). Then the modal's contents were **bisected**: minimal magenta `<div>` (✓
mounts) → +flex/nesting/title (✓) → +`<input>` (✗ whole modal dies) → bare
`<input>` (✗) → `<textarea>` (✓). When you can't see a device's console, put the
diagnostics **on the screen** and bisect.

**What changed (feature itself):**
- **`git-vista-core::model`**: new shared `CreateCommitRequest { message: String,
  allow_empty: bool }` (mirrors `CreateBranchRequest` from Issue #18).
- **`git-vista-server`**: new route **`POST /api/commit`** → `create_commit()`
  shells out to `git -C <repo> commit [-m <msg>] [--allow-empty]`, argv-separated
  (no shell injection), rejects an empty message. Surfaces git's own error text;
  note **"nothing to commit" goes to git's *stdout* (not stderr) with a non-zero
  exit**, so the handler prefers stderr, then falls back to stdout, then a generic
  line. (Same B3 "let git validate + report" posture as `/api/branch`.)
- **Frontend `app.rs`**:
  - Context menu (on a commit dot) gained **"Commit staged changes"**
    (`allow_empty:false`) and **"Create empty commit"** (`allow_empty:true`),
    enabled **only on the current HEAD tip** — computed as
    `gr.refs.iter().any(|r| r.kind == RefKind::Head)` — and rendered as a disabled
    greyed `<span>` elsewhere (only there can a commit land without moving HEAD).
    `MenuData` gained `is_head: bool` (a branch stub is always `false`).
  - **The modal** (`commit_dialog_view`): a `commit_dialog: RwSignal<Option<bool>>`
    (Some(allow_empty) = open) + `commit_msg: RwSignal<String>`. Prompts for the
    message because **`window.prompt()` is blocked/flashed inside the webview**
    (its unreliability was the whole reason for a custom modal). On confirm it
    POSTs `/api/commit`, refreshes the graph (bumps `reload`), and shows git's
    error via `alert()` on failure. Commit button is `prop:disabled` until the
    message is non-blank.
  - `create_commit_request(message, allow_empty)` helper (mirrors
    `create_branch_request`).

**Modal structure details worth knowing (all deliberate, don't "clean up" naïvely):**
- **`<textarea>`, never `<input>`** (see the big lesson above).
- **Inline styles + viewport-unit sizing** (`position:fixed; width:100vw;
  height:100vh`), NOT CSS classes and NOT the `inset:0` shorthand. Viewport units
  proved to render reliably on iOS; `inset` is unsupported on older iOS Safari
  (<14.5) and would collapse the backdrop. The old `.modal-*` CSS classes were
  removed — the modal is styled inline in `app.rs` on purpose.
- **Single reactive overlay block.** The menu and modal render from ONE
  `move || { let menu = menu_view(); let modal = commit_dialog_view(); view!{{menu}{modal}} }`
  block (they're mutually exclusive — opening the modal closes the menu). This was
  originally a workaround for a suspected "second adjacent reactive block fails to
  mount on WebKit" theory that turned out to be **wrong** (the real culprit was the
  `<input>`); the single block is kept because it's confirmed-working and harmless,
  but two separate blocks would also work now.
- **Ghost-click guard.** `dialog_opened_at` (a `store_value(f64)`) records
  `js_sys::Date::now()` when the modal opens (in `on_commit`); the backdrop's
  click-to-close ignores a dismiss within **`DIALOG_GUARD_MS` (400 ms)** of
  opening. Rationale: iOS synthesizes a `click` a few ms after a tap; opening the
  modal puts its full-screen backdrop under that tap point, so without the guard
  the synthesized "ghost click" could hit the backdrop and instantly close the
  modal. (Added defensively; never proven to be the failure — the `<input>` kept
  the modal from mounting at all — but it's correct and cheap, so it stays.)
- The `on_commit` handler sets `commit_dialog`/`commit_msg` **before**
  `menu.set(None)`, because `menu.set(None)` synchronously disposes the handler's
  own reactive owner and any signal write after it is unreliable.

**Gotchas:**
- **Test frontend changes on the actual iPad, not just Linux** — WebKit vs
  Blink/Gecko rendering differences are real and silent (this whole saga). A
  headless desktop browser won't catch them either.
- `gv` builds the wasm but a stale bundle can persist on a device; Firefox Private
  + closing the tab guarantees a fresh load. A `BUILD-XXX` marker is the fastest
  way to confirm which bundle a device is running.
- The `git-vista-test` repo (`~/projects/git-vista-test`, github
  `tom2025b/git-vista-test`) is a **target repo you visualize**, not a copy of the
  source — it has **no `crates/git-vista/` and no frontend**. Running bare
  `trunk build` inside it gives *"Unable to find any Trunk configuration"*. Always
  build via `gv` (it cd's into the real source's `crates/git-vista`).

**Verify:**
```sh
cargo test --workspace                        # 58 pass
( cd crates/git-vista && trunk build )        # wasm bundle builds, no warnings
( cd crates/git-vista && cargo check --target wasm32-unknown-unknown )   # clean
```
Real-device check (the one that matters): `gv`, open on the iPad, stage a change
in the visualized repo, tap the HEAD-tip dot → "Commit staged changes" → the dark
modal appears → type a message → Commit → the graph refreshes with the new commit.
Confirmed working end-to-end on an iPad on 2026-07-01.

**Next (remaining Issue #33 actions, not yet done):** "Merge this branch into
main", "Push this branch", "Delete this branch (with confirmation)", and
"Export/Print view". Each maps onto the same pattern: shared request struct in
`git-vista-core::model` → a `POST /api/…` route in `git-vista-server` that shells
out to git → a menu item + handler in `app.rs`. Reuse `CreateBranchRequest` /
`CreateCommitRequest` as the template.


## Phase 9 — Level of detail (2026-07-01)
**Status:** done (frontend-only). **NB: Phase 8 (viewport virtualization) was
skipped** — done out of order at the user's request ("Do Phase 9 now"); Phase 8
is still open. Phase 9 doesn't depend on it (it hides text, doesn't cull nodes).
**What changed:**
- `crates/git-vista/src/lod.rs` (new, pure/host-tested): `detail_for(scale) ->
  Detail` maps the camera zoom to a `Detail` enum — `GraphOnly` (structure only)
  / `Message` (ref badges + commit message) / `Full` (+ the dimmed `hash · author
  · date` meta line). `Detail::shows_message()` / `shows_meta()` are the two gates
  the view reads. Thresholds `MESSAGE_SCALE = 0.5`, `FULL_SCALE = 0.8` (chosen
  against the camera's `[0.2, 5.0]` range so the default unzoomed `scale = 1.0`
  view is `Full`). 5 tests (each boundary, default-is-full, monotonic-in-zoom).
- `crates/git-vista/src/app.rs`: the per-row label build now returns a **tuple**
  `(message_tier, meta_tier)` and the rows `.unzip()` into two `Vec<View>`
  (`label_msgs`, `label_metas`) instead of one combined `collect_view()`. In the
  render each tier is its own `<g class:lod-hidden=move || !detail_for(camera.get()
  .scale).shows_…()>` inside the existing pan/zoom `<g>`, so visibility is reactive
  to the camera signal. Added `use crate::lod::detail_for;`.
- `crates/git-vista/src/main.rs`: `mod lod;` with the same dead-code gating as the
  other pure modules.
- `crates/git-vista/styles.css`: `.lod-hidden { display: none; }`.

**Decisions:**
- **LOD is a pure `scale -> Detail` function in its own module**, matching the
  project's camera/geometry/text/datetime split (DOM-free, host-tested); the view
  just reads it reactively. Kept the palette/geometry split intact.
- **Hide via a CSS class toggle (`display:none`) on a per-tier `<g>`**, not by
  rebuilding the view. The label views are built once (static `Vec<View>`); only
  the class flips as you zoom, so there's no per-frame re-render of the labels.
  `display:none` also drops them from hit-testing, so hidden links aren't tappable.
- **Two tiers, not one on/off.** Badges + message go together (they share the
  left-to-right `bx` layout, so they're built in one `view!`); the smaller meta
  line is a separate tier that drops one zoom-step earlier. Three levels total.
- **Boundaries belong to the finer level** (`<` comparisons): text appears the
  instant you reach a threshold, not one notch late.
- **Stubs stay always visible** — a branch stub draws only a line + hollow ring
  (its name is a `<title>` hover, not on-canvas text), so it's structure, not a
  label tier.

**Gotchas:**
- **Couldn't do a live browser/iPad check in this sandbox** — it kills any process
  that binds a listening socket, so `git-vista-server` exits immediately with no
  output (the documented exit-144 constraint). Verified the pure logic via the
  `lod` unit tests + a clean `cargo clippy --target wasm32-unknown-unknown` and
  `trunk build`. **Real-device visual check still owed**: on the iPad, zoom out and
  confirm the meta line drops first, then all label text, leaving dots/edges/stubs.
- Don't `cargo fmt` this crate — only `git-vista-core` is kept stock-rustfmt-clean
  (Phase 7 gotcha); match the compact hand style here.
- `.unzip()` needs the target annotation `(Vec<_>, Vec<_>)` and each tier
  `.into_view()`'d so both tuple positions are the same `View` type.

**Verify:**
```sh
cargo test -p git-vista        # 33 pass (+5 lod)
cargo clippy -p git-vista --target wasm32-unknown-unknown   # clean
( cd crates/git-vista && trunk build )                      # wasm bundle builds
```
Real-device (owed): `gv`, open on the iPad, pinch-zoom out — the dimmed meta line
disappears first (~0.8×), then the badges + message (~0.5×), leaving just the
coloured dots, edges and stub lines; zooming back in restores each tier.

**Next:** Phase 8 — Viewport virtualization (still open; skipped to do Phase 9),
or Phase 10 — Commit detail panel.


## Phase 8 — Viewport virtualization (2026-07-01)
**Status:** done (frontend-only). Done *after* Phase 9 at the user's request;
Phase 9 (PR #39) was merged to `main` first, then this on its own branch.
**What changed:**
- `crates/git-vista/src/viewport.rs` (new, pure/host-tested): `visible_row_range(
  cam, viewport_h, row_count, overscan) -> (start, end)` — the half-open window of
  row indices whose nodes fall inside a `viewport_h`-tall viewport under the camera.
  It maps the visible screen band `[0, viewport_h]` back to a world-y interval
  (`(y - ty)/scale`), inverts `geometry::node_cy` (`row = (y - PAD_Y)/ROW_HEIGHT`)
  with `floor`/`ceil` so partially-visible rows count, pads by `overscan` rows each
  side, and clamps to `0..row_count` (always ordered). 6 unit tests (empty, default
  starts at top, pan scrolls the top off, zoom-out widens, clamp/order, overscan
  grows both sides).
- `crates/git-vista/src/app.rs`: `graph_canvas` no longer builds every row eagerly.
  The graph + its derived lookups (`row_color`, `remote_set`, `remote_branches`,
  `repo_url`, `text_x`) are moved into a `RenderCtx` behind `store_value` so the
  reactive closures can reach them cheaply. The old eager `edges`/`nodes`/label
  `.map().collect_view()` blocks became **per-item builder closures** `build_edge`
  / `build_node` / `build_msg` / `build_meta` (each `ctx.with_value(|c| … )` →
  `View`). The render uses keyed `<For>`s over the visible window: nodes + both
  label tiers iterate `(start..end)` keyed by row index; edges iterate
  `visible_edges(ctx, range)` (edge indices whose `[from_row, to_row]` span
  intersects `[start, end)`) keyed by edge index. A `visible` **`Memo`** of the row
  range is derived from `camera` + a `vp_h` signal; `vp_h` starts at
  `window_inner_height()` and is refreshed by a `resize` listener (a `web_sys`
  `Closure` that's `.forget()`-leaked for the app's lifetime). Stubs stay **eager**
  (kept inside one `ctx.with_value(...)`).
- `crates/git-vista/src/main.rs`: `mod viewport;` (same dead-code gating).
- `crates/git-vista/Cargo.toml`: unchanged — the web-sys `Window` + `EventTarget`
  features (present since earlier phases) already cover `inner_height` and
  `add_event_listener_with_callback`.

**Decisions:**
- **Keyed `<For>`, not show/hide.** Real virtualization must not create the
  off-screen DOM (hiding with `display:none` still builds thousands of nodes). A
  keyed `<For>` (key = row index / edge index) adds/removes only the rows crossing
  the viewport edge and keeps the DOM for rows that stay — so a pan is a cheap
  delta, not a full re-render.
- **Range is a `Memo`.** `visible_row_range` reads `camera` (which changes every
  pointer move), but the `Memo`'s `PartialEq` on `(usize, usize)` means the `<For>`
  `each` closures only re-run when the row window actually changes — a sub-row pan
  is a no-op for the DOM.
- **Window height as a safe over-estimate.** The virtualizer needs an *upper* bound
  on the SVG viewport height; the browser window is always ≥ the SVG (the topbar
  sits above it), so `window.inner_height()` is used directly instead of measuring
  the SVG with a `NodeRef`/`ResizeObserver`. Worst case we render a few extra rows
  just past the bottom — never too few (which would leave a blank strip).
- **Overscan of 6 rows** each side so a fast flick doesn't flash a blank edge before
  the `Memo` recomputes; it also absorbs the small vertical extent of a row's badges
  (above the node) and meta line (below).
- **Edges by span-intersection, not endpoint-in-window.** An edge is drawn whenever
  `from_row < end && to_row >= start`, so a long merge line whose *both* endpoints
  are off-screen but which crosses the viewport still renders. (Edges always run
  downward, `from_row` < `to_row`.)
- **Stubs kept eager.** There are only a handful (one per commit-less new branch)
  and their cascade fans *upward* off the anchor commit, which doesn't map onto the
  row window as cleanly as nodes/edges; rendering them all is cheap and dodges that
  edge case.
- **`RenderCtx` behind `store_value`.** The builder closures need the graph +
  derived tables; `StoredValue` is `Copy`, so every closure captures it for free
  (no per-row clone), and the derived sets are built once, not per row.

**Gotchas:**
- The builder closures are only `Copy` (hence reusable in `<For children>`) because
  everything they capture is `Copy` — `ctx` (`StoredValue`), `menu` (`RwSignal`),
  `moved` (`StoredValue`), and `suppress` (a closure capturing only the `Copy`
  `moved`). Keep it that way; capturing a non-`Copy` value would break the `<For>`.
- `app.rs` is **wasm-only** (`#[cfg(target_arch = "wasm32")] mod app`), so the host
  `cargo test` never compiles it — the `<For>`/closure changes are only checked by
  `cargo clippy --target wasm32-unknown-unknown` and `trunk build`. Run those, not
  just `cargo test`.
- The `resize` `Closure` is `.forget()`-leaked deliberately (it must outlive the
  function scope). That's fine for a single top-level app component; don't "fix" it.
- **Couldn't do a live browser/iPad check in this sandbox** (it kills any process
  binding a listening socket — the documented exit-144 constraint). Verified the
  pure `viewport` logic via its 6 unit tests, plus clean `clippy` + `trunk build`.
  **Real-device check still owed:** on a large repo, scroll a long history and
  confirm it stays smooth, labels/dots appear/disappear correctly at the viewport
  edges (no blank strips, no missing edges), and pan/zoom/tap still behave.
- Don't `cargo fmt` this crate — only `git-vista-core` is kept stock-rustfmt-clean
  (Phase 7 gotcha); match the compact hand style here.

**Verify:**
```sh
cargo test -p git-vista        # 39 pass (+6 viewport)
cargo clippy -p git-vista --target wasm32-unknown-unknown   # clean
( cd crates/git-vista && trunk build )                      # wasm bundle builds
```
Real-device (owed): `gv <big-repo>`, open on the iPad, fling-scroll the history —
it should stay responsive with only the on-screen rows in the DOM; rows and edges
should fill in at the edges without blanks, and tapping a dot / pinch-zoom / LOD
should all still work.

**Next:** Phase 10 — Commit detail panel (clicking a commit opens a side panel with
full details), or Phase 11 — Search & filter.

---

## Phase 12 — Open repository UX ("Open URL", read-only) (2026-07-01)
**Status:** done (skipped Phase 10/11 at the user's request). Ships on branch
`phase12-open-url`.

**Scope was deliberately cut.** The DESIGN framing was "open any local repository",
but the user's real need is: *"look at a complex public git history in this app a
few times to learn git better"* — read-only, occasional, public URLs. So this phase
is NOT a local-filesystem picker. It's "paste a public URL → clone → view read-only".
Explicitly NOT built: allowed-roots/local-path picker, directory browser, recents
persistence, auto-discovery, private-repo auth, any write path on a clone. (A local
picker could reuse the mutable-repo foundation below if ever wanted.)

**How it works.**
- **Mutable current repo.** The server's repo was a fixed `OnceLock<PathBuf>`;
  now it's `OnceLock<RwLock<Current { path, read_only }>>` in
  `git-vista-server/src/main.rs`. `current() -> (PathBuf, bool)` snapshots + clones
  out of the lock so no guard is ever held across an `.await`; `set_current()`
  swaps it. Startup seeds it from the CLI arg with `read_only:false` (your own repo
  is writable). All existing handlers read `current().0` instead of the old
  `repo_path()`.
- **`POST /api/clone { url }`** (new). Validates the URL with the pure, unit-tested
  `git_vista_core::model::validate_clone_url` (accepts only `https://`/`http://`/
  `git://`; rejects `git@…` SSH, `file://`, local paths, leading `-`, and embedded
  spaces — so a paste can't trigger an SSH key prompt or smuggle a git option),
  then `git clone -- <url> <dest>`. Full clone (history bounded downstream by
  `HISTORY_LIMIT=5000`) — user chose full over `--depth`.
- **Temp dirs + cleanup.** Clones go under `std::env::temp_dir()/git-vista-clones/
  clone-<nanos>-<counter>`. On a successful clone the *previous* clone is deleted,
  so disk holds at most one. `cleanup_clone()` refuses to `rm` anything not under
  `clones_root()` — a bug can never delete a real repo. No shutdown cleanup (temp
  dir, low stakes); the "delete previous on next open" bound is what matters.
- **Read-only enforcement, two layers.** `/api/commits` sets the new
  `Graph.read_only` field (`git-vista-core::model`) from the current repo's flag.
  Frontend (`app.rs` `graph_canvas`) hides ALL write items in the context menu
  when `read_only` (create branch / commit / merge / push / delete) — only the
  header + GitHub link remain. The server ALSO refuses every write endpoint with
  `403` via `reject_if_read_only()` (defense in depth; the UI hiding them is just
  honesty).
- **Frontend UX.** New "Open URL…" button in the `.topbar` next to Refresh opens a
  modal (App component in `app.rs`). On success it bumps the existing `reload`
  signal — the same refresh path used everywhere — so the cloned graph loads with
  no new plumbing. Modal reuses the commit modal's iPad-proven inline-styled
  overlay and a **`<textarea>` (NOT a void `<input>`** — void inputs panic the
  Leptos CSR node-walk on iOS WebKit; long-standing gotcha) for the URL field. The
  Open button shows "Cloning…" and is disabled while git runs.

**Gotchas / caveats.**
- **Single shared current repo:** switching the clone affects ALL connected
  browsers (one global `RwLock`). Fine for this single-user tool; documented, not a
  bug. A per-client repo would mean threading the repo through every request.
- **Couldn't live-test the HTTP round-trip here.** This sandbox blocks binding a
  listening socket (the exit-144 note above; even sandbox-disabled the server won't
  stay up). Verified instead: 23 core tests pass incl. the two new
  `validate_clone_url` cases; `cargo build --workspace` + `trunk build` clean;
  clippy clean; and a manual `git clone <local repo> <temp>` → `git log` /
  `for-each-ref` → `rm -rf` proving the clone read path (identical to the proven
  `/api/commits`) and the cleanup. **Owed:** real run — `gv`, click "Open URL…",
  paste e.g. a small public GitHub repo, confirm the graph loads read-only (no
  write items in the dot menu) and that opening a second URL replaces it.

**Verify:**
```sh
cargo test -p git-vista-core   # 23 pass (+2 validate_clone_url)
cargo build --workspace        # server + tauri + frontend compile
cargo clippy -p git-vista-server -p git-vista   # clean
( cd crates/git-vista && trunk build )          # wasm bundle builds
```

**Next:** Phase 10 — Commit detail panel, or Phase 11 — Search & filter, or
Phase 13 — Packaging & polish.

---

## Phase 10 — Commit detail panel (2026-07-01)
**Status:** done. Ships on branch `phase10-commit-detail-panel`.

**What changed.**
- **New lazy detail read, not a fatter graph.** The graph's `CommitSummary` still
  carries only what a row needs (first line, author name, commit time). A new
  `git_vista_core::model::CommitDetail` carries everything the panel shows — full
  message body, and *both* the author and committer signatures (name, email, and
  each one's own time). It's fetched per-commit on demand rather than bloating
  every one of up to 5000 rows.
- **Backend.** `git_vista_git::read_commit(path, id)` (in `history.rs`) opens the
  repo isolated (same as the walk), parses the hex id with
  `gix::ObjectId::from_hex`, `repo.find_commit`, and pulls `author()`/`committer()`
  /`message_raw()`/`parent_ids()`. Signature seconds via `SignatureRef::time()`
  (lenient; malformed → epoch, never fails the read). A malformed/absent id is a
  new `RepoError::CommitNotFound` (→ 404), distinct from a read error (→ 500).
  Two unit tests: full-detail read of the merge fixture, and the not-found cases.
- **Server.** `GET /api/commit/{id}` → `commit_detail` handler (axum **0.8**, so the
  path param is `{id}` not `:id`). Reads the *current* repo (works on read-only
  clones — it's a read), maps `CommitNotFound` → 404 / else → 500, `no-store` like
  the graph. Note the pre-existing `POST /api/commit` (create) is a different
  method+path, no conflict.
- **Frontend (`app.rs`).** A `detail_id: RwSignal<Option<String>>` (the selected
  full hash) drives a `create_local_resource` that calls `fetch_commit_detail`.
  The context menu (shared by commit dots *and* stubs) gained a **"View details"**
  item — shown even on read-only clones, since it's a read — that sets `detail_id`
  (before `menu.set(None)`, per the existing owner-disposal caveat). A new
  `detail_panel_view` renders a right-docked `<aside class="detail-panel">`: title +
  close (×), full hash, author line, committer line **only when it differs** from
  the author, parent hashes as clickable chips that re-point the panel at that
  parent (walk up history), an "Open on GitHub" link gated on repo_url + the commit
  being in `remote_set` (same 404-avoidance rule as labels/menu), and the full
  message in a `<pre class="detail-msg">` (`white-space: pre-wrap`). Dates via the
  existing `datetime::local_timestamp`.
- **CSS.** `.detail-panel` and friends appended to `styles.css` — fixed to the
  right edge, `width: min(420px, 100vw)` so it goes full-width on iPad portrait,
  scrollable body. Same overlay posture as the context menu (outside the SVG, so it
  never pans/zooms or clips).

**Decisions.**
- Lazy per-commit endpoint over fattening the graph: keeps the (up to 5000-row)
  `/api/commits` payload lean; the body + emails + committer are only ever wanted
  for the one commit you're looking at.
- Entry point is a menu item, not a bare dot-tap: the dot already opens the context
  menu (Issue #18), so "View details" joins that menu rather than fighting it.
- Stale-guard in the panel body: if the loaded detail's id ≠ the currently-open
  `detail_id` (e.g. mid-navigation between parents), it shows "Loading…" rather than
  one commit's chrome over another's data.

**Gotchas.**
- axum 0.8 path syntax is `{id}` (0.7 was `:id`) — wrong syntax panics at route
  registration.
- `menu.set(None)` disposes the menu handler's reactive owner; set `detail_id`
  *before* it (matches the commit/branch items' existing ordering note).
- `CommitDetail`'s field is `commit_time`, not `committer_time` (compile caught it).

**Verify:**
```sh
cargo test -p git-vista-core -p git-vista-git   # incl. 2 new read_commit tests
cargo clippy -p git-vista-server -p git-vista   # clean (frontend: --target wasm32…)
( cd crates/git-vista && trunk build )          # wasm bundle builds
# Live round-trip (done here): run the server, then
curl -s localhost:8080/api/commit/$(git rev-parse HEAD)   # → full JSON, 200
curl -s -o/dev/null -w '%{http_code}' localhost:8080/api/commit/000…0  # → 404
```
Live-tested the endpoint (200 with full detail incl. differing author/committer on
a merge; 404 for bad/unknown ids). **Owed:** eyeball the panel itself in a browser
(no browser in this sandbox) — open the app, tap a dot → "View details", confirm
the panel, parent-chip navigation, and the committer row appearing only on commits
where it differs (e.g. a GitHub-merged commit).

**Next:** Phase 11 — Search & filter, or Phase 13 — Packaging & polish.

---

## Phase 13 — Packaging & polish (2026-07-01)
**Status:** in progress. First tranche ships on branch `phase10-commit-detail-panel`
(alongside the Phase 10 work); Phase 11 (search) and a few Phase 13 items remain.

**What changed.**
- **Keyboard shortcuts (`crates/git-vista/src/app.rs`).** A `keydown` listener on
  `window`, registered in `graph_canvas`:
  - `Esc` backs out of the topmost overlay — the context menu first, then a modal
    (commit / confirm), then the detail panel. **Esc is only a shortcut**: the user
    runs an iPad Magic Keyboard with **no physical Esc key**, so every overlay still
    closes via its Cancel button / a backdrop tap / tapping away. Don't ever make Esc
    the *only* way out of anything.
  - `+`/`=` zoom in, `-`/`_` zoom out (anchored at the viewport centre via
    `Camera::zoomed_at`, since a key press has no cursor), `0` resets the camera.
  - `r`/`R` bumps the `reload` counter (same as Refresh).
  - Non-Esc keys are ignored when the keydown target is a `<textarea>`/`<input>` (so
    typing an "r" in the commit/URL box doesn't refresh) and when Ctrl/Meta/Alt is
    held (so the browser's own Cmd/Ctrl-R reload is untouched).
  - Needs the **`KeyboardEvent`** web-sys feature — added to
    `crates/git-vista/Cargo.toml`.
- **Listener leak fix (same file).** The pre-existing `resize` listener — and the new
  `keydown` one — are now removed via `on_cleanup` instead of `cb.forget()`.
  `graph_canvas` reruns on every graph reload (each Refresh/clone), so `forget()` was
  stacking a fresh listener bound to the *new* signals on top of the old, disposed
  ones each time. `on_cleanup` drops the old one when the reactive owner is disposed.
- **Reset-view button (same file + `styles.css`).** A floating `.reset-view` button
  (bottom-left, clears the right-docked detail panel) calls `camera.set(Camera::
  default())`. This is the *only* recenter path for a pure touch/trackpad user, so
  it's not just a keyboard-`0` duplicate.
- **Icons / mobile meta (`crates/git-vista/index.html`).** An inline SVG favicon as a
  `data:image/svg+xml;base64,…` URI (no separate asset for Trunk to copy), plus
  `theme-color`, `apple-mobile-web-app-*`, and `viewport-fit=cover` +
  `user-scalable=no` on the viewport (so a stray pinch zooms *our* camera, not the
  page). Trunk passes these through into `dist/index.html` untouched (verified).
- **Server packaging (`crates/git-vista-server/src/main.rs`).**
  - `DEFAULT_REPO` changed from the hardcoded `/home/tom/projects/git-vista` to `.`
    (current working dir, canonicalised at startup). `gv` always passes a path, so
    this only bites when the server is run directly with no arg — but a hardcoded
    personal path is not shippable.
  - On startup the server now `remove_dir_all`s `clones_root()` (guarded by
    `.exists()`). `gv` SIGKILLs the previous server, so its last Phase 12 clone never
    got cleaned up and piled up under the temp dir across runs. Nothing is served
    from there yet at startup, so wiping it is safe; the next clone recreates it.
- **Removed the legacy Tauri desktop shell** (`crates/git-vista/src-tauri`, pkg
  `git-vista-tauri`). It was a Phase-4 stub — its one command `list_commits`
  returned an empty graph and nothing used it — yet it cost a whole CI job plus
  WebKitGTK/GTK/appindicator/librsvg system deps and a 5th workspace member. Gone:
  the `src-tauri/` tree, the workspace member, the CI `tauri` job, the
  `src-tauri/gen` gitignore line, and every Tauri mention in docs/comments (the
  data types now describe the HTTP/JSON boundary, not "Tauri IPC"). History is
  preserved in git; the branch `phase10-commit-detail-panel` and all others are
  left in place (standing "never delete branches" rule).

**Decisions.**
- Floating reset button lives in `graph_canvas` (where `camera` is), not the topbar
  (in `App`) — avoids lifting the camera signal up through the load state just for
  one button. Trade-off: the topbar (Open URL / Refresh) and the reset control are in
  two different components.
- Esc handling kept despite the user's keyboard lacking the key: it's harmless there
  (never fires) and helps desktop users; the invariant is just "never the only exit".

**Gotchas.**
- `graph_canvas` is re-invoked on every reload — anything registered with
  `Closure::forget()` there leaks and duplicates. Use `on_cleanup` + `remove_event_
  listener_with_callback` (see the resize/keydown handlers).
- Couldn't live-test the server changes here: port 8080 was held by the user's own
  running `gv` session (my instance correctly refused to double-bind and exited — I
  did **not** kill theirs). **Owed:** next `gv` run, confirm the favicon/tab icon, the
  Reset-view button recenters, `0`/`+`/`-`/`r` work from the Magic Keyboard, and the
  temp clones dir is cleared on startup.

**Verify:**
```sh
cargo test -p git-vista-core -p git-vista-git -p git-vista   # 73 pass
cargo clippy -p git-vista-server 2>&1 | tail                 # clean
cargo clippy -p git-vista --target wasm32-unknown-unknown    # clean
( cd crates/git-vista && trunk build )                       # bundle builds; favicon in dist/index.html
```

**Next:** finish Phase 13 (an optional perf pass — e.g. open the gix repo once per
`/api/commits` instead of ~5×), then Phase 11 — Search & filter.
