//! Modal overlays: which one is up, and the iOS ghost-click guard (M1.11, #64).

pub mod core;

#[cfg(target_arch = "wasm32")]
pub mod signals;
