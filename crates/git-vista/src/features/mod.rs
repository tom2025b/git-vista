//! Feature boundaries (M1.11, #64).
//!
//! One module per area named in the issue. Each owns its state; nothing here writes
//! another feature's state directly (design spec D2). `core.rs` files are framework-free
//! and host-tested; `signals.rs` files are the wasm-only reactive wrappers.

pub mod core_traits;

pub mod diff;
pub mod operations;
pub mod session;
pub mod status;
