//! Passive discovery of git's bisect session (#708, ADR 0138).
pub mod core;
#[cfg(target_arch = "wasm32")]
pub mod signals;
