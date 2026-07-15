//! Assigns commits to lanes (columns) for the vertical graph.
//!
//! Input must be ordered newest-first (row 0 sits at the top). This is a full
//! **active-lane tracker** (Phase 6): we walk the history once, top to bottom,
//! maintaining the set of lanes that are currently "live" and which commit each
//! one is waiting to draw next. From that we get clean routing for arbitrary
//! branch/merge topologies — including octopus merges — while keeping the graph
//! as narrow as the topology allows.
//!
//! ## The lane rule
//!
//! We track **active lanes** in a `Vec<Option<Oid>>`: `lanes[i] == Some(id)`
//! means lane `i` is reserved by an already-drawn child and expects (older)
//! commit `id` to appear in it next; `None` means the lane is free and reusable.
//!
//! For each commit, newest to oldest:
//!
//! 1. **Pick its lane.** If one or more lanes already expect this commit (its
//!    children reserved them), it takes the **leftmost** of those; the rest are
//!    **freed** — those sibling branch lines have converged here. If no lane
//!    expects it (a branch tip / the newest commit), it takes the **leftmost
//!    free lane**, only widening the graph when nothing is free. (When refs are
//!    known, lane 0 starts out reserved for the trunk's tip — see
//!    [`trunk_reserve_tip`](topology::trunk_reserve_tip) — so a side branch's
//!    newer commit can't capture the trunk's column.)
//! 2. **Continue its first parent in the same lane**, so a branch keeps a stable
//!    column for its whole life (the mainline stays in lane 0). If the first
//!    parent is out of window the lane is freed.
//! 3. **Place each additional (merge) parent.** If some lane already expects that
//!    parent, the branches share it — no new lane. Otherwise it takes the
//!    leftmost free lane **strictly to the right** of this commit, so merge lines
//!    fan out rightward and never cross back over the mainline to the left.
//!
//! Because a merged-back branch frees its lane (step 1) and the next new branch
//! reuses the leftmost free one (steps 1/3), lanes are recycled: sequential side
//! branches stay narrow, while branches that are live at the same time always get
//! distinct lanes and never share a column.
//!
//! Lanes are assigned in one forward pass (each commit's *final* lane); edges are
//! wired in a second pass so they connect each commit to its parent's final lane
//! even when sibling lanes collapsed left at a merge.
//!
//! ## Determinism
//!
//! Input order is normalised first by [`stable_topo_order`](topology::stable_topo_order)
//! — a date-ordered topological sort — so the layout is a pure function of the DAG +
//! refs. The git walk upstream sorts by commit time alone, and same-second
//! commits (every burst of test commits, every rebase) land in whatever order the
//! walker's queue happened to produce, which reshuffled lanes wholesale after
//! unrelated operations. Colours are likewise a pure function of the branch *name*
//! (see [`stable_color_slot`]), so a branch keeps its colour across operations and
//! a stub keeps its colour when it grows into a real line.
//!
//! ## Module layout
//!
//! This split is move-only — the passes described above just live in their own
//! files now:
//!
//!   * [`topology`] — order normalisation, lane assignment, edge wiring.
//!   * [`color`]    — the palette and the first-parent-chain colouring.
//!   * [`badges`]   — attaching refs to their commits.
//!
//! The two entry points ([`layout`] and [`layout_with_refs`]) stay here and
//! orchestrate those passes.

mod badges;
mod color;
mod topology;

use std::collections::HashSet;

use crate::color::stable_color_slot;
use crate::model::{BranchStub, CommitSummary, GitRef, Graph};

use badges::attach_ref_badges;
use color::assign_branch_colors;
use topology::{layout_topology, stable_topo_order, trunk_reserve_tip};

/// Lay commits out into a [`Graph`], with no ref information. `commits` must be
/// newest-first. Every commit still gets a stable per-branch [`color`], derived
/// purely from topology (first-parent chains). Use [`layout_with_refs`] to also
/// attach branch/tag/HEAD badges and let real branch tips drive the colouring.
///
/// [`color`]: crate::model::GraphRow::color
pub fn layout(commits: Vec<CommitSummary>) -> Graph {
    let commits = stable_topo_order(commits);
    let mut graph = layout_topology(commits, None);
    assign_branch_colors(&mut graph, &[], None);
    graph
}

/// Lay commits out and decorate the graph with `refs`: attach each ref as a badge
/// on the commit it points at, and colour each branch consistently across the
/// whole graph (branch tips seed the colouring; `head_branch` — the checked-out
/// branch — is preferred for the trunk). A local branch that ends up with no
/// commits of its own (e.g. one just created from an existing commit) is drawn as
/// a distinct stub line via [`Graph::stubs`] rather than a second badge.
/// `commits` must be newest-first.
pub fn layout_with_refs(
    commits: Vec<CommitSummary>,
    refs: Vec<GitRef>,
    head_branch: Option<&str>,
) -> Graph {
    let commits = stable_topo_order(commits);
    let trunk_tip = trunk_reserve_tip(&refs, head_branch);
    let mut graph = layout_topology(commits, trunk_tip.as_ref());
    // Colouring also tells us which local branches own no commits of their own
    // (their tip was already claimed by a higher-priority branch) — those become
    // distinct stub lines instead of a second badge on the shared commit.
    let stub_seeds = assign_branch_colors(&mut graph, &refs, head_branch);

    // Lay the stubs out as *cascades*: all stubs that point at the same commit
    // stack into a staircase, each forking off the previous one's tip rather than
    // every one fanning back to the shared commit. So creating another branch at a
    // commit that already has a stub adds a new hollow dot off the last dot — a
    // visible fork from the stub you branched from, not another dot on the commit.
    // Grouping preserves first-appearance order (seed order = branch name), which
    // is the only deterministic order available (git records no "from which stub").
    let mut groups: Vec<(usize, Vec<String>)> = Vec::new();
    let mut stub_names = HashSet::new();
    for (name, anchor_row) in stub_seeds {
        stub_names.insert(name.clone());
        match groups.iter_mut().find(|(row, _)| *row == anchor_row) {
            Some((_, names)) => names.push(name),
            None => groups.push((anchor_row, vec![name])),
        }
    }
    // Each cascade gets its own block of consecutive lanes (right of the commit
    // lanes) so stub `depth` maps to lane `base + depth` — the connector for a
    // deeper stub starts one lane left, at the previous stub's dot. A stub's
    // colour is its *name's* stable slot — the very colour its line will have
    // once it owns commits — so committing on a stub visibly grows that stub
    // into a line instead of handing the new commit to a differently-coloured
    // one.
    let mut next_lane = graph.lane_count;
    let mut stubs = Vec::new();
    for (anchor_row, names) in groups {
        let base = next_lane;
        for (depth, name) in names.into_iter().enumerate() {
            let color = stable_color_slot(&name);
            stubs.push(BranchStub {
                name,
                anchor_row,
                anchor_lane: graph.rows[anchor_row].lane,
                lane: base + depth,
                color,
                depth,
            });
            // The cascade occupies lanes base..=base+depth; keep the next cascade
            // clear of it.
            next_lane = base + depth + 1;
        }
    }
    // Widen the lane count to include the stub columns so the label column sits
    // to the right of them (and the gutter is wide enough to draw the stubs).
    graph.lane_count = graph.lane_count.max(next_lane);
    graph.stubs = stubs;

    // Badge the remaining refs on their commits — but not the stub branches, which
    // are drawn as their own lines (the whole point of this feature).
    attach_ref_badges(&mut graph, refs, &stub_names);
    graph
}

#[cfg(test)]
mod tests;
