//! The topology pass: normalise commit order, then assign each commit a lane and
//! wire the edges between commits and their parents.
//!
//! Split out of `layout.rs`: this is the geometry half of the layout — the pure
//! "which column does each commit sit in, and how do the lines connect" walk
//! described in the [module docs](super). [`assign_branch_colors`](super) then
//! paints the result and [`attach_ref_badges`](super) decorates it. Everything
//! here is `pub(super)` (the layout entry points in the parent are the only
//! callers); the two lane-picking helpers stay private to this file.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::model::{CommitSummary, Edge, GitRef, Graph, GraphRow, Oid};

/// Normalise commit order into a *deterministic* newest-first topological
/// order: every commit precedes all of its in-window parents (so the lane walk
/// is always sound), and among the commits whose children are all placed, the
/// newest commit time goes first with the id as a fixed tie-break. The git
/// walk upstream orders by commit time alone — same-second commits (bursts of
/// test commits, rebases, `commit-tree` writes) arrive in whatever order the
/// walker's queue produced, and that order shifted whenever the tip set
/// changed, reshuffling the whole layout after unrelated operations.
pub(super) fn stable_topo_order(commits: Vec<CommitSummary>) -> Vec<CommitSummary> {
    let index: HashMap<Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.clone(), i))
        .collect();
    // How many in-window children still wait to be emitted before a commit may
    // appear (children always draw above their parents).
    let mut pending_children = vec![0usize; commits.len()];
    for c in &commits {
        for p in &c.parents {
            if let Some(&i) = index.get(p) {
                pending_children[i] += 1;
            }
        }
    }
    // Max-heap on (commit time, Reverse(id)): newest first, smaller id on ties.
    let mut ready: BinaryHeap<(i64, Reverse<String>, usize)> = commits
        .iter()
        .enumerate()
        .filter(|(i, _)| pending_children[*i] == 0)
        .map(|(i, c)| (c.time, Reverse(c.id.0.clone()), i))
        .collect();
    let mut order = Vec::with_capacity(commits.len());
    while let Some((_, _, i)) = ready.pop() {
        order.push(i);
        for p in &commits[i].parents {
            if let Some(&pi) = index.get(p) {
                pending_children[pi] -= 1;
                if pending_children[pi] == 0 {
                    ready.push((commits[pi].time, Reverse(commits[pi].id.0.clone()), pi));
                }
            }
        }
    }
    // Git DAGs are acyclic, so every commit is emitted; keep the original list
    // as a safety net if something upstream ever handed us a cycle.
    if order.len() != commits.len() {
        return commits;
    }
    let mut slots: Vec<Option<CommitSummary>> = commits.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("each index emitted once"))
        .collect()
}

/// Leftmost free (`None`) lane, growing the lane set only if none is free.
/// Used for branch tips, which have no incoming edge and so can safely take any
/// free column — picking the leftmost keeps the graph compact.
fn leftmost_free(lanes: &mut Vec<Option<Oid>>) -> usize {
    if let Some(i) = lanes.iter().position(Option::is_none) {
        i
    } else {
        lanes.push(None);
        lanes.len() - 1
    }
}

/// Leftmost free lane strictly to the right of `after`, appending a new lane if
/// none is free. Used for merge parents so a merge's branch lines always sit to
/// the right of the merge commit — they never reuse a lane to the left and cross
/// back over the mainline.
fn leftmost_free_right_of(lanes: &mut Vec<Option<Oid>>, after: usize) -> usize {
    if let Some(i) = (after + 1..lanes.len()).find(|&i| lanes[i].is_none()) {
        i
    } else {
        lanes.push(None);
        lanes.len() - 1
    }
}

/// The commit whose line lane 0 is held for — the trunk's own tip. `None` when
/// there's no trunk to protect (no branch refs at all).
///
/// Why this exists: the lane walk hands the newest commit "the leftmost free
/// lane", which at row 0 is always lane 0 — even when that commit belongs to a
/// side branch. Its first-parent then continues the same lane, so the side
/// branch visually glues itself on top of the trunk as one unbroken vertical
/// line. Committing on a fresh branch stub made the branch "disappear" into
/// main this way. Reserving lane 0 for the trunk's own tip forces such a side
/// tip into lane 1+, where it forks off the trunk like any other branch.
///
/// Deliberately *not* checkout-aware: an earlier iteration reserved the
/// checked-out branch's tip when its chain ran through the trunk tip (Issue
/// #30's "one unbroken trunk"), which made the same branch render blue-in-lane-0
/// or coloured-in-lane-1 depending on what happened to be checked out — the
/// "main keeps changing colour" instability of the July test round. Now the
/// trunk line always ends at the trunk's tip, and any branch ahead of it forks
/// right in its own stable colour, checked out or not.
pub(super) fn trunk_reserve_tip(refs: &[GitRef], head_branch: Option<&str>) -> Option<Oid> {
    let local = |name: &str| {
        refs.iter()
            .find(|r| matches!(r.kind, crate::model::RefKind::Branch) && r.name == name)
            .map(|r| r.target.clone())
    };
    // Same priority the colour seeding gives the trunk: local `main`, then local
    // `master`, then the checked-out branch — so lane 0 and colour slot 0 always
    // describe the same line.
    local("main")
        .or_else(|| local("master"))
        .or_else(|| head_branch.and_then(&local))
}

