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
//!    free lane**, only widening the graph when nothing is free.
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

use std::collections::HashMap;

use crate::model::{CommitSummary, Edge, Graph, GraphRow, Oid};

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

/// Lay commits out into a [`Graph`]. `commits` must be newest-first.
pub fn layout(commits: Vec<CommitSummary>) -> Graph {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Oid;

    fn commit(id: &str, parents: &[&str]) -> CommitSummary {
        CommitSummary {
            id: Oid(id.into()),
            parents: parents.iter().map(|p| Oid((*p).into())).collect(),
            summary: format!("commit {id}"),
            author: "tester".into(),
            time: 0,
        }
    }

    fn lane_of(g: &Graph, id: &str) -> usize {
        g.rows.iter().find(|r| r.commit.id.0 == id).unwrap().lane
    }

    /// The edge for a specific commit -> parent link, found by id (independent of
    /// the order edges happen to be emitted in).
    fn edge(g: &Graph, from: &str, to: &str) -> Edge {
        let from_row = g.rows.iter().position(|r| r.commit.id.0 == from).unwrap();
        let to_row = g.rows.iter().position(|r| r.commit.id.0 == to).unwrap();
        g.edges
            .iter()
            .find(|e| e.from_row == from_row && e.to_row == to_row)
            .cloned()
            .unwrap_or_else(|| panic!("no edge {from} -> {to}"))
    }

    /// Sanity invariants every laid-out graph must satisfy, whatever its shape:
    /// rows are top-to-bottom, every node's lane is in range, every edge runs
    /// downward (child above parent) between in-range lanes, and there's exactly
    /// one edge per in-window parent link.
    fn assert_well_formed(g: &Graph) {
        for (i, r) in g.rows.iter().enumerate() {
            assert_eq!(r.row, i, "rows are sequential top-to-bottom");
            assert!(r.lane < g.lane_count, "node lane within lane_count");
        }
        let mut expected_edges = 0;
        let present: std::collections::HashSet<&str> =
            g.rows.iter().map(|r| r.commit.id.0.as_str()).collect();
        for r in &g.rows {
            for p in &r.commit.parents {
                if present.contains(p.0.as_str()) {
                    expected_edges += 1;
                }
            }
        }
        assert_eq!(g.edges.len(), expected_edges, "one edge per in-window link");
        for e in &g.edges {
            assert!(e.to_row > e.from_row, "child {e:?} sits above its parent");
            assert!(e.from_lane < g.lane_count && e.to_lane < g.lane_count);
        }
    }

    #[test]
    fn empty_history_yields_empty_graph() {
        let g = layout(vec![]);
        assert!(g.rows.is_empty());
        assert!(g.edges.is_empty());
        assert_eq!(g.lane_count, 0);
    }

    #[test]
    fn linear_history_stays_in_lane_zero() {
        let g = layout(vec![
            commit("c", &["b"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_well_formed(&g);
        assert_eq!(g.rows.len(), 3);
        assert!(g.rows.iter().all(|r| r.lane == 0), "all in the trunk lane");
        assert_eq!(g.lane_count, 1);
        assert_eq!(g.edges.len(), 2); // c->b, b->a
        assert_eq!(g.rows[0].commit.id.short(), "c"); // newest at row 0
                                                      // Linear links are straight (same lane), top to bottom.
        assert_eq!(
            edge(&g, "c", "b"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 1,
                to_lane: 0
            }
        );
        assert_eq!(
            edge(&g, "b", "a"),
            Edge {
                from_row: 1,
                from_lane: 0,
                to_row: 2,
                to_lane: 0
            }
        );
    }

    #[test]
    fn dangling_parents_are_skipped() {
        // Parent "z" is outside the walked window — no edge, and no lane spent.
        let g = layout(vec![commit("a", &["z"])]);
        assert!(g.edges.is_empty());
        assert_eq!(g.lane_count, 1); // just "a" itself
    }

    #[test]
    fn branch_and_merge_routes_to_the_right() {
        // A feature branch off B that merges back at M. The mainline keeps lane 0;
        // the feature takes lane 1 (to the *right* of the merge), and both
        // collapse back into lane 0 at their shared base B.
        //
        //   M        merge[C, D]
        //   |\
        //   C D
        //   |/
        //   B
        //   |
        //   A
        let g = layout(vec![
            commit("M", &["C", "D"]),
            commit("C", &["B"]),
            commit("D", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "M"), 0);
        assert_eq!(lane_of(&g, "C"), 0, "first parent keeps the merge's lane");
        assert_eq!(
            lane_of(&g, "D"),
            1,
            "the merged branch takes the lane to the right"
        );
        assert_eq!(
            lane_of(&g, "B"),
            0,
            "both branches collapse into lane 0 at B"
        );
        assert_eq!(lane_of(&g, "A"), 0);
        assert_eq!(g.lane_count, 2);

        // The merge's second parent sits to the right of the merge commit...
        assert!(
            lane_of(&g, "D") > lane_of(&g, "M"),
            "no leftward (crossing) merge"
        );
        // ...so the merge edge fans right, and D's edge collapses back to lane 0.
        assert_eq!(
            edge(&g, "M", "D"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 1
            }
        );
        assert_eq!(
            edge(&g, "D", "B"),
            Edge {
                from_row: 2,
                from_lane: 1,
                to_row: 3,
                to_lane: 0
            }
        );
    }

    #[test]
    fn octopus_merge_fans_each_parent_into_its_own_lane() {
        // A 3-parent (octopus) merge O of three branches that all fork from the
        // same root R. Each merged parent gets its own lane to the right of the
        // merge, in parent order, and they all collapse back at R.
        //
        //   O        merge[A, B, C]
        //  /|\
        // A B C
        //  \|/
        //   R
        let g = layout(vec![
            commit("O", &["A", "B", "C"]),
            commit("A", &["R"]),
            commit("B", &["R"]),
            commit("C", &["R"]),
            commit("R", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "O"), 0);
        assert_eq!(lane_of(&g, "A"), 0, "first parent keeps the merge lane");
        assert_eq!(lane_of(&g, "B"), 1, "second parent fans one lane right");
        assert_eq!(lane_of(&g, "C"), 2, "third parent fans two lanes right");
        assert_eq!(
            lane_of(&g, "R"),
            0,
            "all branches collapse back at the root"
        );
        assert_eq!(g.lane_count, 3);

        // Every merge parent is to the right of the octopus node (no crossing).
        for p in ["A", "B", "C"] {
            assert!(lane_of(&g, p) >= lane_of(&g, "O"));
        }
        // The three merge edges fan out to lanes 0, 1, 2.
        assert_eq!(
            edge(&g, "O", "A"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 1,
                to_lane: 0
            }
        );
        assert_eq!(
            edge(&g, "O", "B"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 2,
                to_lane: 1
            }
        );
        assert_eq!(
            edge(&g, "O", "C"),
            Edge {
                from_row: 0,
                from_lane: 0,
                to_row: 3,
                to_lane: 2
            }
        );
    }

    #[test]
    fn sequential_branches_reuse_a_freed_lane() {
        // Two features in sequence: the first merges back (freeing its lane)
        // before the second starts, so the second REUSES lane 1 instead of
        // opening a lane 2 — the whole graph stays only 2 lanes wide.
        //
        //   M2          merge[M1, F2]
        //   |\
        //   | F2        [M1]
        //   |/
        //   M1          merge[B, F1]
        //   |\
        //   | F1        [A]
        //   B |         [A]
        //   |/
        //   A
        let g = layout(vec![
            commit("M2", &["M1", "F2"]),
            commit("F2", &["M1"]),
            commit("M1", &["B", "F1"]),
            commit("F1", &["A"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "F2"), 1, "first feature uses lane 1");
        assert_eq!(
            lane_of(&g, "F1"),
            1,
            "second feature REUSES lane 1, not lane 2"
        );
        assert_eq!(g.lane_count, 2, "graph stays 2 lanes wide thanks to reuse");
    }

    #[test]
    fn concurrent_branches_get_distinct_lanes() {
        // Here feature2 is still open (its base B is deep) when feature1 is merged,
        // so the two branches are live at the same time and must NOT share a lane:
        // feature1 is pushed out to lane 2 while feature2 holds lane 1.
        //
        //   M2          merge[M1, f2]
        //   |\
        //   | f2        [B]   (feature2 — stays open across M1)
        //   M1 |        merge[B, f1]
        //   |\ |
        //   | f1|       [A]   (feature1)
        //   |  /
        //   B /         [A]
        //   |/
        //   A
        let g = layout(vec![
            commit("M2", &["M1", "f2"]),
            commit("f2", &["B"]),
            commit("M1", &["B", "f1"]),
            commit("f1", &["A"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ]);
        assert_well_formed(&g);

        assert_eq!(lane_of(&g, "M2"), 0);
        assert_eq!(lane_of(&g, "M1"), 0, "mainline keeps lane 0");
        assert_eq!(
            lane_of(&g, "f2"),
            1,
            "feature2 holds lane 1 while it's open"
        );
        assert_eq!(
            lane_of(&g, "f1"),
            2,
            "feature1 can't reuse lane 1 — it's still busy"
        );
        assert_ne!(
            lane_of(&g, "f1"),
            lane_of(&g, "f2"),
            "concurrent branches never share"
        );
        assert_eq!(g.lane_count, 3);
    }

    #[test]
    fn a_long_running_branch_keeps_one_stable_lane() {
        // A side branch with two commits of its own, parallel to the mainline,
        // should keep a single stable lane for its whole life (no lane hopping)
        // — its internal link is a straight, same-lane edge.
        //
        //   M           merge[main2, side2]
        //   |\
        //   m2 s2
        //   |  |
        //   m1 s1
        //   |/
        //   base
        let g = layout(vec![
            commit("M", &["main2", "side2"]),
            commit("main2", &["main1"]),
            commit("side2", &["side1"]),
            commit("main1", &["base"]),
            commit("side1", &["base"]),
            commit("base", &[]),
        ]);
        assert_well_formed(&g);

        // Mainline stays in lane 0 the whole way down.
        for c in ["M", "main2", "main1", "base"] {
            assert_eq!(lane_of(&g, c), 0, "{c} stays on the mainline lane");
        }
        // The side branch keeps lane 1 for both its commits — no mislabeling.
        assert_eq!(lane_of(&g, "side2"), 1);
        assert_eq!(lane_of(&g, "side1"), 1);
        assert_eq!(g.lane_count, 2);

        // Same-lane links are straight: the side branch's internal edge and the
        // mainline's edges don't change lanes.
        assert_eq!(
            edge(&g, "side2", "side1"),
            Edge {
                from_row: 2,
                from_lane: 1,
                to_row: 4,
                to_lane: 1
            }
        );
        assert_eq!(
            edge(&g, "main2", "main1"),
            Edge {
                from_row: 1,
                from_lane: 0,
                to_row: 3,
                to_lane: 0
            }
        );
    }
}
