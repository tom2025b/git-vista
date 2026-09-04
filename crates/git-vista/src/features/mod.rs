//! Feature boundaries (M1.11, #64).
//!
//! One module per area named in the issue. Each owns its state; nothing here writes
//! another feature's state directly (design spec D2). `core.rs` files are framework-free
//! and host-tested; `signals.rs` files are the wasm-only reactive wrappers.

pub mod core_traits;

pub mod a11y;
pub mod activity;
// No `conflicts` module here any more. M4.31's four-pane view model and its
// marker-file block editor moved to the `git-vista-conflicts` crate for
// M10.07 (#462; ADR 0105), so the terminal client resolves conflicts through
// the same implementation this one does rather than a second copy of it. They
// were always framework-free and host-tested, which is what made the move a
// `git mv`; `api::conflicts` and `viewer.rs` now name that crate directly.
pub mod dialogs;
pub mod diff;
pub mod explain;
pub mod graph;
pub mod operations;
pub mod preview;
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
