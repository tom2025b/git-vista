//! The app shell: which overlays are up, in what order, and which one Esc dismisses
//! (M1.11, #64).

pub mod core;
/// ADR 0032's tripwire: no service worker, ever — see the module doc.
mod pwa_guard;
/// Where the inspector sits per mode, and the bottom sheet's detent model (M1.12, #65).
/// Pure decision logic, host-tested; nothing renders it yet — see the module doc.
pub mod sheet;

#[cfg(target_arch = "wasm32")]
pub mod signals;
