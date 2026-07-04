//! The SVG graph builders — edges, commit dots, the per-node icons, the two
//! label tiers, and the branch stubs.
//!
//! Everything here turns one row/edge of the laid-out [`Graph`] into SVG. The
//! builders are free functions (not closures) so `app.rs` can hand each one to a
//! virtualizing `<For>`; they read the graph and its derived lookups back out of
//! a shared [`RenderCtx`] behind a `StoredValue`, so a per-row closure never
//! clones the graph. Spatial math lives in [`crate::geometry`], colours in
//! [`git_vista_core::color`]; this module is just view assembly.
//!
//! # Module layout
//!
//! This split is move-only — the builders are grouped by what they draw:
//!
//!   * [`edges`]  — the visible-edge filter and the per-edge link builder.
//!   * [`nodes`]  — the commit dot (+ its tap menu) and the per-node glyph.
//!   * [`labels`] — the two label tiers: message (+ ref badges) and meta.
//!   * [`stubs`]  — the branch-stub lines, rings, menus, and their glyphs.
//!
//! [`RenderCtx`] (the shared per-render state) and [`suppress`] (the drag-vs-tap
//! link guard the label/stub builders share) stay here; each builder is
//! re-exported so `render::build_node`, `render::stubs`, … read exactly as before.

use std::collections::HashSet;

use leptos::*;

use git_vista_core::model::Graph;

mod edges;
mod labels;
mod nodes;
mod stubs;

pub use edges::{build_edge, visible_edges};
pub use labels::{build_meta, build_msg};
pub use nodes::{build_node, build_node_icon};
pub use stubs::{stub_icons, stubs};

/// Everything the per-row / per-edge view builders need, bundled behind a
/// `StoredValue` so the reactive `<For>` closures (Phase 8 viewport
/// virtualization) can reach the graph and its derived lookups cheaply — without
/// cloning the graph into each closure or rebuilding these tables per row.
pub struct RenderCtx {
    pub graph: Graph,
    /// Per-row branch-colour slot (row index → palette slot), so an edge can pick
    /// up the coloured line of the row it belongs to.
    pub row_color: Vec<usize>,
    /// Commit ids present on the remote, for the "is this pushed?" link gating.
    pub remote_set: HashSet<String>,
    /// Remote branch short-names, for gating local-branch links.
    pub remote_branches: HashSet<String>,
    /// GitHub web base (e.g. "https://github.com/owner/repo"), when this repo has
    /// a github.com origin; `None` => labels stay plain text.
    pub repo_url: Option<String>,
    /// Per-row left edge (x) of the label text, hugging the graph — indexed by
    /// row number (see [`crate::geometry::label_x_per_row`]).
    pub text_x: Vec<i32>,
}

/// Cancel a link's navigation only when the "click" is actually the tail of a
/// drag/pan (desktop). Links are real SVG `<a target="_blank">` anchors, so a tap
/// is native link navigation — which works on iOS WebKit, where the scripted
/// `window.open` pop-ups we used before are silently blocked. `moved` is the
/// gesture's drag flag (set in pointermove).
pub fn suppress(moved: StoredValue<bool>, ev: web_sys::MouseEvent) {
    if moved.get_value() {
        ev.prevent_default();
    }
}
