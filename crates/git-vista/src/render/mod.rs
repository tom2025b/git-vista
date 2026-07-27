//! The SVG graph builders — edges, commit dots, the per-node icons, the two
//! label tiers, and the branch stubs.
//!
//! Everything here turns one row/edge of the assembled history into SVG. The
//! builders are free functions (not closures) so `canvas.rs` can hand each one
//! to a virtualizing `<For>`; they read the history back out of a shared
//! [`RenderCtx`](crate::features::graph::core::RenderCtx) behind a `StoredValue`,
//! so a per-row closure never clones it. Spatial math lives in
//! [`crate::geometry`], colours in [`git_vista_core::color`]; this module is just
//! view assembly.
//!
//! Since M1.10 (#63) that `StoredValue` holds the **one mutable aggregate**:
//! rows, edges, stubs and per-row label geometry all come out of
//! [`LoadedHistory`](crate::features::graph::core::LoadedHistory), which grows as
//! pages land. There is no second copy — no `Graph`, no per-row colour vector, no
//! remote-commit set, no duplicate `text_x` — because a second copy would
//! silently go stale the first time a page appended.
//!
//! `RenderCtx` and `suppress` moved to `features::graph` in M1.11 (#64): a plain
//! data bundle and a DOM-only helper have no reason to live in the view-builder
//! module, and belong beside the state they concern.
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
//! Each builder is re-exported so `render::build_node`, `render::stubs`, … read
//! exactly as before.

mod edges;
mod labels;
mod nodes;
mod stubs;

pub use edges::{build_edge, visible_edges};
pub use labels::{build_meta, build_msg};
pub use nodes::{build_node, build_node_icon};
pub use stubs::{stub_icons, stubs};
