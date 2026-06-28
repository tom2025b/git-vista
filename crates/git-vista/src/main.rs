//! Entry point for the Leptos frontend.
//!
//! Trunk compiles this crate for `wasm32-unknown-unknown` and serves the result.
//! The `cfg` split keeps a plain `cargo build --workspace` (host target) happy:
//! on native there's nothing to mount, so we emit a tiny stub binary.

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
