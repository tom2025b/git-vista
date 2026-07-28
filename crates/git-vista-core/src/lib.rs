//! `git-vista-core` — pure, UI-independent logic for git-vista.
//!
//! Nothing in this crate knows about HTTP, Leptos, rendering — or even how to
//! read a git repository. It's **pure logic with no platform dependencies**, so
//! it compiles cleanly for both native and wasm and is shared, as-is, by the
//! browser frontend and the native backend. Two small layers, each testable:
//!
//! - [`model`]    — serializable data types shared across the HTTP/JSON boundary.
//! - [`identity`] — stable, opaque, path-independent repository/worktree ids,
//!   validated object ids, and repository generation tokens.
//! - [`color`]    — the single source of truth for branch colours (palette,
//!   slots, hex values), shared by the layout engine and the UI.
//! - [`layout`]   — assigns commits to lanes for the vertical graph.
//! - [`status`]   — working-tree status types + the porcelain-v2 parser.
//! - [`diff`]     — commit-diff types + the name-status/numstat parsers.
//! - [`activity`] — activity-feed types, reflog-message parsing, feed assembly.
//! - [`net`]      — user-facing wording for network-level fetch failures.
//! - [`seed`]     — test-repo seed parsing + reset planning ("Reset Test Repo").
//! - [`virtualize`] — the windowed-list primitive (M2.16, #69c): item heights
//!   and a scroll offset in, the visible render range out. Knows nothing
//!   about diffs or the commit graph, so both can share it.
//! - [`request_generation`] — cancellation via generation-tag (M2.16, #69d):
//!   a monotonic counter a virtualized view bumps on every scroll-driven
//!   refetch, so a late-arriving response can identify itself as stale and
//!   be discarded instead of painting over newer content. Same shape as
//!   [`identity::RepositoryGeneration`] (ADR 0001) applied to view state
//!   instead of repository state.
//!
//! Reading real history (which needs `gix` and a filesystem, and so can't run in
//! a browser) lives in the separate native-only `git-vista-git` crate. Keeping
//! it out of here is what lets this crate stay clean and browser-compatible.

pub mod activity;
pub mod color;
pub mod diff;
pub mod forge;
pub mod identity;
pub mod layout;
pub mod model;
pub mod net;
pub mod request_generation;
pub mod seed;
pub mod status;
pub mod virtualize;
