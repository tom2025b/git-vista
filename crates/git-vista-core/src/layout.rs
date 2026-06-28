//! Assigns commits to lanes (columns) for the vertical graph.
//!
//! Input must be ordered newest-first (row 0 sits at the top). The full
//! algorithm — active-lane tracking, parent routing across columns, and stable
//! per-branch colouring — lands in a later milestone. For now every commit is
//! placed in lane 0 and edges are wired commit→parent, which exercises the whole
//! pipeline and the data types end to end.

use std::collections::HashMap;

use crate::model::{CommitSummary, Edge, Graph, GraphRow};

/// Lay commits out into a [`Graph`]. `commits` must be newest-first.
pub fn layout(commits: Vec<CommitSummary>) -> Graph {
    // Map each commit id to its row index so we can wire edges to parents.
    let index: HashMap<_, _> = commits
        .iter()
        .enumerate()
        .map(|(row, c)| (c.id.clone(), row))
        .collect();

    let mut rows = Vec::with_capacity(commits.len());
    let mut edges = Vec::new();

    for (row, commit) in commits.into_iter().enumerate() {
        for parent in &commit.parents {
            if let Some(&to_row) = index.get(parent) {
                edges.push(Edge {
                    from_row: row,
                    from_lane: 0,
                    to_row,
                    to_lane: 0,
                });
            }
        }
        rows.push(GraphRow {
            commit,
            row,
            lane: 0,
        });
    }

    let lane_count = if rows.is_empty() { 0 } else { 1 };
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

    #[test]
    fn empty_history_yields_empty_graph() {
        let g = layout(vec![]);
        assert!(g.rows.is_empty());
        assert!(g.edges.is_empty());
        assert_eq!(g.lane_count, 0);
    }

    #[test]
    fn linear_history_wires_parent_edges() {
        let g = layout(vec![
            commit("c", &["b"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(g.rows.len(), 3);
        assert_eq!(g.edges.len(), 2); // c->b, b->a
        assert_eq!(g.lane_count, 1);
        assert_eq!(g.rows[0].commit.id.short(), "c"); // newest at row 0
        assert_eq!(g.edges[0], Edge { from_row: 0, from_lane: 0, to_row: 1, to_lane: 0 });
    }

    #[test]
    fn dangling_parents_are_skipped() {
        // Parent "z" is outside the walked window — no edge should be emitted.
        let g = layout(vec![commit("a", &["z"])]);
        assert!(g.edges.is_empty());
    }
}
