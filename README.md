# Git-Vista

Git-Vista is becoming a professional, touch-first visual Git client for one
developer using an iPad to work with real repositories on Linux. The browser is
the portable UI platform; Rust, Axum, `gix`, System Git, Leptos, and WebAssembly
provide the native repository service and adaptive client.

The current implementation is a working prototype centered on a clean, zoomable
vertical commit graph. The proposed V2 direction adds a safe daily-driver Git
workflow, SSH-first remote access, worktrees, stash, history editing, conflicts,
forge integration, PWA behavior, and teaching built on professional semantics.

> **Current security warning:** the prototype now binds `127.0.0.1:8080` by
> default, but its write endpoints do not yet authenticate requests. Session,
> origin/CSRF, and repository-isolation controls remain V1 architecture work.
> `gv --lan` is an explicit unauthenticated compatibility mode for a trusted
> personal LAN, not the target secure remote mode.

## Product Direction

- Professional Git client first; teaching is a major layer, not the base product.
- iPad, finger, and Apple Pencil as primary design inputs.
- Local-first and single-user, with no required Git-Vista cloud account.
- Git-Vista runs beside repositories on Linux; an SSH tunnel is the target remote
  access path.
- One frontend for iPad, Linux, macOS, Windows, touchscreens, and large displays.
- Standard Git remains authoritative and interoperable with terminal workflows.

## Documentation

- [Future vision](docs/FUTURE_VISION.md)
- [V2 architecture](docs/V2_ARCHITECTURE.md)
- [Git client roadmap](docs/GIT_CLIENT_ROADMAP.md)
- [iPad interaction design](docs/IPAD_DESIGN.md)
- [Remote Linux architecture](docs/REMOTE_ARCHITECTURE.md)
- [Security model](docs/SECURITY_MODEL.md)
- [Feature and competitive matrix](docs/FEATURE_MATRIX.md)
- [Reusable architecture review prompt](docs/prompts/ARCHITECT_REVIEW.md)
- [Agent handoff prompt](docs/prompts/HANDOFF.md)

`DESIGN.md` and `PROJECT_MEMORY.md` preserve the prototype's phased implementation
history. The documents under `docs/` are proposed architecture, not claims about
the current code.

## Workspace layout

The current prototype has five crates:

```
git-vista/
├── Cargo.toml                    # workspace root
├── rust-toolchain.toml           # stable toolchain + wasm32 target
├── gv                            # launcher: rebuild the SPA + serve a repo
└── crates/
    ├── git-vista-core/           # wasm-safe domain model + pure shared logic
    ├── git-vista-protocol/       # versioned HTTP contract: protocol negotiation,
    │                             #   API error envelope, request-id, wire DTOs
    ├── git-vista-git/            # native git reading via gix (native-only)
    ├── git-vista-server/         # axum HTTP backend
    │   └── src/                  # routes, contract middleware, Git commands, state
    └── git-vista/                # the Leptos wasm UI (bin: git-vista-ui)
        ├── index.html            # Trunk entry point
        ├── styles.css
        └── src/                  # feature, rendering, gesture, and state modules
```

`git-vista-protocol` is the **transport contract**, separated from the domain
model so the wire format versions independently: the server and the wasm frontend
both depend on it, while `git-vista-core` stays free of transport concerns. See
[ADR 0002](docs/adr/0002-versioned-api-contract.md).

`git-vista-git` is kept **separate** from `git-vista-core` on purpose: gix reads a
filesystem repo and can't compile for wasm, so keeping it out of `core` lets the
browser frontend depend on a clean, wasm-safe core. Both the server and the UI
share `git-vista-core`'s types, so the same structs flow from the git walker
through JSON into the UI with no duplication. V2 will split pure domain,
versioned protocol, graph, repository application, and forge concerns as those
boundaries earn their own crates.

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

The server serves both the WASM bundle and same-origin API on `:8080`. Same-origin
delivery reduces frontend configuration; it does not authenticate the current
write endpoints.

## Current Prototype Features

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
- Working-tree summary, stage-all/unstage-all, commit diffs, file viewing, branch
  checkout, rebase gating, activity history, contextual undo, and graph printing.

See the [feature matrix](docs/FEATURE_MATRIX.md) for an honest current/target split.

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

Then open the **sign-in link** it prints (M1.04):

```
gv: sign in on the iPad/browser by opening this one-time link:
gv:   http://localhost:8080/#s=<token>
```

- on this machine: open that link in the browser.
- from an iPad: forward local port `8080` through SSH to `127.0.0.1:8080` on the
  Linux host, then open the link (the tunnel makes `localhost:8080` on the iPad the
  server).

The link carries a one-time token in the URL *fragment* — it never reaches the
server or any log. Opening it exchanges the token for an HttpOnly, `SameSite=Strict`
session cookie; the app then works normally. The token is **single-use** and
expires; `./gv --token` reprints a fresh link for the running server (e.g. for a
second device), and restarting the server mints a new one. Until a browser signs
in, the app shows a "Connect to git-vista" screen and the API answers `401`.

Every mutating request additionally carries a per-session CSRF token, and the
server validates `Origin`/`Host` (loopback-only, defeating DNS rebinding) and
content type on top of the session — see [ADR 0004](docs/adr/0004-loopback-sessions.md)
and [the security model](docs/SECURITY_MODEL.md).

Opening `http://127.0.0.1:8080/` directly in Safari without a tunnel will not
work: on the iPad, `127.0.0.1` means the iPad itself.

For temporary direct access on a trusted personal LAN, run `./gv --lan [path]`.
This binds all interfaces over plain HTTP (no TLS); a session is still required,
but DNS-rebinding protection is weaker than loopback, so prefer the SSH tunnel. See
[Remote Linux Architecture](docs/REMOTE_ARCHITECTURE.md) for the target session
and tunnel design.

Under the hood that's just:

```sh
( cd crates/git-vista && trunk build )        # build the wasm bundle into dist/
cargo run -p git-vista-server -- <repo-path>  # serve SPA + API on :8080
```

Frontend-only iteration (no API, no real data) still works with
`cd crates/git-vista && trunk serve`.

## Tests

These are the exact commands CI runs, in the order it runs them:

```sh
cargo fmt --all -- --check                                # formatting is clean
cargo clippy --workspace --all-targets -- -D warnings     # strict clippy (native)
cargo clippy -p git-vista --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test --workspace                                    # headless test suite
cd crates/git-vista && trunk build                        # the real wasm bundle
```

The core and Git crates include headless tests and repository fixtures. V2 requires
additional route-policy, operation-state, crash-recovery, browser, and real-device
coverage described in the architecture documents.

### Toolchain and terminal colour

The toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml): stable
Rust plus the `wasm32-unknown-unknown` target, so `rustup` selects the right
compiler automatically. The strict clippy pass runs twice — once for the native
host across the whole workspace, and once for the configured wasm32 target scoped
to the frontend crate, because `git-vista-git` (gix) and `git-vista-server`
(axum/tokio) are native-only and don't compile for wasm.

Diagnostic colour follows the standard `NO_COLOR` / `CARGO_TERM_COLOR`
conventions. CI pins `CARGO_TERM_COLOR=always` so logs keep colour; set
`NO_COLOR=1` (or `CARGO_TERM_COLOR=never`) locally for plain output when capturing
a `--check` diff or a lint log into a file.

## Status

The visualizer prototype is functional and has continued beyond its original
Phase 12/13 plan. It is not yet a secure professional daily-driver client. The V2
roadmap begins with repository identity, protocol versioning, session security,
typed operation planning, mutation serialization, and recovery evidence before
expanding Git feature breadth.
