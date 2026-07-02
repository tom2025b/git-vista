//! `git-vista-core` — pure, UI-independent logic for git-vista.
//!
//! Nothing in this crate knows about HTTP, Leptos, rendering — or even how to
//! read a git repository. It's **pure logic with no platform dependencies**, so
//! it compiles cleanly for both native and wasm and is shared, as-is, by the
//! browser frontend and the native backend. Two small layers, each testable:
//!
//! - [`model`]  — serializable data types shared across the HTTP/JSON boundary.
//! - [`layout`] — assigns commits to lanes for the vertical graph.
//! - [`status`] — working-tree status types + the porcelain-v2 parser.
//! - [`diff`]   — commit-diff types + the name-status/numstat parsers.
//!
//! Reading real history (which needs `gix` and a filesystem, and so can't run in
//! a browser) lives in the separate native-only `git-vista-git` crate. Keeping
//! it out of here is what lets this crate stay clean and browser-compatible.

pub mod diff;
pub mod layout;
pub mod model;
pub mod status;
