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
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod color;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod datetime;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod geometry;
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

#[cfg(target_arch = "wasm32")]
mod app;

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
         Run `trunk serve` (browser) or `cargo tauri dev` (desktop) instead."
    );
}
