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
//! All four consult **one** guard since M1.11 (#64) — [`Dialogs`], created in `App`.
//! Before that there were three separate `StoredValue<f64>` clocks and eleven inlined
//! copies of the same `Date::now()` comparison; the rule was right everywhere by
//! repetition rather than by construction.
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
//!   * [`preview_panel`] — the before/after graph a confirmation draws (#594).
//!
//! [`DIALOG_GUARD_MS`]: crate::features::dialogs::core::DIALOG_GUARD_MS
//! [`Dialogs`]: crate::features::dialogs::signals::Dialogs

use leptos::*;

mod commit;
mod confirm;
mod open_url;
// M10.08 A6 (#594): the before/after graph drawn inside a confirmation.
mod preview_panel;
mod reset;

pub use commit::commit_dialog_view;
pub use confirm::{confirm_modal_view, error_modal_view, pull_picker_view};
pub use open_url::open_url_view;
pub use preview_panel::preview_panel_view;
pub use reset::reset_repo_view;

/// Pop a native alert with `msg` (there's always a window in the running SPA).
///
/// Only the test-repo reset still uses this. The branch operations moved off it in M1.11
/// (#64): a modal outside the component tree cannot be styled, cannot be dismissed by the
/// app, and — the real problem — is not *state*, so a failure it reported left nothing
/// behind. Those outcomes now settle into the operations core instead.
fn alert(msg: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.alert_with_message(msg);
    }
}
