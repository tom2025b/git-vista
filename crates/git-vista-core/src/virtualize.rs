//! The virtualization primitive (M2.16, #69c): item heights plus scroll
//! offset in, the render range out.
//!
//! Pure logic, **knowing nothing about what an item represents** — a diff
//! line (#69a), a commit-graph row, a status entry, anything with a height.
//! That's the whole point: the commit graph could reuse this exact type
//! instead of #69's diff viewer inventing a second one. No rendering, no
//! Leptos, no DOM — just the math a virtualized/windowed list needs to decide
//! which item indices to actually render.
//!
//! Lives in `git-vista-core`, not `git-vista-protocol`: this never crosses
//! the wire. It's local rendering math, recomputed client-side from whatever
//! list the client already holds, not part of the HTTP/JSON contract.
//!
//! ## Shape and complexity
//!
//! [`CumulativeHeights`] precomputes prefix sums once, in `O(n)`, when the
//! item list is built or changes. [`CumulativeHeights::visible_range`] then
//! answers each scroll event in `O(log n)` via binary search
//! ([`slice::partition_point`]) over that precomputed array — not a linear
//! rescan of every item on every scroll frame, which would defeat the
//! purpose virtualization exists for in the first place. This two-phase
//! shape (build once, query many times per rebuild) matches how a real
//! virtualized list is actually used: the item list changes far less often
//! than the user scrolls.

/// The result of one [`CumulativeHeights::visible_range`] query: which items
/// to render, and where the first one starts, so the caller can position the
/// rendered block with absolute offsets instead of re-measuring anything.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleRange {
    /// First item index to render (inclusive).
    pub start: usize,
    /// One past the last item index to render (exclusive) — `start..end` is
    /// a normal Rust range, always usable directly to slice the item list.
    pub end: usize,
    /// The pixel (or whatever unit the caller's heights are in) offset the
    /// `start` item begins at, relative to the top of the full list.
    pub start_offset: f64,
}

impl VisibleRange {
    fn empty() -> Self {
        VisibleRange {
            start: 0,
            end: 0,
            start_offset: 0.0,
        }
    }

    /// Number of items in the range.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Precomputed prefix sums over a list of item heights — the one-time `O(n)`
/// cost that makes every later [`visible_range`](Self::visible_range) query
/// `O(log n)` instead of `O(n)`. Rebuild when the item list changes; reuse
/// across every scroll event in between.
#[derive(Debug, Clone)]
pub struct CumulativeHeights {
    /// `prefix[i]` = sum of `item_heights[0..i]`. Length is `n + 1` for `n`
    /// items: `prefix[0] == 0.0`, `prefix[n] == total_height()`. Monotonic
    /// non-decreasing, not strictly increasing — a zero-height item leaves
    /// two consecutive entries equal, which every method here handles
    /// correctly (see the module's tests).
    prefix: Vec<f64>,
}

impl CumulativeHeights {
    /// Build the prefix-sum table from `item_heights`. Negative heights are
    /// clamped to `0.0` rather than trusted — a negative height has no
    /// sensible visual meaning, and trusting one would make the prefix array
    /// non-monotonic, breaking every binary search below.
    pub fn new(item_heights: &[f64]) -> Self {
        let mut prefix = Vec::with_capacity(item_heights.len() + 1);
        prefix.push(0.0);
        let mut running = 0.0;
        for &h in item_heights {
            running += h.max(0.0);
            prefix.push(running);
        }
        CumulativeHeights { prefix }
    }

    /// Number of items this table was built from.
    pub fn item_count(&self) -> usize {
        self.prefix.len() - 1
    }

    /// Total height of every item combined.
    pub fn total_height(&self) -> f64 {
        *self.prefix.last().unwrap_or(&0.0)
    }

    /// The pixel offset item `index` starts at. Panics if `index >
    /// item_count()` (matching ordinary slice-index semantics — `index ==
    /// item_count()` is the valid "one past the end" position, the total
    /// height).
    pub fn offset_of(&self, index: usize) -> f64 {
        self.prefix[index]
    }