/// The pure topology pass: assign each commit a lane and wire edges. Leaves every
/// row's `refs` empty and `color` at 0 — [`assign_branch_colors`](super) fills those.
/// `trunk_tip` (if given and in-window) holds lane 0 for the trunk's line — see
/// [`trunk_reserve_tip`] for why.
pub(super) fn layout_topology(commits: Vec<CommitSummary>, trunk_tip: Option<&Oid>) -> Graph {
    // Row index per commit id, so an edge can find its parent's row, and so we
    // can tell in-window parents from dangling ones.
    let index: HashMap<Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(row, c)| (c.id.clone(), row))
        .collect();

    // Active lanes: `lanes[i] = Some(id)` means lane i currently expects commit
    // `id` next; `None` means the lane is free and reusable.
    let mut lanes: Vec<Option<Oid>> = Vec::new();
    // Hold lane 0 for the trunk before any tip can claim it. When the trunk tip
    // is itself the newest commit this is a no-op (it just collects its
    // reservation); when a side branch's commit is newer, that tip now starts in
    // lane 1+ and forks off the trunk instead of absorbing it.
    if let Some(tip) = trunk_tip.filter(|t| index.contains_key(t)) {
        lanes.push(Some(tip.clone()));
    }
    // Each commit's final lane, decided as we walk newest -> oldest.
    let mut lane_of: HashMap<Oid, usize> = HashMap::new();
    let mut rows = Vec::with_capacity(commits.len());

    // Pass 1: assign every commit a lane and reserve its parents' lanes.
    for (row, commit) in commits.iter().enumerate() {
        // Lanes already expecting this commit (reserved by its children).
        let reserved: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref() == Some(&commit.id))
            .map(|(i, _)| i)
            .collect();

        let lane = match reserved.first() {
            // Take the leftmost reserved lane; the rest are sibling branch lines
            // that converge at this commit, so free them for reuse.
            Some(&leftmost) => {
                for &i in &reserved[1..] {
                    lanes[i] = None;
                }
                leftmost
            }
            // A branch tip: reuse the leftmost free lane.
            None => leftmost_free(&mut lanes),
        };
        lane_of.insert(commit.id.clone(), lane);

        // First parent continues this lane (or frees it if there's nothing
        // in-window to continue to). Keeping the first parent in the same lane is
        // what gives each branch a stable column down to its merge base.
        match commit.parents.first() {
            Some(p) if index.contains_key(p) => lanes[lane] = Some(p.clone()),
            _ => lanes[lane] = None,
        }
        // Extra (merge) parents: if a lane already expects this parent, the
        // branches share it; otherwise reserve the leftmost free lane to the
        // right, so merge lines fan out rightward and don't cross the mainline.
        for parent in commit.parents.iter().skip(1) {
            if !index.contains_key(parent) {
                continue; // dangling: no lane, no edge
            }
            if lanes.iter().any(|s| s.as_ref() == Some(parent)) {
                continue; // already reserved — merges into an existing line
            }
            let i = leftmost_free_right_of(&mut lanes, lane);
            lanes[i] = Some(parent.clone());
        }

        rows.push(GraphRow {
            commit: commit.clone(),
            row,
            lane,
            refs: Vec::new(),
            color: 0,
        });
    }

    // Pass 2: wire edges using each endpoint's final lane.
    let mut edges = Vec::new();
    for (row, commit) in commits.iter().enumerate() {
        let from_lane = lane_of[&commit.id];
        for parent in &commit.parents {
            if let Some(&to_row) = index.get(parent) {
                edges.push(Edge {
                    from_row: row,
                    from_lane,
                    to_row,
                    to_lane: lane_of[parent],
                });
            }
        }
    }

    let lane_count = rows.iter().map(|r| r.lane).max().map_or(0, |m| m + 1);
    Graph {
        rows,
        edges,
        lane_count,
        repo_url: None,
        remote_commits: Vec::new(),
        stubs: Vec::new(),
        repo_label: None,
        repo_id: None,
        worktree_id: None,
        read_only: false,
        resettable: false,
    }
}
