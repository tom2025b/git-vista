//! Graph state: the frame, the paged-history aggregate, its request state, and the
//! epoch that decides when a stale view must be discarded (M1.11, #64).
//!
//! Framework-free (M1.11 D1): this is the whole reason `LoadedHistory`'s validate-
//! then-commit invariants are unit-tested on the host rather than only exercised in a
//! browser, and it is the pattern the rest of the feature cores generalise. Moved here
//! verbatim from the crate-root `history.rs` (M1.10, #63) and `render/mod.rs`'s
//! `RenderCtx`; the 17 `LoadedHistory` tests below moved with it, unchanged.

use crate::features::core_traits::{Applied, Invalidate, InvalidateScope};

use std::collections::{HashMap, HashSet};
use std::fmt;

use git_vista_core::model::{Edge, FrameStub, GitRef, GraphRow, Oid, RefKind};
use git_vista_protocol::plan::Advisory;
use git_vista_protocol::{
    CommitOid, GenerationToken, HistoryFrame, HistoryPage, RefChange, RefState, RepoMode, RiskLevel,
};

use crate::geometry::{node_cx, LABEL_GAP, ROW_HEIGHT};

/// The frontend's concrete half of the generic wire envelopes. The protocol crate
/// stays free of domain types, so *this* crate names the row/edge/stub/ref types
/// that fill them in — the server declares its own, identical aliases.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub type Frame = HistoryFrame<GitRef>;
pub type Page = HistoryPage<GraphRow, Edge, FrameStub>;

/// The reload epoch and the fencing rule that decides when it must bump.
///
/// Before M1.11 this was `reload: RwSignal<u32>` in `App` — a bare counter every
/// writer bumped unconditionally after every write, so the graph re-read after
/// EVERY operation regardless of whether the repository actually moved. `GraphCore`
/// makes that decision a tested function of the invalidation's generation (design
/// spec D3): the same generation means nothing moved, so nothing re-reads.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GraphCore {
    epoch: u64,
    generation: Option<GenerationToken>,
}

impl GraphCore {
    /// Start at epoch 0, already at `generation` — the seed state once the first
    /// Frame has landed and reported a generation.
    pub fn at_generation(generation: &str) -> Self {
        Self {
            epoch: 0,
            generation: Some(GenerationToken::new(generation).expect("valid generation token")),
        }
    }

    /// Bump unconditionally — a repository switch, a 409 drift reseed, or an
    /// explicit Refresh — and report the new epoch. These are not invalidations
    /// with a generation to compare against; they are the user (or the server)
    /// saying "everything you have is void," so there is nothing to be
    /// conservative about. Returning the new value lets a caller stamp a UI
    /// phase (`SeedLoading { epoch }`, `DriftReloading { epoch }`) with the epoch
    /// that will actually be live, not one read before the bump.
    pub fn force_bump(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn generation(&self) -> Option<&GenerationToken> {
        self.generation.as_ref()
    }

    /// Apply a published invalidation. Only `InvalidateScope::Graph` and
    /// `InvalidateScope::Everything` are this core's business; anything else is
    /// silently `NoChange` — the invalidation was never addressed to it.
    pub fn on_invalidate(&mut self, inv: &Invalidate) -> Applied {
        if !matches!(
            inv.scope,
            InvalidateScope::Graph | InvalidateScope::Everything
        ) {
            return Applied::NoChange;
        }
        match &inv.generation {
            // The server could not read a generation after execution (ADR 0020
            // allows `None`). Re-reading is the safe default: silently skipping
            // would strand a stale graph with no way to notice it moved.
            None => {
                self.epoch += 1;
                Applied::Committed
            }
            Some(g) if self.generation.as_ref() == Some(g) => Applied::NoChange,
            Some(g) => {
                self.generation = Some(g.clone());
                self.epoch += 1;
                Applied::Committed
            }
        }
    }
}

#[cfg(test)]
mod graph_core_suite;

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

/// Everything the per-row / per-edge view builders need, bundled so a reactive
/// `<For>` closure (Phase 8 viewport virtualization) can reach it cheaply.
///
/// This is the mounted canvas's single owner of history (M1.10, #63): the
/// once-per-view [`Frame`] and the growing [`LoadedHistory`]. `remote_branches`
/// is the only derived table kept here, and only because it is a property of the
/// Frame — it cannot drift as pages land. Moved here from `render/mod.rs`
/// (M1.11, #64): a plain data bundle has no reason to live in the view-builder
/// module, and belongs beside the state it bundles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCtx {
    /// The reload epoch this canvas was mounted for. A page reply carrying any
    /// other epoch belongs to a retired view and is dropped.
    pub epoch: u64,
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
    pub epoch: u64,
    pub generation: GenerationToken,
    pub cursor: String,
}

