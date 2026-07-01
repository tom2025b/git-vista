//! Pure viewport math for row virtualization (Phase 8).
//!
//! The graph can hold thousands of commits, but only a screenful of rows is ever
//! visible. Rendering every row's SVG is wasteful, so the view asks this module
//! which rows fall inside the current viewport and renders only those. Like
//! [`crate::camera`] and [`crate::geometry`], it's DOM-free and host-tested — the
//! view layer just feeds it the camera + viewport height and renders the range.
//!
//! Coordinate model (shared with [`crate::camera`]): the SVG has no `viewBox`, so
//! one user unit is one CSS px and a content point `c` lands on screen at
//! `t + scale * c`. A row `r` sits at world-y [`crate::geometry::node_cy`]`(r)`.

use crate::camera::Camera;
use crate::geometry::{PAD_Y, ROW_HEIGHT};

/// The half-open range `[start, end)` of row indices whose nodes fall inside a
/// `viewport_h`-tall viewport under `cam`, padded by `overscan` rows on each side
/// and clamped to `0..row_count`.
///
/// `overscan` renders a few rows beyond the visible edge so a fast pan doesn't
/// flash a blank strip before the caller recomputes the range; it also absorbs
/// the small vertical extent of a row's badges/meta line, which reach a little
/// above and below the node centre.
///
/// The returned range is always ordered (`start <= end`) and in bounds, so the
/// caller can iterate it directly.
pub fn visible_row_range(
    cam: Camera,
    viewport_h: f64,
    row_count: usize,
    overscan: usize,
) -> (usize, usize) {
    if row_count == 0 || cam.scale <= 0.0 {
        return (0, 0);
    }
    // The visible screen band [0, viewport_h] maps back to this world-y interval.
    let world_top = (0.0 - cam.ty) / cam.scale;
    let world_bottom = (viewport_h - cam.ty) / cam.scale;
    // Invert node_cy(r) = PAD_Y + r * ROW_HEIGHT to get the row at a world-y, then
    // widen by the overscan. `floor`/`ceil` so a partially-visible row still counts.
    let first = ((world_top - PAD_Y as f64) / ROW_HEIGHT as f64).floor() as i64 - overscan as i64;
    let last = ((world_bottom - PAD_Y as f64) / ROW_HEIGHT as f64).ceil() as i64 + overscan as i64;
    let n = row_count as i64;
    let start = first.clamp(0, n) as usize;
    let end = (last + 1).clamp(0, n) as usize; // +1: half-open, includes `last`
    (start, end.max(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Row count comfortably larger than any window used in the tests.
    const ROWS: usize = 1000;

    #[test]
    fn empty_graph_has_an_empty_range() {
        assert_eq!(visible_row_range(Camera::default(), 800.0, 0, 6), (0, 0));
    }

    #[test]
    fn the_default_view_starts_at_the_top_and_covers_a_screenful() {
        let (start, end) = visible_row_range(Camera::default(), 800.0, ROWS, 6);
        assert_eq!(start, 0, "unpanned view begins at the first row");
        // An 800px window at 56px/row shows ~14 rows; end must cover them and stay
        // in bounds.
        assert!(end >= 14, "covers the visible rows, got {end}");
        assert!(end <= ROWS);
        assert!(start <= end);
    }

    #[test]
    fn panning_up_scrolls_the_top_rows_out_of_the_window() {
        // Content shifted up by 1000px (ty negative) pushes the first rows above
        // the viewport, so the range should start well past row 0.
        let cam = Camera { tx: 0.0, ty: -1000.0, scale: 1.0 };
        let (start, end) = visible_row_range(cam, 800.0, ROWS, 6);
        assert!(start > 0, "top rows scrolled off, start = {start}");
        assert!(end > start);
        assert!(end <= ROWS);
    }

    #[test]
    fn zooming_out_widens_the_row_window() {
        let vh = 800.0;
        let full = visible_row_range(Camera::default(), vh, ROWS, 6);
        let out = visible_row_range(Camera { scale: 0.3, ..Camera::default() }, vh, ROWS, 6);
        let span = |(s, e): (usize, usize)| e - s;
        assert!(
            span(out) > span(full),
            "zoomed out shows more rows: {} vs {}",
            span(out),
            span(full)
        );
    }

    #[test]
    fn the_range_is_always_clamped_and_ordered() {
        // Panned far past the end of the graph: nothing is visible, but the range
        // stays in bounds and ordered rather than going negative or past the end.
        let cam = Camera { tx: 0.0, ty: -1.0e6, scale: 1.0 };
        let (start, end) = visible_row_range(cam, 800.0, ROWS, 6);
        assert!(start <= end);
        assert!(end <= ROWS);
        assert_eq!((start, end), (ROWS, ROWS));
    }

    #[test]
    fn overscan_grows_the_window_on_both_sides() {
        // Mid-graph so both edges have room to expand (not clamped at 0).
        let cam = Camera { tx: 0.0, ty: -1000.0, scale: 1.0 };
        let (s0, e0) = visible_row_range(cam, 800.0, ROWS, 0);
        let (s6, e6) = visible_row_range(cam, 800.0, ROWS, 6);
        assert_eq!(s0 - s6, 6, "overscan extends the top by exactly its size");
        assert_eq!(e6 - e0, 6, "overscan extends the bottom by exactly its size");
    }
}
