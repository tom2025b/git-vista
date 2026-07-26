//! The edge layer: which edges are on screen, and the per-edge link builder.
//!
//! Split out of `render.rs`. An edge is the curved line from a commit to a
//! parent; [`visible_edges`] narrows the set to those crossing the viewport and
//! [`build_edge`] draws one, coloured by the branch line it belongs to.

use leptos::*;

use crate::geometry::edge_path;
use git_vista_core::color::branch_color;

use super::RenderCtx;

/// Indices of edges whose row span intersects the visible row window `[start,
/// end)`. An edge is kept whenever any part of it could cross the viewport — even
/// when both endpoints are off-screen (a long merge line passing through) — so an
/// edge never visibly disappears at the window's edge. Edges always run downward
/// (`from_row` < `to_row`), so the span is `[from_row, to_row]`.
pub fn visible_edges(ctx: StoredValue<RenderCtx>, range: (usize, usize)) -> Vec<usize> {
    let (start, end) = range;
    ctx.with_value(|c| {
        c.loaded
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.from_row < end && e.to_row >= start)
            .map(|(i, _)| i)
            .collect()
    })
}

/// Per-edge view builder — invoked by a `<For>` only for edges whose row span
/// intersects the viewport. Colour a link by the branch *line* it belongs to,
/// so it matches the dots it connects:
///  * a first-parent link is part of the child's own branch — a side branch
///    forking off main is drawn in the side branch's colour all the way down to
///    its fork point, not main's blue;
///  * a merge link (any non-first parent) is part of the merged-in branch, so
///    it takes that parent's colour as it curves in.
///
/// Only main (colour slot 0) ever stays blue this way.
///
/// Every index is *checked* (M1.10, #63). With paged history an edge index and a
/// row index no longer come from the same snapshot: a `<For>` can still be
/// holding an index built a moment before the aggregate changed shape. Panicking
/// on that would take the whole canvas down, so an edge whose endpoints aren't
/// both loaded simply draws nothing until the page owning them lands.
pub fn build_edge(ctx: StoredValue<RenderCtx>, ei: usize) -> View {
    ctx.with_value(|c| {
        let rows = &c.loaded.rows;
        let Some(e) = c.loaded.edges.get(ei) else {
            return ().into_view();
        };
        let (Some(from), Some(to)) = (rows.get(e.from_row), rows.get(e.to_row)) else {
            return ().into_view();
        };
        let d = edge_path(e);
        let is_first_parent = from.commit.parents.first() == Some(&to.commit.id);
        // A first-parent link belongs to the child's own branch; a merge link to
        // the merged-in parent's — so each takes that row's colour slot.
        let color = branch_color(if is_first_parent {
            from.color
        } else {
            to.color
        });
        view! {
            <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
        }
        .into_view()
    })
}