impl PageRequestKey {
    pub fn is_current(
        &self,
        current_epoch: u64,
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
///
/// `eager` (#217) bypasses the viewport-proximity check entirely — still
/// single-flight (`Idle`) and still stops the moment `has_cursor` goes false,
/// but no longer waits for the camera to scroll near the loaded edge. An epoch
/// bump (Refresh, a settled write, a 409 drift reload) always remounts the
/// canvas from page 1 (`seed_for_epoch`), so a history that was ever driven to
/// completion in an earlier epoch loses that completeness the instant the new
/// one mounts — genuinely, not spuriously: pages are pinned to a generation and
/// a fresh aggregate has only page 1. Without `eager`, recovering it demands
/// scrolling all the way back down through however many pages the repository
/// has, with the camera reset to the top by the same remount (`canvas.rs`'s
/// `home` camera). `eager` is how the App resumes that pagination itself in
/// the background once the user has demonstrated they want the whole history
/// (`HistoryUiSignals::want_full_history`, set the first time `complete` goes
/// true and never reset by the epoch effect), so Print Graph re-enables on its
/// own instead of staying dark until a full manual re-scroll.
pub fn should_prefetch(
    visible_end: usize,
    row_count: usize,
    viewport_h: f64,
    scale: f64,
    page_load: &PageLoadState,
    has_cursor: bool,
    eager: bool,
) -> bool {
    let lookahead_rows = (PREFETCH_VIEWPORTS * viewport_h
        / (scale.max(f64::EPSILON) * f64::from(ROW_HEIGHT)))
    .ceil() as usize;
    matches!(page_load, PageLoadState::Idle)
        && has_cursor
        && (eager || visible_end.saturating_add(lookahead_rows) >= row_count)
}

/// Whether the fixed in-canvas "loading" affordance is shown. Only a request
/// actually in flight earns it; an error shows its own retry affordance instead.
pub fn show_fixed_loading_overlay(page_load: &PageLoadState) -> bool {
    matches!(page_load, PageLoadState::Loading { .. })
}

/// The Print Graph topbar button's label and tooltip for a given
/// `history_complete` state (#217).
///
/// Before this, only the `title` attribute changed when the button disabled
/// itself — CSS dimming plus a native tooltip that never surfaces on tap (the
/// reported iPad case), so the button read as silently, unexplainably broken.
/// The label now carries the same reason the tooltip does, so it is visible
/// without hover/long-press. Pure so the two states are testable without a DOM:
/// the view (wasm-only, `app/mod.rs`) supplies `complete` and renders both
/// strings as-is.
pub fn print_button_copy(complete: bool) -> (&'static str, &'static str) {
    if complete {
        (
            "Print Graph",
            "A clean, printable view of the whole graph — print it or save it as a PDF",
        )
    } else {
        (
            "Print Graph (loading history…)",
            "Load all history before printing.",
        )
    }
}

/// The same fix as [`print_button_copy`], applied to `menu.rs`'s four disabled
/// context-menu items (#65): a `title`-only reason never surfaces on a tap, only
/// on a mouse hover, so the reason has to live somewhere a finger — or VoiceOver
/// — can reach it too.
///
/// Returns `(aria_label, visible_line)`. `menu.rs` (wasm-only, `view!`-macro
/// code that cannot itself be host-tested) supplies `label`/`reason` and is
/// responsible for putting `aria_label` on `aria-label` and rendering
/// `visible_line` as a second line inside the item — this function only
/// composes the strings, so the composition is the one part of the fix a host
/// test can pin.
pub fn disabled_menu_item_copy(label: &str, reason: &str) -> (String, String) {
    (format!("{label}: {reason}"), reason.to_string())
}

/// The picker's Visualize/Active button label for a given `mode` while
/// `opening` tracks which mode (if any) is mid-request (#244 follow-up).
///
/// Before this, both buttons bound to one shared `busy` flag: click either
/// one and BOTH went inert with no visual change, for up to two minutes on a
/// slow retry (`send_write_with_key`'s 60s-times-two design). Indistinguishable
/// from the app being broken. Now only the clicked button's label changes —
/// the other stays disabled but keeps its normal wording, since it isn't the
/// one doing anything. Pure so the mapping is testable without a DOM: the
/// view (wasm-only, `picker.rs`) supplies `mode`/`opening` and renders the
/// string as-is.
pub fn mode_button_label(mode: RepoMode, opening: Option<RepoMode>) -> &'static str {
    match (mode, opening) {
        (RepoMode::Visualize, Some(RepoMode::Visualize)) => "Visualize — opening…",
        (RepoMode::Visualize, _) => "Visualize — look only, with links out",
        (RepoMode::Active, Some(RepoMode::Active)) => "Active — opening…",
        (RepoMode::Active, _) => "Active — full git operations",
    }
}

/// The "Pull" context-menu item's label (#325 follow-up): unlike its `Rebase
/// onto {base}` sibling, Pull named no subject at all — a bare "Pull" gave no
/// hint which branch or remote a tap would act on. `menu.rs` already reads a
/// live checked-out-branch name for `rebase_item` (`RebaseStatus::branch`,
/// fetched by `fetch_rebase_status()` under the same `!m.is_branch` gate Pull
/// itself renders behind) — reusing that signal here costs no new endpoint or
/// poll. `branch` is `None` while the resource is still loading, or on a
/// genuinely detached HEAD; the label degrades to naming just the remote
/// rather than showing nothing or a placeholder. `remote` is passed in rather
/// than hardcoded so a future remote picker doesn't have to touch this
/// function again — today `menu.rs`'s only caller always passes `"origin"`,
/// the same static value `fetch_item`'s confirm dialog uses. Pure so the
/// wasm-only view (which cannot itself be host-tested) supplies `branch`/
/// `remote` and this function only composes the string — the same split
/// `disabled_menu_item_copy` and `print_button_copy` above use. Wording
/// mirrors `dialogs/confirm.rs`'s own "Pull '{branch}' from '{remote}'..."
/// prompt for the same operation, so the menu names what the confirm dialog
/// is about to ask.
pub fn pull_label(branch: Option<&str>, remote: &str) -> String {
    match branch {
        Some(b) => format!("Pull ‘{b}’ from ‘{remote}’"),
        None => format!("Pull from ‘{remote}’"),
    }
}

/// The "Create tag…" context-menu item's label (M2.21d, #238): the same
/// stub-vs-commit-dot wording split `MenuData::create_label` makes for
/// "Create branch…", pulled into one pure, host-tested function instead of
/// being duplicated as a literal at each of the three `MenuData`
/// construction sites — the same move `mode_button_label` above already
/// makes for its own pair of wordings. `is_branch` is `MenuData::is_branch`
/// itself: `true` for a stub's own menu (its subject is the branch, so
/// tagging reads "from this branch"), `false` for a commit dot ("from this
/// commit").
pub fn create_tag_item_label(is_branch: bool) -> &'static str {
    if is_branch {
        "Create tag from this branch"
    } else {
        "Create tag from this commit"
    }
}

