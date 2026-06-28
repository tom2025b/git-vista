# git-vista

A desktop application that visualizes git history as a clean, zoomable **vertical**
graph. Built in Rust with **Tauri** (native desktop shell) + **Leptos**
(Rust to WebAssembly UI), on top of a pure-logic core crate.

## Workspace layout

```
git-vista/
├── Cargo.toml                    # workspace root
├── rust-toolchain.toml           # stable toolchain + wasm32 target
└── crates/
    ├── git-vista-core/           # pure logic — NO UI dependencies
    │   └── src/
    │       ├── lib.rs            # crate root, module wiring
    │       ├── model.rs          # serializable data types (cross IPC)
    │       ├── repo.rs           # git history reader (stub)
    │       └── layout.rs         # vertical lane-assignment (stub)
    └── git-vista/                # the GUI application
        ├── index.html           # Trunk entry point
        ├── Trunk.toml           # frontend build/serve config
        ├── styles.css
        ├── src/
        │   ├── main.rs          # Leptos wasm entry (mounts App)
        │   └── app.rs           # Leptos components
        └── src-tauri/           # native desktop shell (Tauri v2)
            ├── Cargo.toml
            ├── build.rs
            ├── tauri.conf.json
            ├── capabilities/default.json
            └── src/
                ├── main.rs      # desktop binary entry
                ├── lib.rs       # Tauri builder + IPC handlers
                └── commands.rs  # #[tauri::command] functions
```

## Why three packages for "two crates"?

Conceptually there are two crates: **git-vista-core** (logic) and **git-vista**
(the GUI). But Tauri requires its native shell to be a *separate* cargo package,
because the frontend and the desktop binary compile for **different targets** and
never link together:

- The Leptos frontend (`crates/git-vista`) compiles to **wasm32** and is built by
  **Trunk** into `dist/`.
- The Tauri shell (`crates/git-vista/src-tauri`) compiles to a **native** binary
  that opens a webview and loads those `dist/` assets.

Both depend on `git-vista-core` for shared data models, so the same types flow
from the git walker through the IPC boundary into the UI with no duplication.

## Prerequisites

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk
cargo install tauri-cli --version "^2.0"
```

On Linux (Mint), Tauri also needs system webview libraries — `webkit2gtk-4.1`,
`librsvg2`, `libayatana-appindicator3`, etc. See the Tauri v2 prerequisites docs.

> Dependency versions in the Cargo.toml files (leptos, tauri, gix) are pinned to
> known-good majors — bump them to the latest releases before the first build.

## Running

```sh
# Dev: launches a native window with the hot-reloading Leptos frontend.
cargo tauri dev

# Frontend only, in a browser (no native shell):
cd crates/git-vista && trunk serve
```

`cargo tauri dev` runs `trunk serve` first (see `beforeDevCommand` in
`tauri.conf.json`), then opens the Tauri window pointed at it.

## Tests

```sh
cargo test -p git-vista-core
```

## Status

Scaffold. The `repo` (git walking) and `layout` (lane assignment) modules are
documented stubs — see their module docs for the planned implementation and the
companion lesson in the teacher repo.
