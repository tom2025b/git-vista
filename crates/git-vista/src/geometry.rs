//! Pure layout geometry for the vertical commit graph.
//!
//! Everything in here maps abstract graph coordinates — a commit's `(row, lane)`
//! — onto concrete SVG user units, with no Leptos/DOM dependency. Splitting it
//! out of the [`crate::app`] component keeps that file about *view assembly* and
//! lets the spatial math be reasoned about (and unit-tested) on its own. Colours
//! live separately in [`crate::color`].
//!
//! All values are whole numbers so the emitted SVG attributes stay clean.

use git_vista_core::model::Edge;

// Geometry of the graph, in SVG user units (px).
pub const ROW_HEIGHT: i32 = 56; // vertical gap between commits
pub const LANE_WIDTH: i32 = 34; // horizontal gap between lanes
// Used only by the wasm-only `app` view, so it reads as dead on host/test builds.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const NODE_RADIUS: i32 = 7;
pub const PAD_X: i32 = 28;
pub const PAD_Y: i32 = 28;
// Horizontal gap between the rightmost lane and the start of the label column.
pub const LABEL_GAP: i32 = 18;

/// Centre x of a node in the given lane.
pub fn node_cx(lane: usize) -> i32 {
    PAD_X + lane as i32 * LANE_WIDTH
}

/// Centre y of a node in the given row.
pub fn node_cy(row: usize) -> i32 {
    PAD_Y + row as i32 * ROW_HEIGHT
}

/// Left edge (x) of the commit-label column: a fixed column just to the right of
/// the widest lane, so every row's text is aligned regardless of its own lane.
pub fn label_x(lane_count: usize) -> i32 {
    // `lane_count` lanes occupy indices 0..lane_count; sit past the last one.
    node_cx(lane_count.saturating_sub(1)) + LABEL_GAP
}

/// Baseline y of a row's first (message) label line — just above the node's
/// centre, so the two-line label straddles the node.
pub fn label_top_y(row: usize) -> i32 {
    node_cy(row) - 3
}

/// Baseline y of a row's second (hash · author) label line — just below centre.
pub fn label_bottom_y(row: usize) -> i32 {
    node_cy(row) + 12
}

/// SVG path data for a commit->parent edge. Same-lane links are a straight
/// vertical line; lane-changing links (branches/merges) get a smooth vertical
/// S-curve so they read as flowing between columns rather than cutting across.
pub fn edge_path(e: &Edge) -> String {
    let (x1, y1) = (node_cx(e.from_lane), node_cy(e.from_row));
    let (x2, y2) = (node_cx(e.to_lane), node_cy(e.to_row));
    if x1 == x2 {
        format!("M {x1} {y1} L {x2} {y2}")
    } else {
        let ym = (y1 + y2) / 2;
        format!("M {x1} {y1} C {x1} {ym}, {x2} {ym}, {x2} {y2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_centres_step_by_the_configured_gaps() {
        assert_eq!(node_cx(0), PAD_X);
        assert_eq!(node_cx(2), PAD_X + 2 * LANE_WIDTH);
        assert_eq!(node_cy(0), PAD_Y);
        assert_eq!(node_cy(3), PAD_Y + 3 * ROW_HEIGHT);
    }

    #[test]
    fn label_column_sits_past_the_widest_lane_and_rows_straddle_nodes() {
        // One lane → column just right of lane 0; three lanes → right of lane 2.
        assert_eq!(label_x(1), node_cx(0) + LABEL_GAP);
        assert_eq!(label_x(3), node_cx(2) + LABEL_GAP);
        // The two text baselines bracket the node centre.
        assert!(label_top_y(2) < node_cy(2));
        assert!(label_bottom_y(2) > node_cy(2));
    }

    #[test]
    fn same_lane_edges_are_straight_others_curve() {
        let straight = Edge { from_row: 0, from_lane: 0, to_row: 1, to_lane: 0 };
        assert_eq!(
            edge_path(&straight),
            format!("M {} {} L {} {}", node_cx(0), node_cy(0), node_cx(0), node_cy(1))
        );

        let curved = Edge { from_row: 0, from_lane: 0, to_row: 1, to_lane: 1 };
        let d = edge_path(&curved);
        assert!(d.starts_with('M'), "starts with a move");
        assert!(d.contains(" C "), "lane-changing edge is a cubic curve");
    }
}
