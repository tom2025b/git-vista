//! Disjoint pane projections layered onto the persistent M10.02 shell.
//!
//! Each feature pane owns its view-specific state and pure row projection;
//! the shared reducer, key dispatcher and frame renderer only integrate those
//! seams. Keeping the detail logic here prevents #458's diff vocabulary and
//! windowing rules from turning `app.rs` or `ui.rs` into a second monolith.

pub mod conflicts;
pub mod detail;
pub mod graph;
