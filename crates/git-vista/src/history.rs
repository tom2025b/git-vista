//! The frontend's paged-history aggregate and its request state (M1.10, #63).
//!
//! History arrives as a cheap once-per-view [`Frame`] plus a stream of
//! cursor-paginated [`Page`]s, so the browser now *assembles* the graph instead
//! of receiving it whole. That assembly is the risky part: a page that starts at
//! the wrong row, repeats an OID, or re-delivers an edge the prefix already owns
//! would silently corrupt a graph the user is reading. So this module owns one
//! mutable aggregate ([`LoadedHistory`]) and one validate-then-commit path — every
//! check runs in temporary values, and a page that fails any of them leaves the
//! aggregate byte-for-byte untouched.
//!
//! It's deliberately DOM-free and host-compiled, like [`crate::camera`],
//! [`crate::geometry`] and [`crate::viewport`]: the invariants are the whole
//! point, so they're unit-tested on the host rather than only exercised in a
//! browser.

use std::collections::{HashMap, HashSet};
use std::fmt;

use git_vista_core::model::{Edge, FrameStub, GitRef, GraphRow, Oid};
use git_vista_protocol::{GenerationToken, HistoryFrame, HistoryPage};

use crate::geometry::{node_cx, LABEL_GAP, ROW_HEIGHT};

/// The frontend's concrete half of the generic wire envelopes. The protocol crate
/// stays free of domain types, so *this* crate names the row/edge/stub/ref types
/// that fill them in — the server declares its own, identical aliases.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub type Frame = HistoryFrame<GitRef>;
pub type Page = HistoryPage<GraphRow, Edge, FrameStub>;

/// Rows asked for per page. Big enough that a first screenful never needs a
/// second round trip, small enough that page 1 is cheap on a huge repository.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const DEFAULT_PAGE_LIMIT: usize = 250;

/// Hard ceiling on rows the culler may hand the renderer at once. A pathological
/// camera (zoomed far out over a loaded history) would otherwise ask for every
/// row it can see, and the SVG node count — not the aggregate — is what stalls
/// the browser.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const MAX_LIVE_ROWS: usize = 2_000;

/// How far past the last visible row the camera fetches ahead, in viewports.
/// Enough that a steady scroll never reaches an empty strip, few enough that
/// idling near the bottom doesn't drag the whole repository in.
pub const PREFETCH_VIEWPORTS: f64 = 1.5;

/// Why a delivered page was refused. Every variant carries the offending values
/// rather than a message, so the UI can show what actually went wrong and the
/// tests can assert on the exact fault instead of on prose.
///
/// A page that produces any of these is dropped whole: see
/// [`LoadedHistory::append_page`] for the validate-then-commit rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryInvariantError {
    GenerationMismatch {
        expected: GenerationToken,
        actual: GenerationToken,
    },
    CursorChanged {
        requested: String,
        current: Option<String>,
    },
    NonContiguousRow {
        expected: usize,
        actual: usize,
    },
    DuplicateOid(Oid),
    DuplicateEdge {
        from_row: usize,
        from_lane: usize,
        to_row: usize,
        to_lane: usize,
    },
    NonForwardEdge {
        from_row: usize,
        to_row: usize,
    },
    EdgeDestinationOutsidePage {
        page_start: usize,
        page_end: usize,
        to_row: usize,
    },
    LaneHighWaterRegressed {
        previous: usize,
        actual: usize,
    },
}

impl fmt::Display for HistoryInvariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenerationMismatch { expected, actual } => write!(
                f,
                "history moved while loading (expected generation {expected}, page carried {actual})"
            ),
            Self::CursorChanged { requested, current } => write!(
                f,
                "history page answered cursor {requested} but the graph is at {}",
                current.as_deref().unwrap_or("no cursor")
            ),
            Self::NonContiguousRow { expected, actual } => {
                write!(f, "history page delivered row {actual}, expected {expected}")
            }
            Self::DuplicateOid(oid) => {
                write!(f, "history page repeats commit {}", oid.short())
            }
            Self::DuplicateEdge {
                from_row,
                from_lane,
                to_row,
                to_lane,
            } => write!(
                f,
                "history page repeats edge ({from_row},{from_lane}) -> ({to_row},{to_lane})"
            ),
            Self::NonForwardEdge { from_row, to_row } => write!(
                f,
                "history page edge {from_row} -> {to_row} does not run forward"
            ),
            Self::EdgeDestinationOutsidePage {
                page_start,
                page_end,
                to_row,
            } => write!(
                f,
                "history page edge ends at row {to_row}, outside its own rows {page_start}..{page_end}"
            ),
            Self::LaneHighWaterRegressed { previous, actual } => write!(
                f,
                "history page claims {actual} lanes, fewer than the {previous} already drawn"
            ),
        }
    }
}