/// Turns the "Create tag" flow's second native prompt — the optional
/// annotation message — into the `message` field `CreateTagRequest` sends
/// (#238, M2.21d, mirroring `CreateTagRequest`'s own doc comment in
/// `git-vista-protocol`).
///
/// This is the one decision in the whole create-tag flow worth pinning
/// somewhere a host test can check it: `None` (or `Some(text)` where `text`
/// is empty/whitespace-only — cancelling the second prompt and dismissing it
/// with nothing typed read the same to a user) means a **lightweight** tag;
/// anything else, trimmed, means an **annotated** one. Getting this backwards
/// silently changes which *kind* of tag gets created — exactly the class of
/// wrong-outcome-with-no-error the DTO's own doc comment (ADR 0048) warns
/// about — so `menu.rs` (wasm-only, cannot itself be host-tested) calls this
/// rather than deciding it inline.
pub fn tag_annotation_from_prompt(raw: Option<String>) -> Option<String> {
    let trimmed = raw?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Whether the "Create tag" flow's sign offer applies at all, and — if it
/// does — whether the user accepted it (M2.21e, #239).
///
/// A lightweight tag has no tag object for a signature to live in, so the
/// sign offer only makes sense once [`tag_annotation_from_prompt`] has
/// already produced a message; this is the same "decide it once, in a
/// host-testable function" posture that function's own doc comment argues
/// for, applied to the one extra branch `menu.rs` (wasm-only, cannot itself
/// be host-tested) cannot pin on its own.
pub fn tag_sign_choice(has_message: bool, confirmed_sign: bool) -> bool {
    has_message && confirmed_sign
}

// ---------------------------------------------------------------------------
// The Push / force-with-lease confirmation (#233, M2.20g)
// ---------------------------------------------------------------------------

/// The three distinct facts a `POST /api/plan` response's
/// `expected_ref_changes[0].before` can carry for a `PushBranch` preview —
/// see `planner.rs`'s D5 comment (around line 1595), which is explicit that
/// "the read failed" and "the branch was never pushed" must not share one
/// `None`. Collapsing them here would repeat exactly the mistake that
/// comment forbids server-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTipKnowledge {
    /// The live read succeeded and origin/`<branch>` is at this commit —
    /// the oid a force-with-lease would pin.
    Known(CommitOid),
    /// origin/`<branch>` doesn't exist yet — nothing to overwrite, so a
    /// force-with-lease is meaningless; a plain push already does
    /// everything a lease-force would.
    NotYetPushed,
    /// The live read itself failed (the plan's `before` field is
    /// unknowable, `planner.rs`'s `Obs::Unknown` case) — the honest answer
    /// is "we don't know", never a guess in either direction.
    Unreadable,
}

