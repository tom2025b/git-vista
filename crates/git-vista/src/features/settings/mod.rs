//! Settings: GitHub credentials (#584) and local browser-data controls (#75).
//! `storage` owns export and clear policy; `browser_storage` is its DOM adapter.
//!
//! `core` is pure and host-tested — every decision and every sentence. The
//! dialog itself (`dialogs::settings`) is the wasm-only shell around it: it
//! reads a [`git_vista_protocol::TokenStatus`] over the wire and calls
//! `core::status_line`/`core::save_enabled` rather than deciding either
//! question inline, the same split ADR 0115 already asks of every other
//! feature here.

#[cfg(target_arch = "wasm32")]
pub mod browser_storage;
pub mod core;
pub mod storage;
