//! The checkpointable core of the topology pass.
//!
//! This is the same single lane algorithm the one-shot layout has always used
//! (see the [module docs](super) for the lane rule), turned inside out: instead
//! of walking a whole commit vector and returning a finished [`Graph`], it takes
//! commits one at a time and can be cut at any row into a [`LayoutChunk`] plus a
//! [`LayoutCheckpoint`] that a later, entirely separate request can
//! [`resume`](StreamLayout::resume) from.
//!
//! Three things make paging safe:
//!
//! * **Rows are absolute.** `next_row` rides along in the checkpoint, so row 250
//!   is row 250 whether it arrived in one window or the fiftieth.
//! * **Membership is the caller's.** [`push`](StreamLayout::push) takes the
//!   snapshot-membership predicate per commit, so a parent deliberately cut by
//!   the caller (a shallow boundary, a commit below the walk floor) reserves no
//!   lane and wires no edge, while an included parent always does both.
//! * **Edges belong to their destination page.** A commit's parents are always
//!   *below* it, so every edge starts life as a [`PendingEdge`] and only becomes
//!   a [`ResolvedEdge`] on the page where the parent row actually lands. A page
//!   therefore owns each of its edges exactly once, and never has to index an
//!   absolute `from_row` into its own row slice — that is what
//!   [`ResolvedEdge::parent_ordinal`] is for.

use crate::model::{CommitSummary, Edge, GraphRow, Oid};

use super::topology::{leftmost_free, leftmost_free_right_of};

/// Everything the lane walk needs to carry on from a row boundary — and nothing
/// else. It holds no repository generation and no traversal state; pairing it
/// with the right snapshot is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutCheckpoint {
    /// Active lanes: `open_lanes[i] == Some(id)` means lane `i` is reserved by an
    /// already-drawn child and expects (older) commit `id` next.
    open_lanes: Vec<Option<Oid>>,
    /// Edges whose parent row has not been reached yet.
    pending_edges: Vec<PendingEdge>,
    /// The absolute row the next pushed commit takes.
    next_row: usize,
    /// Highest commit lane used so far, plus one — monotonic across checkpoints.
    lane_high_water: usize,
}

/// One page of laid-out geometry: the rows this window produced, and the edges
/// whose *destination* row lands in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutChunk {
    pub rows: Vec<GraphRow>,
    pub resolved_edges: Vec<ResolvedEdge>,
    /// Commit-lane high-water through this checkpoint; stub lanes are excluded.
    pub lane_count: usize,
}

/// A commit -> parent link whose parent row has not been walked yet. Retained in
/// the checkpoint until the parent arrives (or the walk ends, in which case it is
/// dropped — an unresolved edge names no row and must never reach the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdge {
    parent_oid: Oid,
    parent_ordinal: usize,
    from_row: usize,
    from_lane: usize,
}

/// A wire [`Edge`] plus the index this parent had in the child's parent vector.
/// Keeping the ordinal alongside the edge is what lets a page sort itself into
/// canonical order without looking up `from_row` in its own (page-local) rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEdge {
    pub edge: Edge,
    pub parent_ordinal: usize,
}

/// The lane walk, mid-flight: a [`LayoutCheckpoint`] plus the buffers for the
/// chunk currently being built.
pub struct StreamLayout {
    open_lanes: Vec<Option<Oid>>,
    pending_edges: Vec<PendingEdge>,
    next_row: usize,
    lane_high_water: usize,
    rows: Vec<GraphRow>,
    resolved_edges: Vec<ResolvedEdge>,
}

impl StreamLayout {
    /// Start a fresh walk at absolute row zero.
    ///
    /// `trunk_tip` holds lane 0 for the trunk's own line — see
    /// [`trunk_reserve_tip`](super::topology::trunk_reserve_tip) for why. It must
    /// already be filtered through the *same* membership predicate that
    /// [`push`](Self::push) will be given: a reservation for a commit that never
    /// arrives is never released, and would leave lane 0 permanently occupied.
    pub fn new(trunk_tip: Option<Oid>) -> Self {
        let mut open_lanes = Vec::new();
        if let Some(tip) = trunk_tip {
            open_lanes.push(Some(tip));
        }
        Self {
            open_lanes,
            pending_edges: Vec::new(),
            next_row: 0,
            lane_high_water: 0,
            rows: Vec::new(),
            resolved_edges: Vec::new(),
        }
    }

    /// Carry on from a checkpoint with empty chunk buffers.
    pub fn resume(checkpoint: LayoutCheckpoint) -> Self {
        let LayoutCheckpoint {
            open_lanes,
            pending_edges,
            next_row,
            lane_high_water,
        } = checkpoint;
        Self {
            open_lanes,
            pending_edges,
            next_row,
            lane_high_water,
            rows: Vec::new(),
            resolved_edges: Vec::new(),
        }
    }

