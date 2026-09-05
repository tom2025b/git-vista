//! Rename-aware file history and blame (M5.33, #86): `core` holds the pure
//! touch/keyboard selection state and the path-state/rename-limit messages;
//! `view` (wasm-only) is the panel that fetches `/api/blame` and
//! `/api/file-history` and renders them, wired to the existing commit-detail
//! panel and comparison-anchor machinery rather than inventing new ones.

pub mod core;
#[cfg(target_arch = "wasm32")]
pub mod view;