/// A [`FrameStub`] placed on the canvas: its anchor found by commit id, and its
/// lane resolved against the commit-lane high-water of everything loaded so far.
///
/// A page anchors its stubs by OID precisely because it doesn't know the absolute
/// row or the final lane count — both are properties of the whole aggregate, so
/// they are computed here and recomputed after every append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStub {
    pub stub: FrameStub,
    pub anchor_row: usize,
    pub anchor_lane: usize,
    pub lane: usize,
}

/// Every page accepted so far, as one graph.
///
/// This is the frontend's single mutable history aggregate. `label_occupancy` and
/// `text_x` are private on purpose: they are *monotonic* — an append may push a
/// row's label further right but never pull it back — and that guarantee only
/// holds while this module is the sole writer. Read them through
/// [`Self::label_occupancy`]/[`Self::text_x`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedHistory {
    pub rows: Vec<GraphRow>,
    pub edges: Vec<Edge>,
    pub stubs: Vec<FrameStub>,
    pub lane_high_water: usize,
    pub cursor: Option<String>,
    pub generation: GenerationToken,
    pub oid_to_row: HashMap<Oid, usize>,
    label_occupancy: Vec<usize>,
    text_x: Vec<i32>,
}

/// What an accepted page changed, so the view can repaint the minimum: the rows
/// that existed before it, whether any of *those* labels moved, and whether the
/// resolved stubs did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendDelta {
    pub old_row_count: usize,
    pub prefix_geometry_changed: bool,
    pub stub_geometry_changed: bool,
}

impl LoadedHistory {
    /// Seed the aggregate from page 1. The caller has already proved the Frame and
    /// the page carry the same generation, so the shared validator's generation
    /// check is a tautology here — what it still enforces is that page 1 really
    /// starts at absolute row zero.
    pub fn from_first_page(page: Page) -> Result<Self, HistoryInvariantError> {
        let mut history = LoadedHistory {
            rows: Vec::new(),
            edges: Vec::new(),
            stubs: Vec::new(),
            lane_high_water: 0,
            cursor: None,
            generation: page.generation.clone(),
            oid_to_row: HashMap::new(),
            label_occupancy: Vec::new(),
            text_x: Vec::new(),
        };
        history.apply_page(None, page)?;
        Ok(history)
    }

    /// Append the page fetched with `requested_cursor`, or refuse it whole.
    ///
    /// `requested_cursor` is the cursor the *HTTP request* carried, not one read
    /// off the response: comparing it with the aggregate's own cursor is what
    /// catches a reply that arrives after the graph has moved on.
    pub fn append_page(
        &mut self,
        requested_cursor: &str,
        page: Page,
    ) -> Result<AppendDelta, HistoryInvariantError> {
        self.apply_page(Some(requested_cursor), page)
    }

