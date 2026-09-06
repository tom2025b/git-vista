//! The settings surface (M13.03, #584): one field to save the GitHub token
//! the credential helper offers for private-repository operations.
//!
//! `core` is pure and host-tested — every decision and every sentence. The
//! dialog itself (`dialogs::settings`) is the wasm-only shell around it: it
//! reads a [`git_vista_protocol::TokenStatus`] over the wire and calls
//! `core::status_line`/`core::save_enabled` rather than deciding either
//! question inline, the same split ADR 0115 already asks of every other
//! feature here.

pub mod core;
