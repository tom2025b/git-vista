//! Phase 1 demo data for the static vertical graph.
//!
//! This is **hardcoded, pre-laid-out fake data** — no repository is read and no
//! lanes are computed. Each commit's lane (column) is hand-authored right here
//! so the SVG view has something branchy to draw while the real `repo` reader
//! (Phase 3) and the lane-assignment algorithm (Phase 6) are still ahead of us.
//!
//! The only thing derived is the edge list: once every commit has a fixed
//! `(row, lane)`, a commit→parent edge is just a line between two known points.
//! That is plumbing for the renderer, not layout — Phase 6 is what will actually
//! decide lanes (from real history) and replace this module's fixtures.

use std::collections::HashMap;

use git_vista_core::model::{CommitSummary, Edge, Graph, GraphRow, Oid};

/// One hand-placed commit in the demo history.
struct FakeCommit {
    id: &'static str,
    /// The column this commit sits in — authored by hand, not computed.
    lane: usize,
    parents: &'static [&'static str],
    summary: &'static str,
    author: &'static str,
}

/// The demo history: ~18 commits with three side branches (`feature`, `topic`,
/// `release`) and three merges, newest-first (row 0 is the most recent, at the
/// top). Lanes are fixed here so the picture stays stable and obvious:
///
/// ```text
///   lane 0      lane 1      lane 2
///   (main)      (side)      (side)
/// ```
const HISTORY: &[FakeCommit] = &[
    FakeCommit { id: "c18", lane: 0, parents: &["c17"],        summary: "Polish vertical graph styling",   author: "Ada Lovelace" },
    FakeCommit { id: "c17", lane: 0, parents: &["c15", "c16"], summary: "Merge branch 'release' into main", author: "Grace Hopper" },
    FakeCommit { id: "c16", lane: 1, parents: &["c13"],        summary: "Write 1.0 release notes",          author: "Alan Turing" },
    FakeCommit { id: "c15", lane: 0, parents: &["c14", "c11"], summary: "Merge branch 'topic' into main",   author: "Ada Lovelace" },
    FakeCommit { id: "c14", lane: 0, parents: &["c12"],        summary: "Speed up the initial paint",       author: "Grace Hopper" },
    FakeCommit { id: "c13", lane: 1, parents: &["c12"],        summary: "Bump version to 1.0.0-rc",         author: "Alan Turing" },
    FakeCommit { id: "c12", lane: 0, parents: &["c09"],        summary: "Tidy up the module layout",        author: "Ada Lovelace" },
    FakeCommit { id: "c11", lane: 2, parents: &["c10"],        summary: "Add edge curve rendering",         author: "Grace Hopper" },
    FakeCommit { id: "c10", lane: 2, parents: &["c09"],        summary: "Sketch the topic experiment",      author: "Alan Turing" },
    FakeCommit { id: "c09", lane: 0, parents: &["c08"],        summary: "Wire fake data into the view",     author: "Ada Lovelace" },
    FakeCommit { id: "c08", lane: 0, parents: &["c07", "c05"], summary: "Merge branch 'feature' into main", author: "Grace Hopper" },
    FakeCommit { id: "c07", lane: 0, parents: &["c06"],        summary: "Document the SVG layout",          author: "Alan Turing" },
    FakeCommit { id: "c06", lane: 0, parents: &["c03"],        summary: "Set up the SVG canvas",            author: "Ada Lovelace" },
    FakeCommit { id: "c05", lane: 1, parents: &["c04"],        summary: "Tune node spacing",                author: "Grace Hopper" },
    FakeCommit { id: "c04", lane: 1, parents: &["c03"],        summary: "Draft the commit node component",  author: "Alan Turing" },
    FakeCommit { id: "c03", lane: 0, parents: &["c02"],        summary: "Add the core graph model",         author: "Ada Lovelace" },
    FakeCommit { id: "c02", lane: 0, parents: &["c01"],        summary: "Scaffold the Leptos app",          author: "Grace Hopper" },
    FakeCommit { id: "c01", lane: 0, parents: &[],             summary: "Initial commit",                   author: "Alan Turing" },
];

/// Build the static demo [`Graph`] from [`HISTORY`].
///
/// Rows come straight from the table (row index = position, lane = the authored
/// column). Edges are then filled in by looking each parent up by id — pure
/// renderer plumbing between already-fixed points, not lane assignment.
pub fn fake_graph() -> Graph {
    // Newest commit gets the largest timestamp; step back an hour per row.
    let base_time: i64 = 1_719_500_000;

    let rows: Vec<GraphRow> = HISTORY
        .iter()
        .enumerate()
        .map(|(row, c)| GraphRow {
            commit: CommitSummary {
                id: Oid(c.id.into()),
                parents: c.parents.iter().map(|p| Oid((*p).into())).collect(),
                summary: c.summary.into(),
                author: c.author.into(),
                time: base_time - (row as i64) * 3_600,
            },
            row,
            lane: c.lane,
            refs: Vec::new(),
            // Phase 7 colours by branch; the fixture has no refs, so fall back to
            // the lane index as a stand-in (this module is test-only now).
            color: c.lane,
        })
        .collect();

    // id -> (row, lane), so a parent reference becomes a concrete endpoint.
    let pos: HashMap<&str, (usize, usize)> = HISTORY
        .iter()
        .enumerate()
        .map(|(row, c)| (c.id, (row, c.lane)))
        .collect();

    let mut edges = Vec::new();
    for c in HISTORY {
        let (from_row, from_lane) = pos[c.id];
        for parent in c.parents {
            if let Some(&(to_row, to_lane)) = pos.get(parent) {
                edges.push(Edge {
                    from_row,
                    from_lane,
                    to_row,
                    to_lane,
                });
            }
        }
    }

    let lane_count = HISTORY.iter().map(|c| c.lane).max().map_or(0, |m| m + 1);
    Graph {
        rows,
        edges,
        lane_count,
        repo_url: None,
        remote_commits: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_graph_is_well_formed() {
        let g = fake_graph();

        // 18 hand-placed commits, newest-first with sequential row indices.
        assert_eq!(g.rows.len(), 18);
        for (i, r) in g.rows.iter().enumerate() {
            assert_eq!(r.row, i, "rows are ordered top to bottom");
        }

        // Three columns (main + two side branches) and at least one merge.
        assert_eq!(g.lane_count, 3);
        assert!(g.rows.iter().any(|r| r.commit.is_merge()), "has merges");
    }

    #[test]
    fn every_edge_connects_real_rows() {
        let g = fake_graph();
        let n = g.rows.len();
        for e in &g.edges {
            assert!(e.from_row < n && e.to_row < n, "edge endpoints are in range");
            // Parents are older, so they sit further down the graph.
            assert!(e.to_row > e.from_row, "child sits above its parent");
            assert!(e.from_lane < g.lane_count && e.to_lane < g.lane_count);
        }
    }
}
