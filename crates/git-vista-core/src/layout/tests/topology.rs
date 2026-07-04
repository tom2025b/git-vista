//! Topology tests: lane assignment, edge wiring, order normalisation, and the
//! determinism of the layout.

use super::*;
use crate::layout::{layout, layout_with_refs};

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
    // mainline's edges don't change lanes. (Rows follow the deterministic
    // time-then-id order; with every time equal here, main1 sorts ahead of
    // side2.)
    assert_eq!(
        edge(&g, "side2", "side1"),
        Edge {
            from_row: 3,
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
            to_row: 2,
            to_lane: 0
        }
    );
}

/// Same-second commits (every burst of test commits) must lay out
/// identically regardless of the order the git walk happened to emit them
/// in — the walker's tie order shifts when the tip set changes, and that
/// used to reshuffle the whole graph after unrelated operations.
#[test]
fn layout_is_deterministic_whatever_order_ties_arrive_in() {
    let refs = || {
        vec![
            gitref("HEAD", RefKind::Head, "M"),
            gitref("main", RefKind::Branch, "M"),
            gitref("one", RefKind::Branch, "S1"),
            gitref("two", RefKind::Branch, "S2"),
        ]
    };
    // M (main tip) and two side tips S1/S2 all share commit time 0.
    let a = layout_with_refs(
        vec![
            commit("M", &["B"]),
            commit("S1", &["B"]),
            commit("S2", &["B"]),
            commit("B", &[]),
        ],
        refs(),
        Some("main"),
    );
    let b = layout_with_refs(
        vec![
            commit("S2", &["B"]),
            commit("M", &["B"]),
            commit("S1", &["B"]),
            commit("B", &[]),
        ],
        refs(),
        Some("main"),
    );
    assert_eq!(a, b, "input order of same-time commits must not matter");
    assert_well_formed(&a);
}

/// A child listed *after* its parent (commit-time clock skew — the walk
/// upstream orders by time alone) is hoisted back above it, so the lane
/// walk's children-first assumption always holds.
#[test]
fn a_time_skewed_child_is_still_laid_out_above_its_parent() {
    // Parent P claims a NEWER time than its child C, so a time-only sort
    // puts P first. The topological order must still emit C above P.
    let mut p = commit("P", &[]);
    p.time = 100;
    let mut c = commit("C", &["P"]);
    c.time = 50;
    let g = layout(vec![p, c]);
    assert_well_formed(&g);
    assert_eq!(g.rows[0].commit.id.0, "C", "the child draws above its parent");
    assert_eq!(g.rows[1].commit.id.0, "P");
}
