//! Git operations: what the user asked for, what is in flight, and how it settled.
//!
//! The only feature permitted to start a write (M1.11 D4). Other features raise an intent
//! here; this module publishes an [`Invalidate`](crate::features::core_traits::Invalidate)
//! back when the write settles, rather than every caller re-reading everything.

pub mod core;
pub mod kind;
// The reactive half. Gated because it imports Leptos; the core above is not, so its tests
// run on the host under the ordinary `cargo test --workspace` (M1.11 D1).
#[cfg(target_arch = "wasm32")]
pub mod signals;
