//! Entry point for the Leptos frontend.
//!
//! Trunk compiles this crate for `wasm32-unknown-unknown` and serves the result.
//! The `cfg` split keeps a plain `cargo build --workspace` (host target) happy:
//! on native there's nothing to mount, so we emit a tiny stub binary.

// Pure layout/colour/demo logic — no UI deps, so it compiles (and is tested) on
// the host too. Only the host's non-test build leaves it unused, hence the
// targeted allows.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod camera;
// Branch colours live in the crate-wide `git_vista_core::color` "Color God" — the
// single source of truth shared with the layout engine — so there's no local
// colour module here; the render code imports from core directly.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod datetime;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod geometry;
// M1.10 (#63): the paged-history aggregate and its request state. Pure
// validate-then-commit logic over the wire types — no Leptos, no DOM — so the
// all-or-nothing invariants are unit-tested on the host like the geometry above,
// not only exercised in a browser.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod history;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod icons;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod lod;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod text;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod viewport;
// Hardcoded demo history. No longer used in the render path (the frontend now
// fetches real history from the backend), but kept for tests/fixtures.
#[cfg(test)]
mod graph;

// The frontend, split out of the former monolithic `app.rs`. Every one of these
// pulls in Leptos / web-sys (wasm-only deps), so — like `app` — they compile only
// for the wasm target; a host `cargo build --workspace` builds the native stub
// below and skips them entirely.
#[cfg(target_arch = "wasm32")]
mod activity;
#[cfg(target_arch = "wasm32")]
mod api;
#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod detail;
#[cfg(target_arch = "wasm32")]
mod dialogs;
#[cfg(target_arch = "wasm32")]
mod gestures;
#[cfg(target_arch = "wasm32")]
mod menu;
// The repo picker + Visualize/Active mode screens (ADR 0006/0009).
#[cfg(target_arch = "wasm32")]
mod picker;
#[cfg(target_arch = "wasm32")]
mod prefs;
#[cfg(target_arch = "wasm32")]
mod print;
#[cfg(target_arch = "wasm32")]
mod render;
// M1.04 (#57): the loopback session bootstrap flow — exchange the one-time
// `#s=<token>` fragment for a session cookie, and the blocking sign-in screen.
#[cfg(target_arch = "wasm32")]
mod session;
#[cfg(target_arch = "wasm32")]
mod state;
// M1.02 (#102): the blocking "Update Required" screen shown when the client's
// protocol version is incompatible with the server's.
#[cfg(target_arch = "wasm32")]
mod update_required;
#[cfg(target_arch = "wasm32")]
mod viewer;

#[cfg(target_arch = "wasm32")]
fn main() {
    // Surface Rust panics in the browser devtools console.
    console_error_panic_hook::set_once();
    leptos::mount_to_body(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!(
        "git-vista's frontend is a WebAssembly app built with Trunk.\n\
         Run it via the `gv` launcher, or `trunk serve` for frontend-only iteration."
    );
}
