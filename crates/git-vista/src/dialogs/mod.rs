//! The four modal overlays: the commit-message dialog (Issue #33), the branch-op
//! confirmation (Issue #33 follow-up), the Reset Test Repo confirmation
//! (iPad-testing follow-up), and the Open-URL clone dialog (Phase 12).
//!
//! All share the iPad-proven recipe learned the hard way: inline, viewport-sized
//! (100vw/100vh) styles that render reliably on iOS WebKit; a `<textarea>` for any
//! text field, never a void `<input>` (which panics Leptos' CSR `<template>`
//! node-walk on iOS WebKit and stops the whole view mounting); and a backdrop that
//! ignores a dismiss landing within [`DIALOG_GUARD_MS`] of opening, so iOS's
//! synthesized post-tap "ghost click" can't close the modal it just opened.
//!
//! # Module layout
//!
//! This split is move-only — one file per overlay, with the two error-reporting
//! helpers ([`alert`] and [`report`]) shared here at the root:
//!
//!   * [`commit`]   — the commit-message dialog.
//!   * [`confirm`]  — the branch-op / undo confirmation (the largest, many arms).
//!   * [`reset`]    — the Reset Test Repo confirmation.
//!   * [`open_url`] — the clone-by-URL dialog.
//!
//! [`DIALOG_GUARD_MS`]: crate::state::DIALOG_GUARD_MS

use leptos::*;

mod commit;
mod confirm;
mod open_url;
mod reset;

pub use commit::commit_dialog_view;
pub use confirm::confirm_modal_view;
pub use open_url::open_url_view;
pub use reset::reset_repo_view;

/// Pop a native alert with `msg` (there's always a window in the running SPA).
fn alert(msg: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.alert_with_message(msg);
    }
}

/// Resolve a git op's result: bump `reload` so the graph re-reads on success, or
/// surface git's own error text ("Couldn't {what}:\n<git stderr>") on failure.
/// Generic over the success payload — some requests return `()`, the branch ops
/// return the server's success line — since either way it's dropped here.
fn report<T>(result: Result<T, String>, what: &str, reload: RwSignal<u32>) {
    match result {
        Ok(_) => reload.update(|n| *n = n.wrapping_add(1)),
        Err(e) => alert(&format!("Couldn't {what}:\n{e}")),
    }
}
