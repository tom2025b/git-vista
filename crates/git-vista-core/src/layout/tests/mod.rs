//! Tests for the layout passes, split by concern to mirror the impl modules.
//!
//! This split is move-only — every test kept its body and its name. The shared
//! fixtures/helpers ([`commit`], [`gitref`], [`lane_of`], [`edge`], [`color_of`],
//! [`ref_names`], [`assert_well_formed`]) live here at the root; each submodule
//! pulls them in with `use super::*` and imports the layout entry points it
//! exercises.
//!
//!   * [`topology`] — lane assignment, edge wiring, ordering, determinism.
//!   * [`color`]    — per-branch colouring, the trunk slot, and branch stubs
//!                    (which the colouring pass produces).
//!   * [`badges`]   — attaching refs to their commits.

use crate::model::{CommitSummary, Edge, GitRef, Graph, Oid, RefKind};

mod badges;
mod color;
mod topology;

fn gitref(name: &str, kind: RefKind, target: &str) -> GitRef {
    GitRef {
        name: name.into(),
        kind,
        target: Oid(target.into()),
    }
}

fn color_of(g: &Graph, id: &str) -> usize {
    g.rows.iter().find(|r| r.commit.id.0 == id).unwrap().color
}

fn ref_names(g: &Graph, id: &str) -> Vec<String> {
    g.rows
        .iter()
        .find(|r| r.commit.id.0 == id)
        .unwrap()
        .refs
        .iter()
        .map(|r| r.name.clone())
        .collect()
}

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