/// Read [`RemoteTipKnowledge`] off a `POST /api/plan` response built for a
/// *plain* `GitOperation::PushBranch` (`ForcePublish::None`) — see
/// `api::preview_push`'s doc comment for why the menu reads a plain-push
/// plan first, before it has an oid to build the real lease plan from.
///
/// Reads only `changes.first()`: `planner.rs`'s `PushBranch` arm never
/// produces more than one `RefChange` (the remote-tracking ref), so a
/// second entry is not a case this function has to account for.
pub fn remote_tip_from_plan(changes: &[RefChange]) -> RemoteTipKnowledge {
    match changes.first().map(|c| &c.before) {
        Some(RefState::At(oid)) => RemoteTipKnowledge::Known(oid.clone()),
        Some(RefState::Absent) => RemoteTipKnowledge::NotYetPushed,
        // `Symbolic`/`Computed` never occur for a `PushBranch`'s
        // remote-tracking `before` — folded into the same "unreadable"
        // answer as a missing entry, rather than a `_ => unreachable!()`
        // that would panic the one caller (a wasm view) least able to
        // afford it.
        Some(RefState::Symbolic(_) | RefState::Computed) | None => RemoteTipKnowledge::Unreadable,
    }
}

/// Everything `dialogs/confirm.rs` needs to render one `PendingOp::Push`
/// confirmation (#233): the plain single-tap ceremony this operation has
/// always had, or the danger tier `ForceDelete`/`Undo` already set the bar
/// for. A named struct, not the bare `(title, body, label, danger)` tuple
/// `pull_label` returns elsewhere in this file — `confirm.rs` also needs
/// `enabled`, which every existing Push confirmation has always hardcoded
/// `true` (there is no "can't push" precondition this dialog itself
/// checks), so that field is left for the caller rather than carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfirmCopy {
    pub title: &'static str,
    pub body: String,
    pub confirm_label: &'static str,
    pub danger: bool,
}

