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
//! - [`preview`]  — laying out a history that does not exist yet (M10.08, #576):
//!   the same commit list plus a hypothetical commit, laid out through the same
//!   [`layout`] engine, and the lane shifts between the two halves. Takes no
//!   repository at all — [`layout::stream::StreamLayout`] asks for a commit and
//!   a membership predicate, never an object database.
//! - [`seed`]     — test-repo seed parsing + reset planning ("Reset Test Repo").
//! - [`virtualize`] — the windowed-list primitive (M2.16, #69c): item heights
//!   and a scroll offset in, the visible render range out. Knows nothing
//!   about diffs or the commit graph, so both can share it.
//!
//! There is deliberately **no** request-cancellation primitive here. One existed
//! (`request_generation`, M2.16 #69d) and was removed unused in ADR 0053: Leptos
//! 0.6.15's own `create_local_resource` already drops out-of-order completions
//! internally, and every diff/detail response echoes the id it was fetched for so
//! the view can re-check it against the live selection before painting. #69's
//! "cancellable" criterion is met by those two layers. A future fetch surface that
//! refetches per scroll range — rather than fetching one capped patch per commit —
//! would not inherit that reasoning and must re-argue it; see ADR 0053.
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
pub mod preview;
pub mod seed;
pub mod status;
pub mod virtualize;
