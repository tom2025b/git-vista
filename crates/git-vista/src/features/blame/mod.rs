//! Rename-aware file history and blame (M5.33, #86): `core` holds the pure
//! touch/keyboard selection state and the path-state/rename-limit messages;
//! `view` (wasm-only) is the panel that fetches `/api/blame` and
//! `/api/file-history` and renders them, wired to the existing commit-detail
//! panel and comparison-anchor machinery rather than inventing new ones.

pub mod core;
#[cfg(target_arch = "wasm32")]
pub mod view;

// `view.rs` is wasm-only, so `cargo test` never compiles a line of it. This
// census reads its bytes instead and pins the claim the module doc above
// makes — that it holds no decisions of its own (ADR 0115).
#[cfg(test)]
mod view_census;