/// `force`: `None` for a plain push. `Some((remote_tip, risk))` for a
/// force-with-lease already resolved to a concrete oid — the menu's
/// force-push entry point only reaches this function once
/// [`RemoteTipKnowledge`] came back [`RemoteTipKnowledge::Known`], so this
/// function never has to render the "couldn't read"/"nothing to overwrite"
/// cases itself (those are shown as their own error notice, `menu.rs`).
///
/// `risk` is the server's `POST /api/plan` classification for *that exact
/// plan* — `danger` below is `risk == RiskLevel::Destructive`, never
/// `force.is_some()`. That is #233's explicit requirement: the danger
/// styling must be driven by what the planner actually answered, not by a
/// client-side assumption that every lease is destructive (true today, per
/// `planner.rs`'s `ForcePublish::WithLease => RiskLevel::Destructive`
/// match, but this function does not re-derive that match — it reads the
/// field the caller already fetched).
pub fn push_confirm_copy(
    branch: &str,
    set_upstream: bool,
    force: Option<(&CommitOid, RiskLevel)>,
    advisories: &[Advisory],
) -> PushConfirmCopy {
    let upstream_line = if set_upstream {
        format!(
            "\n\nThis also records ‘origin/{branch}’ as ‘{branch}’’s upstream \
             (--set-upstream), so future pushes and pulls need no remote named."
        )
    } else {
        String::new()
    };
    match force {
        None => PushConfirmCopy {
            title: "Push branch",
            body: format!("Push ‘{branch}’ to origin?{upstream_line}"),
            confirm_label: "Push",
            danger: false,
        },
        Some((oid, risk)) => PushConfirmCopy {
            title: "Force-push branch",
            body: format!(
                "Force-push ‘{branch}’ to origin? This overwrites origin/{branch} — \
                 currently at {} — with what's here. If anyone else pushed to \
                 ‘{branch}’ since you last looked, their commits become unreachable \
                 there. This can't be undone.{upstream_line}{}",
                short_oid(oid.as_str()),
                advisory_lines(advisories),
            ),
            confirm_label: "Force Push",
            danger: risk == RiskLevel::Destructive,
        },
    }
}

/// What this commit's menu should offer, given the current anchor.
///
/// Pure, so the decision is testable without a DOM: the rendering below reads
/// this and nothing else. Extracted because the interesting part is not which
/// buttons appear but WHICH COMMIT ENDS UP AS `base` — get that backwards and
/// every diff in the app reads inverted while still looking plausible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareOffer {
    /// No anchor set: offer to make this commit the anchor.
    SetAnchor,
    /// This commit IS the anchor: offer only to clear it. Comparing a commit
    /// with itself is an empty diff, so no compare items are offered.
    ClearAnchor,
    /// A different commit is anchored: offer both comparisons.
    ///
    /// `base` is the ANCHOR and `target` is the commit whose menu this is —
    /// "compare from here, with that" reads as anchor → here, matching the
    /// direction `RefVsRef` already uses for "Compare with HEAD".
    Compare { base: String, target: String },
}

