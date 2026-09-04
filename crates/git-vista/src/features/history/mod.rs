//! The graph panel's load phase (#612).
//!
//! There is no `signals.rs` here and there is not meant to be one. The phase
//! lives in a signal the [`App`](crate::app) shell owns, driven from three
//! Leptos effects that cannot be host-compiled; what moved into [`core`] is
//! the part of those effects that *decides* — which phase an epoch bump
//! produces, which seed replies may advance the phase at all, and whether an
//! armed retry timer is still speaking for the failure it was armed for.
//!
//! Before #612 all three rules were written inline in `app/mod.rs`, which is
//! `#[cfg(target_arch = "wasm32")]`: compiled and linted by CI, executed by no
//! test runner. Each is now a function `cargo test -p git-vista --bins` runs,
//! and a source-level census pins the wasm-only effects to them so the rules
//! cannot quietly grow a second copy back in the view.

pub mod core;
