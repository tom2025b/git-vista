//! The SVG graph builders — edges, commit dots, the per-node icons, the two
//! label tiers, and the branch stubs.
//!
//! Everything here turns one row/edge of the assembled history into SVG. The
//! builders are free functions (not closures) so `canvas.rs` can hand each one
//! to a virtualizing `<For>`; they read the history back out of a shared
//! [`RenderCtx`] behind a `StoredValue`, so a per-row closure never clones it.
//! Spatial math lives in [`crate::geometry`], colours in
//! [`git_vista_core::color`]; this module is just view assembly.
//!
//! Since M1.10 (#63) that `StoredValue` holds the **one mutable aggregate**:
//! rows, edges, stubs and per-row label geometry all come out of
//! [`crate::history::LoadedHistory`], which grows as pages land. There is no
//! second copy — no `Graph`, no per-row colour vector, no remote-commit set, no
//! duplicate `text_x` — because a second copy would silently go stale the first
//! time a page appended.
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

use crate::history::{Frame, LoadedHistory};

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
/// virtualization) can reach it cheaply — without cloning history into each
/// closure or rebuilding lookups per row.
///
/// This is the mounted canvas's **single owner** of history (M1.10, #63): the
/// once-per-view [`Frame`] and the growing [`LoadedHistory`]. Appends mutate
/// `loaded` in place through `StoredValue::try_update_value`, and the epoch
/// signals in `canvas.rs` are what tell the view which parts to repaint. The
/// only derived table kept here is `remote_branches`, and only because it is a
/// property of the Frame — it can't drift as pages land.
pub struct RenderCtx {
    /// The reload epoch this canvas was mounted for. A page reply carrying any
    /// other epoch belongs to a retired view and is dropped.
    pub epoch: u32,
    /// Refs, colours and every scrap of repo metadata — read once per view, and
    /// the *only* source of it now that paged rows carry none.
    pub frame: Frame,
    /// Every page accepted so far, as one graph: rows, edges, stubs, the cursor,
    /// and the monotonic per-row label geometry.
    pub loaded: LoadedHistory,
    /// Remote branch short-names (the part after the `<remote>/` prefix),
    /// derived once from [`Frame::refs`] — a local branch links out only when a
    /// remote branch shares its name. Derived from the Frame, never from the
    /// loaded rows: with paging, whichever rows happen to be loaded say nothing
    /// about which branches exist on the remote.
    pub remote_branches: HashSet<String>,
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