pub fn offer_for(anchor: Option<&str>, this: &str) -> CompareOffer {
    match anchor {
        None => CompareOffer::SetAnchor,
        Some(a) if a == this => CompareOffer::ClearAnchor,
        Some(a) => CompareOffer::Compare {
            base: a.to_string(),
            target: this.to_string(),
        },
    }
}

/// A comparison, remembered with the repository it means something in.
///
/// # Why the repo id is stored with it
///
/// A `DiffSpec` is a pair of commit oids or ref names and nothing else. Restore
/// one into a DIFFERENT repository and it either errors or — worse, and the
/// reason this field exists — silently resolves against unrelated commits and
/// renders a diff that looks real. Storing the repo makes that unrepresentable:
/// a comparison whose repo does not match the one on screen is simply not
/// restored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredComparison {
    /// Opaque server-assigned repository token, from `HistoryFrame::repo_id`.
    pub repo_id: String,
    pub spec: git_vista_protocol::diff::DiffSpec,
}

/// The stored comparison to restore, or `None` when it belongs to a different
/// repository.
///
/// Pure and host-tested. The whole point of the type is this one comparison,
/// and `prefs` is `#[cfg(target_arch = "wasm32")]` — a test written beside it
/// would compile out and report `0 passed` forever.
pub fn restorable_for(
    stored: &StoredComparison,
    repo_id: &str,
) -> Option<git_vista_protocol::diff::DiffSpec> {
    (stored.repo_id == repo_id).then(|| stored.spec.clone())
}

/// The planner's advisories, as prose appended to the force-push confirmation
/// (M4.32, #85).
///
/// # Why only two of the three are rendered
///
/// [`Advisory::RemoteHistoryReplaced`] is deliberately skipped: the force body
/// above already says the push overwrites `origin/<branch>`, makes other
/// people's commits unreachable, and cannot be undone. Printing the same fact
/// twice in one dialog is how a reader learns to skim past the warnings that
/// are not duplicated — the same argument `advisories_for` makes server-side
/// for refusing to warn on an ordinary push at all.
///
/// # Why `DefaultBranchUnknown` gets its own sentence
///
/// It is NOT an all-clear, and must never read as one. The server carries a
/// separate `Unknown` variant precisely so that "the check could not run" is
/// distinguishable from "the check ran and this is not the default branch" —
/// the latter emits no advisory at all. Collapsing the two here would throw
/// away the distinction the protocol went out of its way to preserve, and
/// would tell the user a dangerous push is ordinary.
fn advisory_lines(advisories: &[Advisory]) -> String {
    let mut out = String::new();
    for advisory in advisories {
        match advisory {
            Advisory::DefaultBranchPush { branch, remote } => out.push_str(&format!(
                "\n\n‘{}’ is {}’s default branch — the one everyone starts from. \
                 Replacing its history affects every clone.",
                branch.as_str(),
                remote.as_str(),
            )),
            Advisory::DefaultBranchUnknown { reason } => out.push_str(&format!(
                "\n\nThis preview could not tell whether that is the default branch: \
                 {reason}. Treat it as unknown, not as safe.",
            )),
            // Already stated by the body above; see this function's doc.
            Advisory::RemoteHistoryReplaced { .. } => {}
        }
    }
    out
}

/// The conventional 7-char short id, for confirmation copy (#233) —
/// mirrors the server's own `planner::short`/`planner::push::short`
/// (`crates/git-vista-server/src/planner.rs:2258`,
/// `crates/git-vista-server/src/planner/push.rs:667`), so the truncation
/// this client shows matches the one the server's own journal and undo
/// labels already use.
pub(crate) fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

