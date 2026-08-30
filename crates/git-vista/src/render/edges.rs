//! The edge layer: which edges are on screen, and the per-edge link builder.
//!
//! Split out of `render.rs`. An edge is the curved line from a commit to a
//! parent; [`visible_edges`] narrows the set to those crossing the viewport and
//! [`build_edge`] draws one, coloured by the branch line it belongs to.

use leptos::*;

use crate::geometry::edge_path;
use git_vista_core::color::branch_color;
use git_vista_core::model::Edge;

use crate::features::graph::collapse::{DisplayItem, DisplayProjection};
use crate::features::graph::core::RenderCtx;

/// Indices of display edges whose row span intersects the visible display-row
/// window `[start, end)`. Same rule as before collapsing (#374): an edge is
/// kept whenever any part of it could cross the viewport, so a long line
/// passing through never blinks out at the window's edge.
///
/// The span comes from [`DisplayEdge::span`] rather than being read off the
/// endpoints in order. Raw edges always run downward, but a display edge no
/// longer must: with two interleaved chains folded (#478), an edge into a
/// fork point that folded into the *upper* chain's marker points back up the
/// screen. Comparing `from`/`to` positionally culled those wherever they were.
pub fn visible_edges(display: StoredValue<DisplayProjection>, range: (usize, usize)) -> Vec<usize> {
    let (start, end) = range;
    display.with_value(|d| {
        d.edges
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let (top, bottom) = e.span();
                top < end && bottom >= start
            })
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
///
/// A group takes its anchor member's identity for this colour lookup (#374).
/// The collapse projection likewise moves a folded endpoint onto the anchor's
/// marker lane, so colour and position now resolve the same visible object
/// (ADR 0098).
pub fn build_edge(
    ctx: StoredValue<RenderCtx>,
    display: StoredValue<DisplayProjection>,
    ei: usize,
) -> View {
    let Some(de) = display.with_value(|d| d.edges.get(ei).copied()) else {
        return ().into_view();
    };
    let (Some(from_item), Some(to_item)) = display.with_value(|d| {
        (
            d.items.get(de.from_display).copied(),
            d.items.get(de.to_display).copied(),
        )
    }) else {
        return ().into_view();
    };
    ctx.with_value(|c| {
        let rows = &c.loaded.rows;
        let row_of = |item: DisplayItem| match item {
            DisplayItem::Single { row_index } => rows.get(row_index),
            DisplayItem::WipGroup {
                anchor_row_index, ..
            } => rows.get(anchor_row_index),
        };
        let (Some(from), Some(to)) = (row_of(from_item), row_of(to_item)) else {
            return ().into_view();
        };
        let d = edge_path(&Edge {
            from_row: de.from_display,
            from_lane: de.from_lane,
            to_row: de.to_display,
            to_lane: de.to_lane,
        });
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
