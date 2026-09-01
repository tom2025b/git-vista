//! The topology pass's *inputs*: normalise commit order, decide which commit
//! lane 0 is held for, and pick lanes.
//!
//! Split out of `layout.rs`: this is the geometry half of the layout — the pure
//! "which column does each commit sit in, and how do the lines connect" walk
//! described in the [module docs](super). The walk itself lives in
//! [`stream`](super::stream), which is the crate's one and only lane algorithm;
//! what stays here is what feeds it — the deterministic order normalisation, the
//! trunk reservation, and the two lane-picking helpers the walk calls per commit.
//! [`assign_branch_colors`](super) then paints the result and
//! [`attach_ref_badges`](super) decorates it. Everything here is `pub(super)`:
//! the layout entry points in the parent and that walk are the only callers.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::model::{CommitSummary, GitRef, Oid};

/// Normalise commit order into a *deterministic* newest-first topological
/// order: every commit precedes all of its in-window parents (so the lane walk
/// is always sound), and among the commits whose children are all placed, the
/// newest commit time goes first with the id as a fixed tie-break. The git
/// walk upstream orders by commit time alone — same-second commits (bursts of
/// test commits, rebases, `commit-tree` writes) arrive in whatever order the
/// walker's queue produced, and that order shifted whenever the tip set
/// changed, reshuffling the whole layout after unrelated operations.
pub(super) fn stable_topo_order(commits: Vec<CommitSummary>) -> Vec<CommitSummary> {
    walk(commits, false).0
}

/// [`stable_topo_order`], plus the ids whose row position was decided by
/// comparing object id strings.
///
/// # Why this exists rather than a predicate that models the heap
///
/// An id comparison happens in exactly one circumstance: two entries sit in
/// `ready` at the same moment carrying the same `time`, so the heap falls
/// through to `Reverse(id)` to choose between them. That is a property of *this
/// walk's* state at a particular instant, and nothing outside the walk can know
/// it without reproducing the walk.
///
/// #576's preview needs the answer, because its hypothetical commit's id is one
/// a real run will never write — so any row this comparison decides is a coin
/// flip rather than a fact. The first attempt approximated it with "is this an
/// in-window ancestor of the new commit", which is sound but strictly narrower
/// than the real condition: a commit blocked behind any *other* unemitted child
/// also never reaches the heap beside `added`, and was refused anyway. An
/// outside auditor found that needless refusal.
///
/// The reason it was approximated rather than measured is written into
/// `PreviewLayout::added_time_tied`: writing the tie logic out a second time
/// would let the two copies drift. That objection is to **duplication**, and it
/// is answered by construction here — [`stable_topo_order`] and this function
/// are one walk with one heap and one key, and the flag only decides whether
/// the observation is recorded. There is no second copy to drift.
///
/// The scan is skipped entirely when `want_ties` is false, so the ordinary
/// graph path pays nothing for it.
pub(crate) fn topo_order_with_id_ties(
    commits: Vec<CommitSummary>,
) -> (Vec<CommitSummary>, HashSet<Oid>) {
    walk(commits, true)
}

/// The one walk. `want_ties` decides only whether it observes itself.
fn walk(commits: Vec<CommitSummary>, want_ties: bool) -> (Vec<CommitSummary>, HashSet<Oid>) {
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
    let mut id_ties: HashSet<Oid> = HashSet::new();
    while let Some((t, _, i)) = ready.pop() {
        if want_ties {
            // Anything still ready carrying the same second was compared with
            // the entry just popped, by id and nothing else. Both sides of that
            // comparison had their row decided by it, so both are recorded.
            let mut tied = false;
            for (other_t, _, oi) in ready.iter() {
                if *other_t == t {
                    tied = true;
                    id_ties.insert(commits[*oi].id.clone());
                }
            }
            if tied {
                id_ties.insert(commits[i].id.clone());
            }
        }
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
    // Git DAGs are acyclic, so a real walk always emits every commit. This
    // guard is unreachable by any caller that exists in this codebase today:
    // `layout_with_refs()`'s commits come from `walk_history` (real git),
    // and #576's preview hands this walk one commit that isn't real git
    // history — `added` — but `added` is read back from an actual
    // `git commit-tree` write in a scratch clone (see `read_back` in
    // `git-vista-server/src/preview.rs`), never hand-built, so its one parent
    // is by construction a commit that already exists. A cycle can only be
    // built by hand, which is exactly what
    // `cyclic_input_falls_back_to_the_original_order_rather_than_hanging`
    // (in `layout/tests/topology.rs`) does — it pins this fallback (preserve
    // arrival order, never hang looking for a commit that will never become
    // ready) as a deliberate, tested choice rather than a silent guess at a
    // graph shape.
    //
    // Deliberately NOT wired to a `PreviewUnavailable::CheckFailed` the way
    // `added_time_tied` is: that field fires routinely, on real same-second
    // commits; this one cannot fire through any caller that exists, so there
    // is nothing today for a refusal to protect against. If a future preview
    // operation ever builds `added`'s parent chain from something other than
    // a real, already-existing commit, revisit this and route it through
    // `PreviewLayout`'s report fields instead of leaving it here.
    if order.len() != commits.len() {
        return (commits, id_ties);
    }
    let mut slots: Vec<Option<CommitSummary>> = commits.into_iter().map(Some).collect();
    let ordered = order
        .into_iter()
        .map(|i| slots[i].take().expect("each index emitted once"))
        .collect();
    (ordered, id_ties)
}

/// Leftmost free (`None`) lane, growing the lane set only if none is free.
/// Used for branch tips, which have no incoming edge and so can safely take any
/// free column — picking the leftmost keeps the graph compact.
pub(super) fn leftmost_free(lanes: &mut Vec<Option<Oid>>) -> usize {
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
pub(super) fn leftmost_free_right_of(lanes: &mut Vec<Option<Oid>>, after: usize) -> usize {
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
pub fn trunk_reserve_tip(refs: &[GitRef], head_branch: Option<&str>) -> Option<Oid> {
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
