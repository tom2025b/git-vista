//! The worktree drawer (M11.03, #548): see every desk, and switch between them.
//!
//! Split the way every other feature here is: [`core`] holds the decisions and
//! is compiled and run by `cargo test --workspace`; `view` holds the markup and
//! is `#[cfg(target_arch = "wasm32")]`, so nothing in it can decide anything.
//! The browser suite (`ci/browser/tests/worktree-drawer.spec.mjs`) is what
//! proves the view is *reached*; `core`'s host tests are what prove the values
//! it renders are right.

pub mod core;

#[cfg(target_arch = "wasm32")]
pub mod view;
