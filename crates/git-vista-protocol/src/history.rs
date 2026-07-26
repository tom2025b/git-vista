//! Generic paged-history wire envelopes (M1.10, #63).
//!
//! History moved from one whole-graph payload to two shapes: a cheap
//! [`HistoryFrame`] (refs, branch colours, resolved-target metadata — no
//! commits) fetched once per view, and repeated [`HistoryPage`]s (the actual
//! rows/edges/stubs, cursor-paginated) fetched as the user scrolls. Both are
//! generic over the row/edge/stub types so this crate stays pure and
//! wasm-safe: it declares only the transport shape, never the domain types
//! that fill it in. `git-vista-server` and `git-vista` (the frontend) each
//! declare their own concrete aliases — see `docs/superpowers/plans/
//! 2026-07-25-m1.10-paged-history-bounded-diff.md` lines 869-906.
//!
//! `HistoryFrame` carries no stubs — a page is a window into history and only
//! a page's rows can anchor a stub; `HistoryPage` carries `stubs` for exactly
//! that reason. `Page.lane_count` is the commit-lane high-water only (see
//! `FrameStub::lane_offset` in `git-vista-core`), not a count inclusive of
//! stub columns.

use serde::{Deserialize, Serialize};

use crate::plan::GenerationToken;

/// The cheap, once-per-view half of paged history: refs, branch colour slots,
/// and resolved-target/session metadata — never commit rows, edges, or stubs.
/// Generic over the ref type `R` so this crate carries no dependency on
/// `git-vista-core`; the server aliases `R` to `git_vista_core::GitRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFrame<R> {
    pub generation: GenerationToken,
    pub refs: Vec<R>,
    pub head_branch: Option<String>,
    pub branch_colors: Vec<(String, usize)>,
    pub repo_label: Option<String>,
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub read_only: bool,
    pub resettable: bool,
    pub repo_url: Option<String>,
    pub remote_web_url: Option<String>,
}

/// One cursor-paginated window of history rows/edges/stubs. Generic over the
/// row type `R`, the edge type `E`, and the stub type `S` so this crate stays
/// pure and wasm-safe; the server aliases these to `git_vista_core::{GraphRow,
/// Edge, FrameStub}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage<R, E, S> {
    pub rows: Vec<R>,
    pub edges: Vec<E>,
    pub stubs: Vec<S>,
    pub lane_count: usize,
    pub cursor: Option<String>,
    pub generation: GenerationToken,
}
