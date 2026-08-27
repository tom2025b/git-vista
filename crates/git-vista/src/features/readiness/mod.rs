//! The full-screen viewer's machine-readable readiness signal (#387).
//!
//! [`core`] holds the pure predicate — [`core::is_viewer_busy`] — plus the
//! small identity types it compares. `viewer.rs` is the wasm-only caller:
//! it reduces `crate::state::ViewerDoc` and the resource's resolved
//! `DocResult` down to this module's [`core::DocIdentity`]/
//! [`core::FetchOutcome`], calls the predicate, and stamps the result as
//! `aria-busy` on the viewer's outer `<div>`.

pub mod core;