    /// Place the next commit (newest to oldest, children before parents).
    ///
    /// `parent_is_in_snapshot` is the caller's exact membership predicate: a
    /// parent it accepts reserves a lane, a parent it rejects reserves nothing and
    /// wires no edge.
    pub fn push<F>(&mut self, commit: CommitSummary, mut parent_is_in_snapshot: F)
    where
        F: FnMut(&Oid) -> bool,
    {
        let row = self.next_row;

        // Lanes already expecting this commit (reserved by its children). Take the
        // leftmost; the rest are sibling branch lines converging here, so free them.
        let reserved: Vec<usize> = self
            .open_lanes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref() == Some(&commit.id))
            .map(|(i, _)| i)
            .collect();
        let lane = match reserved.first() {
            Some(&leftmost) => {
                for &i in &reserved[1..] {
                    self.open_lanes[i] = None;
                }
                leftmost
            }
            // A branch tip: reuse the leftmost free lane.
            None => leftmost_free(&mut self.open_lanes),
        };

        // Every child of this commit is already placed, so this row is where each
        // of their pending edges finally learns its destination. Both endpoints
        // are known now, and this page owns the resulting edge.
        let resolved_edges = &mut self.resolved_edges;
        self.pending_edges.retain(|pending| {
            if pending.parent_oid != commit.id {
                return true;
            }
            resolved_edges.push(ResolvedEdge {
                edge: Edge {
                    from_row: pending.from_row,
                    from_lane: pending.from_lane,
                    to_row: row,
                    to_lane: lane,
                },
                parent_ordinal: pending.parent_ordinal,
            });
            false
        });

        // First parent continues this lane (or frees it when the caller cut the
        // parent), which is what gives each branch a stable column.
        match commit.parents.first() {
            Some(p) if parent_is_in_snapshot(p) => self.open_lanes[lane] = Some(p.clone()),
            _ => self.open_lanes[lane] = None,
        }
        // Extra (merge) parents fan out strictly rightward unless a lane already
        // expects them, in which case the branches share it.
        for parent in commit.parents.iter().skip(1) {
            if !parent_is_in_snapshot(parent) {
                continue; // cut by the caller: no lane, no edge
            }
            if self.open_lanes.iter().any(|s| s.as_ref() == Some(parent)) {
                continue; // already reserved — merges into an existing line
            }
            let i = leftmost_free_right_of(&mut self.open_lanes, lane);
            self.open_lanes[i] = Some(parent.clone());
        }

        // Parents always sit below their child, so an accepted parent's row is
        // still ahead of us: hold the link (with its parent-vector ordinal) until
        // that row arrives, on whatever page that turns out to be.
        for (parent_ordinal, parent) in commit.parents.iter().enumerate() {
            if !parent_is_in_snapshot(parent) {
                continue;
            }
            self.pending_edges.push(PendingEdge {
                parent_oid: parent.clone(),
                parent_ordinal,
                from_row: row,
                from_lane: lane,
            });
        }

        self.rows.push(GraphRow {
            commit,
            row,
            lane,
            refs: Vec::new(),
            color: 0,
        });
        self.next_row = row + 1;
        self.lane_high_water = self.lane_high_water.max(lane + 1);
    }

    /// Close the current chunk and hand back the state a later window resumes
    /// from. Pending edges stay pending — their destination row is on a later page.
    pub fn checkpoint(self) -> (LayoutChunk, LayoutCheckpoint) {
        let Self {
            open_lanes,
            pending_edges,
            next_row,
            lane_high_water,
            rows,
            mut resolved_edges,
        } = self;
        sort_resolved_edges(&mut resolved_edges);
        let chunk = LayoutChunk {
            rows,
            resolved_edges,
            lane_count: lane_high_water,
        };
        let checkpoint = LayoutCheckpoint {
            open_lanes,
            pending_edges,
            next_row,
            lane_high_water,
        };
        (chunk, checkpoint)
    }

    /// Close the walk. Anything still pending names a parent that never arrived,
    /// so it is dropped rather than wired to a row that does not exist.
    pub fn finish(self) -> LayoutChunk {
        let (chunk, _discarded) = self.checkpoint();
        chunk
    }
}

/// Canonical delivery order for one chunk: `(from_row, parent_ordinal, to_row,
/// from_lane, to_lane)`. Uses only the sidecar, never the chunk's rows, so it is
/// safe on a page that does not start at absolute row zero.
pub fn sort_resolved_edges(edges: &mut [ResolvedEdge]) {
    edges.sort_by_key(|resolved| {
        (
            resolved.edge.from_row,
            resolved.parent_ordinal,
            resolved.edge.to_row,
            resolved.edge.from_lane,
            resolved.edge.to_lane,
        )
    });
}

/// Drop the sidecar and keep the wire edges, in the order given.
pub fn strip_resolved_edges(edges: Vec<ResolvedEdge>) -> Vec<Edge> {
    edges.into_iter().map(|resolved| resolved.edge).collect()
}

/// Full aggregate starting at absolute row zero only; never pass page-local rows.
pub fn canonicalize_edges(rows: &[GraphRow], edges: &mut [Edge]) {
    edges.sort_by_key(|edge| {
        let parent = &rows[edge.to_row].commit.id;
        let ordinal = rows[edge.from_row]
            .commit
            .parents
            .iter()
            .position(|oid| oid == parent)
            .expect("well-formed layout edge names a parent");
        (
            edge.from_row,
            ordinal,
            edge.to_row,
            edge.from_lane,
            edge.to_lane,
        )
    });
}
