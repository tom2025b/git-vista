//! Assigns commits to lanes (columns) for the vertical graph.
//!
//! Input must be ordered newest-first (row 0 sits at the top). The lane rule
//! (Phase 4, compact reuse):
//!
//! - We track **active lanes** top-to-bottom; each active lane "expects" the next
//!   (older) commit due to appear in it. A free lane is reusable.
//! - A commit takes the leftmost lane that expects it. If several lanes expect it
//!   (branches merging back at a shared commit), it takes the leftmost and the
//!   others are **freed** for reuse.
//! - If no lane expects it (a branch tip / the newest commit), it takes the
//!   **leftmost free lane**, only widening the graph when nothing is free.
//! - Its **first parent continues in the same lane**; each **additional (merge)
//!   parent** takes the leftmost free lane. When a lane's commit has no in-window
//!   parent to continue to, the lane is freed.
//!
//! So when a branch merges back in, its lane becomes available again and the next
//! new branch reuses it, keeping the graph as narrow as possible.
//!
//! Lanes are assigned in one forward pass (computing each commit's final lane);
//! edges are then wired in a second pass so they connect each commit to its
//! parent's *final* lane even when lanes shifted left at a merge.

use std::collections::HashMap;

use crate::model::{CommitSummary, Edge, Graph, GraphRow, Oid};

/// Leftmost free (`None`) lane, growing the lane set only if none is free.
fn leftmost_free(lanes: &mut Vec<Option<Oid>>) -> usize {
    if let Some(i) = lanes.iter().position(Option::is_none) {
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

    // Pass 1: assign every commit a lane.
    for (row, commit) in commits.iter().enumerate() {
        // Lanes already expecting this commit (reserved by its children).
        let claimed: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref() == Some(&commit.id))
            .map(|(i, _)| i)
            .collect();

        let lane = if let Some(&leftmost) = claimed.first() {
            // Take the leftmost; free the rest — those branches merged in here.
            for &i in &claimed[1..] {
                lanes[i] = None;
            }
            leftmost
        } else {
            // A branch tip: reuse the leftmost free lane.
            leftmost_free(&mut lanes)
        };
        lane_of.insert(commit.id.clone(), lane);

        // First parent continues this lane (or frees it if there's nothing
        // in-window to continue to).
        match commit.parents.first() {
            Some(p) if index.contains_key(p) => lanes[lane] = Some(p.clone()),
            _ => lanes[lane] = None,
        }
        // Extra (merge) parents each take the leftmost free lane, unless some
        // lane already expects them (then that lane carries them).
        for parent in commit.parents.iter().skip(1) {
            if !index.contains_key(parent) {
                continue; // dangling: no lane, no edge
            }
            if !lanes.iter().any(|s| s.as_ref() == Some(parent)) {
                let i = leftmost_free(&mut lanes);
                lanes[i] = Some(parent.clone());
            }
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
        assert_eq!(g.rows.len(), 3);
        assert!(g.rows.iter().all(|r| r.lane == 0));
        assert_eq!(g.lane_count, 1);
        assert_eq!(g.edges.len(), 2); // c->b, b->a
        assert_eq!(g.rows[0].commit.id.short(), "c"); // newest at row 0
        assert_eq!(g.edges[0], Edge { from_row: 0, from_lane: 0, to_row: 1, to_lane: 0 });
    }

    #[test]
    fn dangling_parents_are_skipped() {
        // Parent "z" is outside the walked window — no edge, and no lane spent.
        let g = layout(vec![commit("a", &["z"])]);
        assert!(g.edges.is_empty());
        assert_eq!(g.lane_count, 1); // just "a" itself
    }

    #[test]
    fn branch_takes_a_new_lane_and_first_parent_stays() {
        // M(0) merge[C,D]; C(1)[B] mainline; D(2)[B] feature; B(3)[A]; A(4) root.
        //
        //   M
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

        // First parent keeps the merge's lane; the second parent took lane 1.
        assert_eq!(lane_of(&g, "M"), 0);
        assert_eq!(lane_of(&g, "C"), 0);
        assert_eq!(lane_of(&g, "D"), 1);
        assert_eq!(lane_of(&g, "B"), 0); // both branches collapse into lane 0 at B
        assert_eq!(lane_of(&g, "A"), 0);
        assert_eq!(g.lane_count, 2);

        // The merge's second edge crosses from lane 0 to the feature lane 1...
        assert!(g.edges.contains(&Edge { from_row: 0, from_lane: 0, to_row: 2, to_lane: 1 }));
        // ...and D's edge to B crosses back from lane 1 to lane 0.
        assert!(g.edges.contains(&Edge { from_row: 2, from_lane: 1, to_row: 3, to_lane: 0 }));
    }

    #[test]
    fn freed_lanes_are_reused_by_later_branches() {
        // Two features in sequence; the first merges back before the second
        // starts. With lane reuse, the second feature reuses lane 1 instead of
        // opening a lane 2 — so the whole graph is only 2 lanes wide.
        //
        //   M2          [M1, F2]
        //   |\
        //   | F2        [M1]
        //   |/
        //   M1          [B, F1]
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

        assert_eq!(lane_of(&g, "F2"), 1, "first feature uses lane 1");
        assert_eq!(lane_of(&g, "F1"), 1, "second feature REUSES lane 1, not lane 2");
        assert_eq!(g.lane_count, 2, "graph stays 2 lanes wide thanks to reuse");
    }
}