    /// Which item indices should be rendered for a viewport of
    /// `viewport_height` scrolled to `scroll_offset`, padded by `overscan`
    /// extra items on each side so a fast scroll doesn't flash blank space
    /// for a frame before the next range is computed.
    ///
    /// **Clamping "an offset past the end."** `scroll_offset` is clamped into
    /// `[0, max(0, total_height - viewport_height)]` before anything else —
    /// the same behaviour a real scroll container has: you cannot actually
    /// scroll past your own content, so a stale or out-of-range
    /// `scroll_offset` (e.g. the list shrank out from under a previously
    /// valid scroll position) is treated as "scrolled all the way to the
    /// bottom," not as "show nothing." An empty list, or `viewport_height >=
    /// total_height`, clamp to `0` — the only sensible scroll position when
    /// everything already fits.
    pub fn visible_range(
        &self,
        viewport_height: f64,
        scroll_offset: f64,
        overscan: usize,
    ) -> VisibleRange {
        let n = self.item_count();
        if n == 0 {
            return VisibleRange::empty();
        }

        let max_scroll = (self.total_height() - viewport_height).max(0.0);
        let scroll_offset = scroll_offset.clamp(0.0, max_scroll);
        let viewport_bottom = scroll_offset + viewport_height;

        // First item whose end-offset (prefix[i+1]) is past the scroll
        // position — i.e. the first item at least partially visible.
        // An item is visible if its end-offset is at or past the viewport's
        // top edge — `>=`, not strictly `>`, so a zero-height item sitting
        // exactly at `scroll_offset` still counts as visible instead of
        // being skipped for having "already ended" at the boundary.
        // Searching `prefix[1..]` (the n item end-offsets) for the first
        // entry that is NOT strictly before `scroll_offset` gives that
        // item's index directly.
        let start_visible = self.prefix[1..].partition_point(|&h| h < scroll_offset);

        // Number of items whose start-offset (prefix[i], i in 0..n) is
        // strictly before the viewport's bottom edge — i.e. how many items
        // are at least partially visible from the top, which is exactly the
        // exclusive end of the visible range.
        let end_visible = self.prefix[..n].partition_point(|&h| h < viewport_bottom);

        let start = start_visible.saturating_sub(overscan);
        let end = (end_visible + overscan).min(n);

        VisibleRange {
            start,
            end,
            start_offset: self.prefix[start],
        }
    }
}

/// Convenience over [`CumulativeHeights`] for a one-off query — builds the
/// table and queries it once. Prefer building a [`CumulativeHeights`]
/// directly and reusing it across scroll events when the item list is
/// stable between calls, which is the normal case; this exists for the
/// occasional caller (or a test) that only needs a single answer.
pub fn visible_range(
    item_heights: &[f64],
    viewport_height: f64,
    scroll_offset: f64,
    overscan: usize,
) -> VisibleRange {
    CumulativeHeights::new(item_heights).visible_range(viewport_height, scroll_offset, overscan)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary case, as a positive control for everything else: a list
    /// taller than the viewport, scrolled partway through, no overscan.
    #[test]
    fn ordinary_scroll_returns_the_partially_and_fully_visible_items() {
        // 10 items, each 20 tall (0..20, 20..40, ..., 180..200), viewport 50.
        let heights = vec![20.0; 10];
        let table = CumulativeHeights::new(&heights);
        assert_eq!(table.total_height(), 200.0);

        // Scrolled to 45: item 2 (40..60) is partially visible from the
        // top, item 4 (80..100) partially visible at the bottom edge
        // (viewport is 45..95).
        let range = table.visible_range(50.0, 45.0, 0);
        assert_eq!(range.start, 2);
        assert_eq!(range.end, 5); // items 2, 3, 4
        assert_eq!(range.start_offset, 40.0);
    }

    #[test]
    fn overscan_pads_both_sides_and_clamps_at_the_list_boundaries() {
        let heights = vec![20.0; 10];
        let table = CumulativeHeights::new(&heights);
        let range = table.visible_range(50.0, 45.0, 2);
        // Without overscan: 2..5. With overscan 2: 0..7, clamped at 0.
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 7);
    }

