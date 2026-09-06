//! Explicitly requested, transient provider reads, separate from graph loading.
pub mod core;
#[cfg(target_arch = "wasm32")]
pub mod view;
