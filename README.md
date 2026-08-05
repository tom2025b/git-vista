# Git-Vista

Git-Vista is a professional, touch-first visual Git client for one developer
using an iPad to work with real repositories on Linux. The browser is the
portable UI platform; Rust, Axum, `gix`, System Git, Leptos, and WebAssembly
provide the native repository service and adaptive client.

Its **V1 foundation is complete**: every write reaches the repository through
one typed, reviewable planner, mutations against a repository serialize, Git
process execution is policy-bounded, and an operation journal makes recovery
provable rather than assumed. V1's daily-driver line — full working-tree
status, diffs, staging, commit/amend/hook/signing UX, remotes, tags, and an
installable PWA — is **in progress** on top of that foundation. The proposed
V2 direction beyond it adds worktrees, stash, history editing, conflict
resolution, forge integration, and teaching built on the same professional
semantics. See [Status](#status) below for exactly what has shipped.

> **Current security boundary:** the server is hard-limited to
> `127.0.0.1:8080` and requires a single-use bootstrap, HttpOnly session, CSRF,
> and strict Host/Origin checks. Non-loopback bind overrides are refused; an SSH
> local-port forward is the only supported iPad access path. Every mutation —
> from the browser or from an MCP agent — is planned, serialized per repository,
> and journaled before it runs; see [the security model](docs/SECURITY_MODEL.md)
> and the ADR index below for how.

## Product Direction

- Professional Git client first; teaching is a major layer, not the base product.
- iPad, finger, and Apple Pencil as primary design inputs.
- Local-first and single-user, with no required Git-Vista cloud account.
- Git-Vista runs beside repositories on Linux; an SSH tunnel is the target remote
  access path.
- One frontend for iPad, Linux, macOS, Windows, touchscreens, and large displays.
- Standard Git remains authoritative and interoperable with terminal workflows.

## Documentation

- [Architecture Decision Records](docs/adr/) — the numbered, dated record of
  every decision expensive to reverse (49 so far, 0001–0049). This is where
  implemented behavior is authoritative; start here before trusting a claim in
  a prose doc below against the running code.
- [Future vision](docs/FUTURE_VISION.md) — proposed, not current.
- [V2 architecture](docs/V2_ARCHITECTURE.md) — proposed, not current.
- [Git client roadmap](docs/GIT_CLIENT_ROADMAP.md) — proposed, not current.
- [iPad interaction design](docs/IPAD_DESIGN.md)
- [Remote Linux architecture](docs/REMOTE_ARCHITECTURE.md)
- [Security model](docs/SECURITY_MODEL.md)
- [Feature and competitive matrix](docs/FEATURE_MATRIX.md) — proposed baseline,
  not current.

`DESIGN.md` preserves the prototype's phased implementation history. The
future-vision, V2-architecture, roadmap, and feature-matrix documents under
`docs/` are proposed direction, not claims about the current code — treat an
ADR or the code itself as the tiebreaker whenever one of them looks ahead of
what has actually shipped. Agent prompts, session handoffs, and running
project memory are local working material and are intentionally excluded from
the repository's public-facing docs.

## Workspace layout

The workspace has six crates:

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
    │   └── src/                  # routes, planner, journal, contract
    │                             #   middleware, sandboxed Git execution, state
    ├── git-vista-mcp/            # MCP stdio bridge: agents drive git-vista
    │                             #   through the same HTTP API the browser uses
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
through JSON into the UI with no duplication.

`git-vista-mcp` is kept **separate from `git-vista-server`** on purpose too: it
links `git-vista-protocol` and `git-vista-core` for the shared wire types but
never the server crate, so an agent talking MCP reaches the repository only
through the same loopback HTTP API and the same reviewable planner the browser
uses — never a shell, never raw argv. A dependency-graph test proves the write
path is structurally unreachable from this crate, not merely unrouted. See
[ADR 0046](docs/adr/0046-mcp-plan-tool-surface.md).

V2 will split pure domain, versioned protocol, graph, repository application,
and forge concerns further as those boundaries earn their own crates.

## Architecture

```
  browser (SPA, wasm)                                  git-vista-server (native)
  ────────────────────                HTTP             ─────────────────────────
  fetch  /api/commits, /api/status  ─────────────────▶  reads: walk_history,
  fetch  /api/diff/{id}, /api/file  ─────────────────▶  status, diff, activity  ─┐
                                                                                  │ gix reads
  POST   /api/plan                  ─────────────────▶  shared planner:         ─┤ the repo
         { operation, targets }                          typed operation        ─┤ on the
                                     ◀─────────────────   → reviewable Plan       │ filesystem
  POST   /api/{commit,branch,merge,                                             ─┤
         push,pull,fetch,tag,       ─────────────────▶  execute: serialized     ─┤ system git
         amend-commit,checkout,…}                         per-repo, journaled,     (shell,
                                                            bounded git exec       sandboxed)
                                     ◀─────────────────   → result / undo ref    ─┘

  MCP agent (stdio)  ──git-vista-mcp──▶  same loopback HTTP API, same planner,
                                          read tools + build-only plan tools
                                          (execution is a separate, later stage)
```

Every write — whatever the caller — goes through the one shared planner
described in [ADR 0016](docs/adr/0016-shared-write-planner.md): build a typed
`Plan` from a closed operation vocabulary
([ADR 0015](docs/adr/0015-typed-operation-vocabulary-and-plan-schema.md)),
then execute it, serialized per repository, against a policy-bounded Git
process, with a durable operation journal behind it
([ADR 0019](docs/adr/0019-serialized-mutations-per-repository.md),
[ADR 0021](docs/adr/0021-durable-operation-journal-and-recovery-refs.md)).
The server serves both the WASM bundle and same-origin API on `:8080`.

## Current Features

- Vertical commit graph with robust lane assignment (branches, merges, octopus).
- Pan & zoom via **Pointer Events** — drag to pan, wheel to zoom on desktop,
  one-finger drag + two-finger pinch on iPad/Safari.
- Stable **per-branch colours**, and HEAD / branch / tag badges beside commits.
- Commit labels (message · short hash · author · local date), with **level of
  detail** (text hidden when zoomed out) and **viewport virtualization** (only
  on-screen rows are rendered, for large histories).
- **GitHub links** on commits/refs when the repo has a `github.com` origin — only
  for pushed objects, so a link never 404s.
- **Commit detail panel**: "View details" opens a side panel with the full
  message body and both author & committer signatures; parent hashes are
  clickable to walk up the history.
- **Open URL**: paste a public `https://`/`http://`/`git://` URL to clone it
  into the persistent clones store, then choose Visualize (read-only) or
  Active mode. Clones survive a restart and stay listed in the picker until
  deleted.
- **Controls & shortcuts**: drag/one-finger to pan, wheel/pinch to zoom, plus
  keyboard shortcuts on desktop and the iPad Magic Keyboard — `+`/`-` zoom, `0`
  resets the view, `r` refreshes, `Esc` closes the open menu/panel. A **Reset
  view** button recenters the camera for pure touch/trackpad use.
- Working-tree summary, stage-all/unstage-all, commit diffs, file viewing,
  branch checkout, rebase gating, activity history, contextual undo, and graph
  printing.
- **Write actions** from the graph's context menu, each confirmed in an
  iPad-safe in-app modal and each running through the shared planner: create
  branch, commit, **amend**, merge, **fetch, pull, push**, delete branch,
  create/list/delete **tags**.
- **MCP agent bridge** (`git-vista-mcp`): an agent gets read tools (graph,
  commit detail, diff, activity, status, repository selection) plus 23
  build-only `plan_<operation>` tools — it can ask "what would this operation
  do" and get back risk, preconditions, and affected refs with nothing
  touching the repository. Submitting an approved plan for execution is a
  separate, later stage. See
  [ADR 0046](docs/adr/0046-mcp-plan-tool-surface.md).

See the [feature matrix](docs/FEATURE_MATRIX.md) for the target/current split
(the matrix predates the M2 work above and is due a refresh; the ADR index is
the current source of truth in the meantime).

## Prerequisites

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk
```

A working `git` on `PATH` (the server shells out to it for writes and clones); the
history read itself uses `gix`'s pure-Rust reader.

## Running

The normal path is the `gv` launcher: it builds the WASM SPA and server before
replacing the previous process, then points the server at a repository.

```sh
./gv                  # visualise the CURRENT directory's repo
./gv ~/code/myproj    # visualise another repo by path
```

Then open the **sign-in link** it prints:

```
gv: sign in on the iPad/browser by opening this one-time link:
gv:   http://localhost:8080/#s=<token>
```

- on this machine: open that complete link in the browser.
- from an iPad: forward local port `8080` through SSH to `127.0.0.1:8080` on the
  Linux host, then open the link (the tunnel makes `localhost:8080` on the iPad the
  server).

The link carries a one-time token in the URL *fragment* — it never reaches the
server or any log. Opening it exchanges the token for an HttpOnly, `SameSite=Strict`
session cookie; the app then works normally. The token is **single-use** and
expires; `./gv --token` prints a fresh localhost link for the running server.
Each browser/device needs a newly printed link because a successful exchange
consumes it. The complete `#s=...` fragment is required; the token is not a
password to paste into the app. Until a browser signs in, the app shows a
"Connect to git-vista" screen and the API answers `401`.

Every mutating request additionally carries a per-session CSRF token, and the
server validates `Origin`/`Host` against loopback plus the content type on top of
the session — see [ADR 0004](docs/adr/0004-loopback-sessions.md) and
[the security model](docs/SECURITY_MODEL.md).

Opening `http://127.0.0.1:8080/` directly in Safari without a tunnel will not
work: on the iPad, `127.0.0.1` means the iPad itself.

Direct LAN access is deliberately disabled. `./gv --lan` is rejected, and the
server also refuses a non-loopback `GIT_VISTA_BIND_ADDR` override. This keeps the
plain-HTTP Git control surface off Wi-Fi, VPN, container, and public interfaces.

### SSH tunnel workflow and diagnostics

Start Git-Vista normally on Linux so it remains loopback-only:

```sh
./gv /absolute/path/to/repository
./gv doctor
```

In the iPad SSH client, configure a **local** forward from iPad port `8080` to
Linux `127.0.0.1:8080`. The command-line equivalent on another client is:

```sh
ssh -N -L 8080:127.0.0.1:8080 linux-user@linux-host
```

With the tunnel connected, run `./gv --token` on Linux and open its complete
`http://localhost:8080/#s=...` link on the iPad. Here `localhost` deliberately
means the iPad end of the forward. If the tunnel drops, reconnect the same
forward and reload: the Git-Vista session cookie remains valid until its own
idle expiry. Generate a new link only if the browser session itself is gone.

`./gv doctor` prints the actual bind address, health and protocol versions,
launch/catalog roots, token age and permissions, process ownership, and the safe
tunnel recipe. It never prints the token, cookies, or CSRF value. It reports a
security error if port 8080 is ever observed on a non-loopback listener.

### Optional systemd user service

For a server that survives terminal and SSH-client closure under the user's
service manager, build/install the binary and adapt the supplied example:

```sh
cargo build --release -p git-vista-server
install -Dm755 target/release/git-vista-server ~/.local/bin/git-vista-server
mkdir -p ~/.config/systemd/user
cp contrib/systemd/git-vista.service ~/.config/systemd/user/
# Edit WorkingDirectory and the final repository argument in ExecStart.
systemctl --user daemon-reload
systemctl --user enable --now git-vista.service
systemctl --user status git-vista.service
```

The example remains loopback-only. Use `systemctl --user restart/stop
git-vista.service` for a supervised process; `gv` deliberately refuses to kill a
port occupant it does not own. `gv --token` and `gv doctor` still work because
the service and launcher share the same per-user state directory. See [Remote
Linux Architecture](docs/REMOTE_ARCHITECTURE.md) for the target session and
tunnel design.

Under the hood that's just:

```sh
( cd crates/git-vista && trunk build )        # build the wasm bundle into dist/
cargo run -p git-vista-server -- <repo-path>  # serve SPA + API on :8080
```

Frontend-only iteration (no API, no real data) still works with
`cd crates/git-vista && trunk serve`.

## Tests

These are the exact commands CI runs, in the order it runs them (`./dev gate`
runs all five):

```sh
cargo fmt --all -- --check                                # formatting is clean
cargo clippy --workspace --all-targets -- -D warnings     # strict clippy (native)
cargo clippy -p git-vista --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test --workspace                                    # headless test suite
cd crates/git-vista && trunk build                        # the real wasm bundle
```

The core and Git crates include headless tests and repository fixtures;
`git-vista-server` additionally carries planner, journal, sandbox, and
contract-suite coverage. V2 requires additional route-policy, browser, and
real-device coverage described in the architecture documents.

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

**M1 — V1 Foundation** is complete (39 issues shipped, 0 open): repository
identity, protocol negotiation, catalog isolation, loopback sessions, the
typed operation vocabulary and shared planner, serialized per-repository
mutations, a bounded and sandboxed Git process policy, and a durable
operation journal with recovery refs have all landed and shipped.

**M2 — Daily Driver: Status to Push [V1]** is the active milestone, roughly
**62% done** (34 shipped / 21 open, per `./dev roadmap`). Fetch, pull, and
push execution, tag listing and local tag create/delete, amend UX, published-
history warnings, the build/submit planner split, and the MCP `plan_<operation>`
tool surface all landed most recently. Remaining M2 work is full working-tree
status and diff UI, file/hunk/partial staging, guarded discard, complete
commit/hook/signing UX, full remote and upstream management, and an
installable PWA with offline read-only mode — see `./dev roadmap` for the
open issue list.

M3 (parallel work & recovery), M4 (history editing), M5 (investigation &
forges), and M6 (teaching semantics, reduced to a single issue) are the
proposed V2 milestones beyond the V1 line; M7 was retired and M8 deleted as
never-started. [ADR 0049](docs/adr/0049-v1-scope-freeze.md) records that scope
freeze — eighteen never-started issues closed as won't-do, each with an
explicit return condition — and is the place to check before assuming
anything described only in `FUTURE_VISION.md`, `V2_ARCHITECTURE.md`, or
`GIT_CLIENT_ROADMAP.md` is still intended as written.

49 ADRs (`docs/adr/`, numbered 0001–0049) now record the project's
architectural decisions in order; treat that index, not this README's prose,
as the living record of what has actually shipped.