    #[test]
    fn fewer_items_than_fit_the_viewport_returns_the_whole_list() {
        let heights = vec![10.0, 20.0, 15.0]; // total 45
        let table = CumulativeHeights::new(&heights);
        let range = table.visible_range(200.0, 0.0, 0);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 3);
        assert_eq!(range.start_offset, 0.0);

        // Even a nonsensical positive scroll offset clamps to 0 (max_scroll
        // is 0 when everything already fits), not an out-of-range slice.
        let range = table.visible_range(200.0, 999.0, 0);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 3);
    }

    #[test]
    fn offset_past_the_end_clamps_to_the_bottom_not_an_empty_range() {
        let heights = vec![20.0; 10]; // total 200
        let table = CumulativeHeights::new(&heights);
        // Viewport 50, scrolled to 10_000 (way past the 150 max_scroll).
        let range = table.visible_range(50.0, 10_000.0, 0);
        assert_eq!(range.start, 7); // items 7, 8, 9 fill 140..200
        assert_eq!(range.end, 10);
        assert_eq!(range.start_offset, 140.0);
    }

    #[test]
    fn zero_height_items_do_not_break_the_search_and_still_count_as_items() {
        // Items 1 and 3 are zero-height (e.g. collapsed sections).
        let heights = vec![20.0, 0.0, 20.0, 0.0, 20.0];
        let table = CumulativeHeights::new(&heights);
        assert_eq!(table.total_height(), 60.0);
        assert_eq!(table.item_count(), 5);

        // A viewport covering everything must include the zero-height items
        // too, not silently drop them because they contribute no height.
        let range = table.visible_range(100.0, 0.0, 0);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 5);

        // Scrolled exactly to the boundary between item 0 and item 1 (both
        // at offset 20.0): item 1 (zero-height, starts and ends at 20.0)
        // must not be skipped just because its own span is empty.
        let range = table.visible_range(20.0, 20.0, 0);
        assert!(
            (range.start..range.end).contains(&1),
            "zero-height item 1 dropped from the range: {range:?}"
        );
    }

    #[test]
    fn empty_item_list_returns_an_empty_range_not_a_panic() {
        let table = CumulativeHeights::new(&[]);
        assert_eq!(table.total_height(), 0.0);
        assert_eq!(table.item_count(), 0);
        let range = table.visible_range(100.0, 0.0, 5);
        assert!(range.is_empty());
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 0);
    }

    #[test]
    fn all_zero_height_items_do_not_infinite_loop_or_divide_by_zero() {
        let heights = vec![0.0; 1000];
        let table = CumulativeHeights::new(&heights);
        assert_eq!(table.total_height(), 0.0);
        let range = table.visible_range(50.0, 0.0, 0);
        // Everything "fits" in any nonzero viewport since total height is 0.
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 1000);
    }

    #[test]
    fn negative_heights_are_clamped_not_trusted() {
        let table = CumulativeHeights::new(&[10.0, -5.0, 10.0]);
        assert_eq!(table.total_height(), 20.0);
        assert_eq!(table.offset_of(1), 10.0);
        assert_eq!(table.offset_of(2), 10.0); // the negative item contributed 0
    }

    #[test]
    fn free_function_matches_building_the_table_directly() {
        let heights = vec![20.0; 10];
        let via_free_fn = visible_range(&heights, 50.0, 45.0, 1);
        let via_table = CumulativeHeights::new(&heights).visible_range(50.0, 45.0, 1);
        assert_eq!(via_free_fn, via_table);
    }
}
