//! The app shell: which overlays are up, in what order, and which one Esc dismisses
//! (M1.11, #64).

pub mod core;

#[cfg(target_arch = "wasm32")]
pub mod signals;