    /// The one row/index/edge/geometry path both entry points share.
    ///
    /// Everything above the "commit" marker runs in temporaries: on any error
    /// `self` is returned untouched, down to the private occupancy vectors. That
    /// is the whole contract — a bad page can never half-apply into a graph the
    /// user is reading.
    fn apply_page(
        &mut self,
        requested_cursor: Option<&str>,
        page: Page,
    ) -> Result<AppendDelta, HistoryInvariantError> {
        // ---- validation (temporaries only; `self` is still untouched) --------
        if page.generation != self.generation {
            return Err(HistoryInvariantError::GenerationMismatch {
                expected: self.generation.clone(),
                actual: page.generation,
            });
        }
        if let Some(requested) = requested_cursor {
            if self.cursor.as_deref() != Some(requested) {
                return Err(HistoryInvariantError::CursorChanged {
                    requested: requested.to_owned(),
                    current: self.cursor.clone(),
                });
            }
        }

        let page_start = self.rows.len();
        let page_end = page_start + page.rows.len();

        // Exact contiguity, so page 1 starts at zero and no page can open a gap
        // or reorder rows under the edges that already point at them.
        for (offset, row) in page.rows.iter().enumerate() {
            let expected = page_start + offset;
            if row.row != expected {
                return Err(HistoryInvariantError::NonContiguousRow {
                    expected,
                    actual: row.row,
                });
            }
        }

        // Unique across the aggregate *and* within the candidate page — a repeat
        // would otherwise silently rewrite an OID's row in the index.
        let mut new_oids: HashMap<Oid, usize> = HashMap::with_capacity(page.rows.len());
        for (offset, row) in page.rows.iter().enumerate() {
            let oid = &row.commit.id;
            if self.oid_to_row.contains_key(oid) || new_oids.contains_key(oid) {
                return Err(HistoryInvariantError::DuplicateOid(oid.clone()));
            }
            new_oids.insert(oid.clone(), page_start + offset);
        }

        // Edges are checked one at a time, and in this order: an edge that isn't
        // forward, or whose destination this page doesn't own, is rejected *as
        // that fault* rather than as a duplicate — a re-delivered prefix edge is
        // both, and "your page ends outside its own rows" is the useful answer.
        // Identity uniqueness is then checked against committed edges plus the
        // candidates accepted so far, so a page can't smuggle in a repeat.
        let mut seen_edges: HashSet<(usize, usize, usize, usize)> =
            self.edges.iter().map(edge_identity).collect();
        for e in &page.edges {
            if e.from_row >= e.to_row {
                return Err(HistoryInvariantError::NonForwardEdge {
                    from_row: e.from_row,
                    to_row: e.to_row,
                });
            }
            // Destination-page ownership: a cross-page *source* is fine (a branch
            // dives from an earlier page into this one), but the destination must
            // be one of this page's own rows, so every edge is delivered exactly
            // once, by the page that owns where it lands.
            if e.to_row < page_start || e.to_row >= page_end {
                return Err(HistoryInvariantError::EdgeDestinationOutsidePage {
                    page_start,
                    page_end,
                    to_row: e.to_row,
                });
            }
            if !seen_edges.insert(edge_identity(e)) {
                return Err(HistoryInvariantError::DuplicateEdge {
                    from_row: e.from_row,
                    from_lane: e.from_lane,
                    to_row: e.to_row,
                    to_lane: e.to_lane,
                });
            }
        }

        // Lanes are cumulative: a page reporting fewer than are already drawn
        // would shrink the gutter and reflow stubs left, under the user's cursor.
        if page.lane_count < self.lane_high_water {
            return Err(HistoryInvariantError::LaneHighWaterRegressed {
                previous: self.lane_high_water,
                actual: page.lane_count,
            });
        }

        // ---- commit (nothing below can fail) ---------------------------------
        let old_row_count = page_start;
        let old_edge_count = self.edges.len();
        let stubs_before = self.resolved_stubs();
        let occupancy_before = self.label_occupancy.clone();

        self.rows.extend(page.rows);
        self.edges.extend(page.edges);
        self.stubs.extend(page.stubs);
        self.oid_to_row.extend(new_oids);
        self.lane_high_water = page.lane_count;
        self.cursor = page.cursor;

        // Geometry is grown, never rebuilt: each pass takes a max against what is
        // already there, so a label can only ever move right. Rebuilding from
        // scratch would be equivalent today and a shrinking-label bug tomorrow.
        let new_lanes: Vec<usize> = self.rows[old_row_count..].iter().map(|r| r.lane).collect();
        self.label_occupancy.extend(new_lanes);
        apply_edge_occupancy(&mut self.label_occupancy, &self.edges[old_edge_count..]);
        // All stubs, not just the new ones: a higher lane high-water shifts every
        // resolved stub right, which widens the rows their rings hang over.
        let stubs_after = self.resolved_stubs();
        apply_stub_occupancy(&mut self.label_occupancy, &stubs_after);
        self.text_x = self
            .label_occupancy
            .iter()
            .map(|&lane| node_cx(lane) + LABEL_GAP)
            .collect();

        Ok(AppendDelta {
            old_row_count,
            // Only rows that existed before this page count as "prefix"; the zip
            // stops at the old length.
            prefix_geometry_changed: occupancy_before
                .iter()
                .zip(&self.label_occupancy)
                .any(|(before, after)| after > before),
            stub_geometry_changed: stubs_before != stubs_after,
        })
    }