// ── Label links, glyphs and badge colours (#653) ─────────────────────────────
//
// `render/labels.rs` (the interactive label tier) and `print.rs` (the static
// print sheet) draw the same badges over the same refs, and both are
// `#[cfg(target_arch = "wasm32")]` — so every rule below used to live where
// `cargo test --workspace` compiles none of it. Two of them were literally
// duplicated across the pair, each copy carrying a comment promising it
// matched the other; `print.rs` even kept a `#[cfg(test)] mod tests` for its
// link rule that has never run a single time on this box. Per ADR 0115 the
// decision moves here, where a host test executes it, and the two wasm-only
// files are left with markup to arrange.

/// Whether a badge or commit label has a GitHub page to point at, and — when
/// it does not — whether that is because the repository has no GitHub remote
/// at all or because this particular ref/commit simply is not pushed yet.
///
/// The distinction is the whole reason this is an enum rather than an
/// `Option<String>`. `render/labels.rs` styles the three cases differently
/// (linked / dimmed-and-unlinked / plain), and it used to derive the middle
/// one at each call site as `repo_url.is_some() && url.is_none()` — the same
/// expression written out three times next to three separate URL
/// computations. Any one of them drifting from its URL rule shows a `.unpushed`
/// commit as ordinary, or dims one that links fine; neither is visible to a
/// test that only checks the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefLink {
    /// A page that resolves: draw it as a real link.
    To(String),
    /// This repository has a GitHub base, but this target has no page on it —
    /// it is not pushed. Draw it dimmed and unlinked; a link here would 404.
    Unpushed,
    /// No GitHub base at all. Nothing to link, and nothing to dim either —
    /// an unlinked label in a repo with no remote is not a deficiency.
    NoRemote,
}

impl RefLink {
    /// Consume into the owned URL, if any — the only accessor the views need,
    /// since both move the string straight into a `href=` attribute. A
    /// borrowing `url(&self)` twin was written first and deleted: nothing but
    /// a test ever called it, and `reachability_census` said so.
    pub fn into_url(self) -> Option<String> {
        match self {
            Self::To(url) => Some(url),
            Self::Unpushed | Self::NoRemote => None,
        }
    }

    /// `class:clickable` — true exactly when there is a URL.
    pub const fn clickable(&self) -> bool {
        matches!(self, Self::To(_))
    }

    /// `class:unpushed` — the dimmed "GitHub repo, but not on it yet" state.
    /// Never true when [`Self::clickable`] is, and never true for a repository
    /// with no GitHub base: those two facts are what the call sites kept
    /// re-deriving.
    pub const fn unpushed(&self) -> bool {
        matches!(self, Self::Unpushed)
    }
}

/// The settled commit-link rule shared by the print sheet and the interactive
/// labels: only a GitHub-backed commit known to be on the remote has a
/// reachable page (#12). Moved here from `print.rs`, which is wasm-only and
/// whose three tests for this rule therefore never compiled.
pub fn commit_link(repo_url: Option<&str>, on_remote: bool, commit_id: &str) -> RefLink {
    match repo_url {
        None => RefLink::NoRemote,
        Some(base) if on_remote => RefLink::To(format!("{base}/commit/{commit_id}")),
        Some(_) => RefLink::Unpushed,
    }
}

/// Where one ref badge links on GitHub (#12), by kind:
///
///  * **HEAD / tag** → the commit they sit on, when that commit is pushed. A
///    tag's own page cannot be verified offline, so the commit it points at is
///    linked instead — that resolves whenever the commit is pushed.
///  * **local branch** → its tree page, but *only* when a remote branch of the
///    same name exists. Without that check a local-only branch badge would
///    link to a tree page that 404s.
///  * **remote branch** → its tree page unconditionally; it is on the remote by
///    definition. Its leading `<remote>/` is stripped, since GitHub's tree
///    URLs name the branch, not the remote.
///
/// `remote_branch_named` answers "does a remote branch with this exact name
/// exist?" — the caller's `RenderCtx::remote_branches` lookup, passed in as a
/// fact so this rule needs no collection type and no lifetime.
pub fn ref_badge_link(
    kind: &RefKind,
    ref_name: &str,
    repo_url: Option<&str>,
    commit_on_remote: bool,
    commit_id: &str,
    remote_branch_named: bool,
) -> RefLink {
    let Some(base) = repo_url else {
        return RefLink::NoRemote;
    };
    match kind {
        RefKind::Head | RefKind::Tag => commit_link(Some(base), commit_on_remote, commit_id),
        RefKind::Branch => {
            if remote_branch_named {
                RefLink::To(format!("{base}/tree/{ref_name}"))
            } else {
                RefLink::Unpushed
            }
        }
        RefKind::RemoteBranch => {
            let branch = ref_name.split_once('/').map_or(ref_name, |(_, b)| b);
            RefLink::To(format!("{base}/tree/{branch}"))
        }
    }
}

