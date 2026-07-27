//! The graph feature: the paged-history aggregate, the render context, and the
//! epoch that decides when a stale view must be discarded (M1.11, #64).

pub mod core;
// The reactive half. Gated because it imports Leptos/web-sys; the core above is not,
// so its rules — including `LoadedHistory`'s validate-then-commit invariants — run on
// the host under the ordinary `cargo test --workspace` (M1.11 D1).
#[cfg(target_arch = "wasm32")]
pub mod signals;