    /// Every stub whose anchor commit is loaded, placed. A stub whose
    /// `anchor_commit` is unknown is *retained* but skipped here — the server
    /// contract puts a stub on the page owning its anchor, and a client that
    /// indexed a malformed one would panic instead of merely not drawing it.
    pub fn resolved_stubs(&self) -> Vec<ResolvedStub> {
        self.stubs
            .iter()
            .filter_map(|stub| {
                let anchor_row = *self.oid_to_row.get(&stub.anchor_commit)?;
                let anchor_lane = self.rows.get(anchor_row)?.lane;
                Some(ResolvedStub {
                    stub: stub.clone(),
                    anchor_row,
                    anchor_lane,
                    // Stub columns live past the commit lanes; `lane_offset` is
                    // cumulative, so two stubs never share a column.
                    lane: self.lane_high_water + stub.lane_offset,
                })
            })
            .collect()
    }

    /// Per-row rightmost occupied lane — the dot, anything passing through, and
    /// any stub ring hanging over the row.
    ///
    /// The view never needs this — it reads [`Self::text_x`], the x the occupancy
    /// resolves to — so the accessor exists for the host tests that pin the
    /// monotonic-growth rule. Hence the wasm-only `dead_code` guard, per-item like
    /// the ones in [`crate::geometry`]: on the browser target it genuinely has no
    /// caller, and that is by design rather than a loose end.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn label_occupancy(&self) -> &[usize] {
        &self.label_occupancy
    }

    /// Per-row left edge (x) of the label column, derived from the occupancy.
    pub fn text_x(&self) -> &[i32] {
        &self.text_x
    }

    /// True once the server has no cursor left to hand out — the aggregate is the
    /// whole history, which is what Print requires.
    pub fn is_complete(&self) -> bool {
        self.cursor.is_none()
    }
}

fn edge_identity(e: &Edge) -> (usize, usize, usize, usize) {
    (e.from_row, e.from_lane, e.to_row, e.to_lane)
}

/// Widen occupancy for the rows `edges` pass through. Same over-approximation of
/// the S-curve as [`crate::geometry::label_x_per_row`]: an endpoint row allows one
/// lane of bulge (capped at the outer lane), a row strictly between takes the
/// outer lane. Applied incrementally — only the newly delivered edges — because
/// the committed rows already carry the effect of every earlier one.
fn apply_edge_occupancy(occupancy: &mut [usize], edges: &[Edge]) {
    if occupancy.is_empty() {
        return;
    }
    let last = occupancy.len() - 1;
    for e in edges {
        let (top, bot) = if e.from_row <= e.to_row {
            (e.from_row, e.to_row)
        } else {
            (e.to_row, e.from_row)
        };
        let hi = e.from_lane.max(e.to_lane);
        for (r, occ_r) in occupancy
            .iter_mut()
            .enumerate()
            .take(bot.min(last) + 1)
            .skip(top.min(last))
        {
            let lane = if r == e.from_row {
                (e.from_lane + 1).min(hi)
            } else if r == e.to_row {
                (e.to_lane + 1).min(hi)
            } else {
                hi
            };
            *occ_r = (*occ_r).max(lane);
        }
    }
}

/// Widen occupancy for the rows a stub ring hangs over: its anchor row and the
/// ⌈(depth+1)/2⌉ rows above it, since the cascade steps upward half a row at a
/// time. Re-applied for *all* resolved stubs after every append.
fn apply_stub_occupancy(occupancy: &mut [usize], stubs: &[ResolvedStub]) {
    if occupancy.is_empty() {
        return;
    }
    let last = occupancy.len() - 1;
    for s in stubs {
        let up = (s.stub.depth + 2) / 2;
        let top = s.anchor_row.saturating_sub(up);
        for occ_r in occupancy
            .iter_mut()
            .take(s.anchor_row.min(last) + 1)
            .skip(top)
        {
            *occ_r = (*occ_r).max(s.lane);
        }
    }
}

/// How a failed page may be retried. A recoverable failure (network, 5xx) can
/// reuse the same cursor; a cursor the server *rejected* never can — the only
/// honest recovery is to reload from page 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRetry {
    SameCursor,
    Reseed,
}

/// Single-flight state for cursor appends. Both `Loading` and `Error` suppress
/// automatic fetching: one keeps the camera from stacking requests, the other
/// keeps a broken cursor from being hammered until the user asks again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageLoadState {
    Idle,
    Loading {
        cursor: String,
    },
    Error {
        cursor: String,
        message: String,
        retry: PageRetry,
    },
}

/// Identity of an in-flight page request. A reply is admitted only while all
/// three still match the live view: the reload `epoch` (a refresh happened), the
/// `generation` (history moved), and the `cursor` (the aggregate advanced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequestKey {
    pub epoch: u32,
    pub generation: GenerationToken,
    pub cursor: String,
}

