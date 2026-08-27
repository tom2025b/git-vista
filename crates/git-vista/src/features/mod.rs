//! Feature boundaries (M1.11, #64).
//!
//! One module per area named in the issue. Each owns its state; nothing here writes
//! another feature's state directly (design spec D2). `core.rs` files are framework-free
//! and host-tested; `signals.rs` files are the wasm-only reactive wrappers.

pub mod core_traits;

pub mod a11y;
pub mod activity;
// M4.31a (#428): the four panes of a conflict view, and the state of each.
pub mod conflicts;
pub mod dialogs;
pub mod diff;
pub mod explain;
pub mod graph;
pub mod operations;
// #387: the full-screen viewer's readiness predicate — derived from the same
// staleness check `viewer.rs`'s body match already makes, not a new signal.
pub mod readiness;
pub mod session;
pub mod shell;
pub mod status;
// M3.24 (#77): the stash drawer — rows, action offers, push preview,
// and the client-composed pop.
pub mod stash;
pub mod tags;
