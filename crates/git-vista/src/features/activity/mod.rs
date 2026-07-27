//! The Activity panel: its visibility, and the pure decisions its feed rows make
//! (M1.11, #64).

pub mod core;

#[cfg(target_arch = "wasm32")]
pub mod signals;