/// The badge glyph for a ref kind. Local branches get the branch icon, remote
/// branches the alternate one — so local and remote pills differ at a glance
/// before the name is read — tags the tag icon, and HEAD the commit icon (it
/// marks the commit you are on). The glyph counts into the pill's width like
/// any other monospace character.
///
/// One mapping, not two: `render/labels.rs` and `print.rs` each held a copy,
/// each labelled "same mapping as" the other.
pub fn ref_glyph(ic: &crate::icons::GitIcons, kind: &RefKind) -> &'static str {
    match kind {
        RefKind::Head => ic.commit,
        RefKind::Tag => ic.tag,
        RefKind::Branch => ic.branch,
        RefKind::RemoteBranch => ic.branch_alt,
    }
}

/// Which surface a badge is being drawn on. The two differ in exactly one
/// place and it is not cosmetic drift: [`RefKind::Head`]'s near-white fill is
/// invisible on paper without a grey outline, and that outline would be wrong
/// on the dark canvas. Naming the surface makes the one intended divergence
/// explicit and keeps the other three kinds structurally identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeSurface {
    /// The interactive canvas: dark background, HEAD outlined in its own fill.
    Screen,
    /// The print sheet: white paper, HEAD outlined in grey so the pill exists.
    Paper,
}

/// A badge pill's three colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadgeColors {
    pub fill: &'static str,
    pub stroke: &'static str,
    pub text: &'static str,
}

/// HEAD's outline on paper. Its `HEAD_BADGE` fill is near-white, so on a white
/// sheet a HEAD pill with a `HEAD_BADGE` stroke has no edge at all.
const HEAD_PAPER_STROKE: &str = "#57606a";

/// The pill colours for one ref kind on one surface. `branch` is the row's
/// already-resolved branch colour (`branch_color(slot)`), which local and
/// remote branch badges take — filled for local, outlined for remote — while
/// HEAD and tags carry fixed colours.
pub fn badge_colors(kind: &RefKind, branch: &'static str, surface: BadgeSurface) -> BadgeColors {
    use git_vista_core::color::{BADGE_DARK, HEAD_BADGE, TAG_BADGE};
    match kind {
        RefKind::Head => BadgeColors {
            fill: HEAD_BADGE,
            stroke: match surface {
                BadgeSurface::Screen => HEAD_BADGE,
                BadgeSurface::Paper => HEAD_PAPER_STROKE,
            },
            text: BADGE_DARK,
        },
        RefKind::Tag => BadgeColors {
            fill: TAG_BADGE,
            stroke: TAG_BADGE,
            text: BADGE_DARK,
        },
        RefKind::Branch => BadgeColors {
            fill: branch,
            stroke: branch,
            text: BADGE_DARK,
        },
        RefKind::RemoteBranch => BadgeColors {
            fill: "none",
            stroke: branch,
            text: branch,
        },
    }
}

#[cfg(test)]
mod label_link_suite;

#[cfg(test)]
mod compare_offer_suite;

#[cfg(test)]
mod restore_comparison_suite;

#[cfg(test)]
mod push_confirm_suite;

#[cfg(test)]
mod history_suite;

#[cfg(test)]
mod ui_copy_suite;