impl PageRequestKey {
    pub fn is_current(
        &self,
        current_epoch: u32,
        current_generation: &GenerationToken,
        current_cursor: Option<&str>,
    ) -> bool {
        self.epoch == current_epoch
            && &self.generation == current_generation
            && current_cursor == Some(self.cursor.as_str())
    }
}

/// Whether the camera should pull the next page. Pure, so the append trigger is
/// testable without a DOM: the view feeds it the culled range, the aggregate's
/// size, and the live viewport/zoom.
///
/// The lookahead is measured in *viewports*, not rows, so it means the same
/// thing zoomed in or out — at a smaller scale a screen shows more rows, so more
/// rows must already be loaded before the user reaches the bottom.
pub fn should_prefetch(
    visible_end: usize,
    row_count: usize,
    viewport_h: f64,
    scale: f64,
    page_load: &PageLoadState,
    has_cursor: bool,
) -> bool {
    let lookahead_rows = (PREFETCH_VIEWPORTS * viewport_h
        / (scale.max(f64::EPSILON) * f64::from(ROW_HEIGHT)))
    .ceil() as usize;
    matches!(page_load, PageLoadState::Idle)
        && has_cursor
        && visible_end.saturating_add(lookahead_rows) >= row_count
}

/// Whether the fixed in-canvas "loading" affordance is shown. Only a request
/// actually in flight earns it; an error shows its own retry affordance instead.
pub fn show_fixed_loading_overlay(page_load: &PageLoadState) -> bool {
    matches!(page_load, PageLoadState::Loading { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::model::CommitSummary;

    /// Every fixture page belongs to the same repository generation unless a test
    /// is specifically about a generation change.
    const GEN: &str = "g1";

    /// A viewport exactly 1.5 * 560 / 56 = 15 rows of lookahead at scale 1, so the
    /// prefetch boundary below is an exact number rather than an approximation.
    const VIEWPORT_H: f64 = 560.0;

    fn generation(value: &str) -> GenerationToken {
        GenerationToken::new(value).expect("test generation token")
    }

    fn commit(id: &str) -> CommitSummary {
        CommitSummary {
            id: Oid(id.into()),
            parents: vec![],
            summary: format!("commit {id}"),
            author: "tester".into(),
            time: 0,
        }
    }

    fn row(index: usize, lane: usize, id: &str) -> GraphRow {
        GraphRow {
            commit: commit(id),
            row: index,
            lane,
            refs: vec![],
            color: 0,
            on_remote: false,
        }
    }

    fn edge(from_row: usize, from_lane: usize, to_row: usize, to_lane: usize) -> Edge {
        Edge {
            from_row,
            from_lane,
            to_row,
            to_lane,
        }
    }

    fn stub(name: &str, anchor: &str, lane_offset: usize) -> FrameStub {
        FrameStub {
            name: name.into(),
            anchor_commit: Oid(anchor.into()),
            lane_offset,
            color: 3,
            depth: 0,
        }
    }

    fn page(
        rows: Vec<GraphRow>,
        edges: Vec<Edge>,
        stubs: Vec<FrameStub>,
        lane_count: usize,
        cursor: Option<&str>,
        generation_value: &str,
    ) -> Page {
        Page {
            rows,
            edges,
            stubs,
            lane_count,
            cursor: cursor.map(str::to_owned),
            generation: generation(generation_value),
        }
    }

    /// Page 1: rows 0..2 on one lane, the straight edge between them, more to come.
    fn page_one() -> Page {
        page(
            vec![row(0, 0, "aaa0"), row(1, 0, "bbb1")],
            vec![edge(0, 0, 1, 0)],
            vec![],
            1,
            Some("c1"),
            GEN,
        )
    }

    fn seeded() -> LoadedHistory {
        LoadedHistory::from_first_page(page_one()).expect("page 1 is valid")
    }

    /// Page 2 in its plain form: row 2 only, hanging off row 1 in the same lane.
    fn page_two() -> Page {
        page(
            vec![row(2, 0, "ccc2")],
            vec![edge(1, 0, 2, 0)],
            vec![],
            1,
            Some("c2"),
            GEN,
        )
    }

    #[test]
    fn two_pages_append_without_mutating_prefix() {
        let mut history = seeded();
        let before = history.clone();

        let delta = history
            .append_page("c1", page_two())
            .expect("a contiguous page-2 append is valid");

        assert_eq!(delta.old_row_count, 2);
        assert!(
            !delta.prefix_geometry_changed,
            "a straight same-lane append must not move an existing label"
        );
        assert!(!delta.stub_geometry_changed, "no stubs on either page");

        // The prefix is *the same rows*, not merely equivalent ones.
        assert_eq!(&history.rows[..2], &before.rows[..]);
        assert_eq!(&history.edges[..1], &before.edges[..]);
        assert_eq!(&history.label_occupancy()[..2], before.label_occupancy());
        assert_eq!(&history.text_x()[..2], before.text_x());

        assert_eq!(history.rows.len(), 3);
        assert_eq!(history.cursor.as_deref(), Some("c2"));
        assert_eq!(history.oid_to_row.get(&Oid("ccc2".into())), Some(&2));
        assert!(!history.is_complete(), "a cursor means more pages remain");
    }

    #[test]
    fn first_page_nonzero_row_rejects_before_mutation() {
        let err = LoadedHistory::from_first_page(page(
            vec![row(1, 0, "bbb1")],
            vec![],
            vec![],
            1,
            Some("c1"),
            GEN,
        ))
        .expect_err("page 1 must start at absolute row zero");
        assert_eq!(
            err,
            HistoryInvariantError::NonContiguousRow {
                expected: 0,
                actual: 1
            }
        );
    }

    #[test]
    fn gap_or_reordered_page_rejects_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        // A gap: page 2 starts at row 3 when the aggregate ends at row 1.
        let err = history
            .append_page(
                "c1",
                page(vec![row(3, 0, "ddd3")], vec![], vec![], 1, None, GEN),
            )
            .expect_err("a page that skips a row must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::NonContiguousRow {
                expected: 2,
                actual: 3
            }
        );
        assert_eq!(history, before);

        // Reordered within the page: the rows are the right *set*, wrong order.
        let err = history
            .append_page(
                "c1",
                page(
                    vec![row(3, 0, "ddd3"), row(2, 0, "ccc2")],
                    vec![],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect_err("a reordered page must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::NonContiguousRow {
                expected: 2,
                actual: 3
            }
        );
        assert_eq!(history, before);
    }

    #[test]
    fn oid_index_rejects_duplicate_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        // Re-delivering a commit the aggregate already holds.
        let err = history
            .append_page(
                "c1",
                page(vec![row(2, 0, "aaa0")], vec![], vec![], 1, None, GEN),
            )
            .expect_err("an OID already in the aggregate must be refused");
        assert_eq!(err, HistoryInvariantError::DuplicateOid(Oid("aaa0".into())));
        assert_eq!(history, before);

        // And a page that repeats an OID inside itself.
        let err = history
            .append_page(
                "c1",
                page(
                    vec![row(2, 0, "ccc2"), row(3, 0, "ccc2")],
                    vec![],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect_err("an OID repeated within one page must be refused");
        assert_eq!(err, HistoryInvariantError::DuplicateOid(Oid("ccc2".into())));
        assert_eq!(history, before);
    }

    #[test]
    fn duplicate_edge_rejects_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        let err = history
            .append_page(
                "c1",
                page(
                    vec![row(2, 0, "ccc2")],
                    vec![edge(0, 0, 2, 0), edge(0, 0, 2, 0)],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect_err("the same four-field edge twice in one page must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::DuplicateEdge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 0
            }
        );
        assert_eq!(history, before);
    }

    #[test]
    fn generation_mismatch_rejects_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        let err = history
            .append_page("c1", page_two_with_generation("g2"))
            .expect_err("a page minted against another generation must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::GenerationMismatch {
                expected: generation("g1"),
                actual: generation("g2"),
            }
        );
        assert_eq!(history, before);
    }

    fn page_two_with_generation(generation_value: &str) -> Page {
        page(
            vec![row(2, 0, "ccc2")],
            vec![edge(1, 0, 2, 0)],
            vec![],
            1,
            Some("c2"),
            generation_value,
        )
    }

    #[test]
    fn stale_cursor_rejects_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        let err = history
            .append_page("c0", page_two())
            .expect_err("a response to a cursor the aggregate has moved past must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::CursorChanged {
                requested: "c0".into(),
                current: Some("c1".into()),
            }
        );
        assert_eq!(history, before);
    }

    #[test]
    fn destination_page_accepts_cross_page_source_edge() {
        let mut history = seeded();

        // from_row 0 lives in page 1, to_row 2 is this page's own row: the page
        // owning the *destination* owns the edge.
        history
            .append_page(
                "c1",
                page(
                    vec![row(2, 0, "ccc2")],
                    vec![edge(0, 0, 2, 0)],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect("a cross-page source edge is valid on the destination's page");

        assert_eq!(history.rows.len(), 3);
        assert_eq!(history.edges.last(), Some(&edge(0, 0, 2, 0)));
        assert!(
            history.is_complete(),
            "no cursor means the history is whole"
        );
    }

    #[test]
    fn prefix_destination_edge_rejects_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        // to_row 1 belongs to page 1, which already delivered this edge.
        let err = history
            .append_page(
                "c1",
                page(
                    vec![row(2, 0, "ccc2")],
                    vec![edge(0, 0, 1, 0)],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect_err("an edge landing in the prefix must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::EdgeDestinationOutsidePage {
                page_start: 2,
                page_end: 3,
                to_row: 1,
            }
        );
        assert_eq!(history, before);
    }

    #[test]
    fn future_destination_edge_rejects_before_mutation() {
        let mut history = seeded();
        let before = history.clone();

        // to_row 3 belongs to a page that hasn't arrived yet.
        let err = history
            .append_page(
                "c1",
                page(
                    vec![row(2, 0, "ccc2")],
                    vec![edge(0, 0, 3, 0)],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect_err("an edge landing past this page must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::EdgeDestinationOutsidePage {
                page_start: 2,
                page_end: 3,
                to_row: 3,
            }
        );
        assert_eq!(history, before);
    }

    #[test]
    fn lane_high_water_regression_rejects_before_mutation() {
        // Seed with two commit lanes, then offer a page claiming only one: lanes
        // never shrink, so an already-drawn lane can't vanish under the graph.
        let mut history = LoadedHistory::from_first_page(page(
            vec![row(0, 0, "aaa0"), row(1, 1, "bbb1")],
            vec![edge(0, 0, 1, 1)],
            vec![],
            2,
            Some("c1"),
            GEN,
        ))
        .expect("a two-lane page 1 is valid");
        let before = history.clone();

        let err = history
            .append_page(
                "c1",
                page(
                    vec![row(2, 0, "ccc2")],
                    vec![edge(1, 1, 2, 0)],
                    vec![],
                    1,
                    None,
                    GEN,
                ),
            )
            .expect_err("a shrinking lane high-water must be refused");
        assert_eq!(
            err,
            HistoryInvariantError::LaneHighWaterRegressed {
                previous: 2,
                actual: 1,
            }
        );
        assert_eq!(history, before);
    }

    #[test]
    fn valid_append_updates_cursor_lane_stubs_atomically() {
        let mut history = seeded();

        let delta = history
            .append_page(
                "c1",
                page(
                    vec![row(2, 1, "ccc2")],
                    vec![edge(0, 0, 2, 1)],
                    vec![stub("wip", "ccc2", 0)],
                    2,
                    Some("c2"),
                    GEN,
                ),
            )
            .expect("page 2 is valid");

        assert_eq!(history.rows.len(), 3);
        assert_eq!(history.cursor.as_deref(), Some("c2"));
        assert_eq!(history.lane_high_water, 2);
        assert_eq!(history.oid_to_row.len(), 3);
        assert_eq!(history.oid_to_row.get(&Oid("ccc2".into())), Some(&2));

        // The stub resolves against its own page's anchor row and sits past the
        // commit lanes.
        assert_eq!(
            history.resolved_stubs(),
            vec![ResolvedStub {
                stub: stub("wip", "ccc2", 0),
                anchor_row: 2,
                anchor_lane: 1,
                lane: 2,
            }]
        );

        // The lane-changing edge widens the rows it passes through, so the delta
        // tells the view its old labels moved.
        assert_eq!(
            delta,
            AppendDelta {
                old_row_count: 2,
                prefix_geometry_changed: true,
                stub_geometry_changed: true,
            }
        );
        assert!(history.label_occupancy()[0] >= 1);
        assert_eq!(
            history.text_x().len(),
            history.rows.len(),
            "text-x is populated from the committed occupancy"
        );
    }

    #[test]
    fn stale_page_request_key_is_not_current() {
        let key = PageRequestKey {
            epoch: 7,
            generation: generation("g1"),
            cursor: "c1".into(),
        };

        assert!(key.is_current(7, &generation("g1"), Some("c1")));
        // Each of the three coordinates alone makes the reply stale.
        assert!(!key.is_current(8, &generation("g1"), Some("c1")), "epoch");
        assert!(
            !key.is_current(7, &generation("g2"), Some("c1")),
            "generation"
        );
        assert!(!key.is_current(7, &generation("g1"), Some("c2")), "cursor");
        assert!(
            !key.is_current(7, &generation("g1"), None),
            "a completed history has no cursor left to match"
        );
    }

    #[test]
    fn prefetch_uses_one_point_five_viewports_and_single_flight() {
        let idle = PageLoadState::Idle;

        // 15 rows of lookahead at scale 1: 85 + 15 reaches 100, 84 + 15 doesn't.
        assert!(should_prefetch(85, 100, VIEWPORT_H, 1.0, &idle, true));
        assert!(!should_prefetch(84, 100, VIEWPORT_H, 1.0, &idle, true));
        // Zoomed out, a viewport covers more rows, so the lookahead grows with 1/scale.
        assert!(should_prefetch(70, 100, VIEWPORT_H, 0.5, &idle, true));
        assert!(!should_prefetch(69, 100, VIEWPORT_H, 0.5, &idle, true));
        // A degenerate scale must saturate, not divide by zero.
        assert!(should_prefetch(0, 100, VIEWPORT_H, 0.0, &idle, true));

        // Single flight: a request already in the air blocks another.
        assert!(!should_prefetch(
            85,
            100,
            VIEWPORT_H,
            1.0,
            &PageLoadState::Loading {
                cursor: "c1".into()
            },
            true
        ));
        assert!(!should_prefetch(
            85,
            100,
            VIEWPORT_H,
            1.0,
            &PageLoadState::Error {
                cursor: "c1".into(),
                message: "boom".into(),
                retry: PageRetry::SameCursor,
            },
            true
        ));
        // A complete history has no cursor to follow.
        assert!(!should_prefetch(85, 100, VIEWPORT_H, 1.0, &idle, false));
    }

    #[test]
    fn page_error_blocks_prefetch_until_explicit_retry() {
        // Same camera boundary throughout: only the load state differs.
        let at_boundary =
            |state: &PageLoadState| should_prefetch(85, 100, VIEWPORT_H, 1.0, state, true);

        let failed = PageLoadState::Error {
            cursor: "c1".into(),
            message: "500 Internal Server Error".into(),
            retry: PageRetry::SameCursor,
        };
        assert!(
            !at_boundary(&failed),
            "a failed page must not be retried by the camera"
        );
        assert!(
            at_boundary(&PageLoadState::Idle),
            "the user's explicit Retry clears the error and the same camera fetches"
        );
    }

    #[test]
    fn bad_cursor_retry_reseeds_instead_of_reusing_cursor() {
        let rejected = PageLoadState::Error {
            cursor: "stale-cursor".into(),
            message: "400 Bad Request".into(),
            retry: PageRetry::Reseed,
        };
        assert!(
            !should_prefetch(85, 100, VIEWPORT_H, 1.0, &rejected, true),
            "the rejected cursor is never fetched again automatically"
        );

        // Retry bumps the reload epoch (App owns that signal); the in-flight key
        // carrying the rejected cursor is stale the moment it does, so a late
        // reply can't re-enter the new aggregate.
        let key = PageRequestKey {
            epoch: 7,
            generation: generation(GEN),
            cursor: "stale-cursor".into(),
        };
        assert!(!key.is_current(8, &generation(GEN), Some("stale-cursor")));

        // And the reseed starts from page 1, which carries the server's fresh
        // cursor — never the rejected one.
        let reseeded = seeded();
        assert_eq!(reseeded.cursor.as_deref(), Some("c1"));
        assert_ne!(reseeded.cursor.as_deref(), Some("stale-cursor"));
        assert!(should_prefetch(
            85,
            100,
            VIEWPORT_H,
            1.0,
            &PageLoadState::Idle,
            true
        ));
    }

    #[test]
    fn fixed_loading_overlay_depends_only_on_page_state() {
        // The overlay's *placement* (an untransformed HTML child of `.graph`, so
        // over-pan can't carry it off-screen) is the canvas view's job; what this
        // module owns is the one state that shows it.
        assert!(show_fixed_loading_overlay(&PageLoadState::Loading {
            cursor: "c1".into()
        }));
        assert!(!show_fixed_loading_overlay(&PageLoadState::Idle));
        assert!(!show_fixed_loading_overlay(&PageLoadState::Error {
            cursor: "c1".into(),
            message: "boom".into(),
            retry: PageRetry::SameCursor,
        }));
        assert!(!show_fixed_loading_overlay(&PageLoadState::Error {
            cursor: "c1".into(),
            message: "400 Bad Request".into(),
            retry: PageRetry::Reseed,
        }));
    }
}
