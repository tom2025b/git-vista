//! Colour tests: the per-branch first-parent-chain colouring, the trunk slot,
//! and the branch stubs the colouring pass produces (a local branch owning no
//! commits of its own).

use super::*;
use crate::color::stable_color_slot;
use crate::layout::{layout, layout_with_refs};

/// A branch created from an existing commit (its tip already owned by another
/// branch) is drawn as a distinct stub line, not a second badge: it owns no
/// commits, gets its own lane and a colour distinct from the branch it forked
/// off, and its name is removed from the shared commit's badges.
#[test]
fn branch_with_no_own_commits_becomes_a_distinct_stub() {
    // c2 <- c1 <- c0 ; `main` and a freshly-created `feature` both at c2.
    let commits = vec![
        commit("c2", &["c1"]),
        commit("c1", &["c0"]),
        commit("c0", &[]),
    ];
    let refs = vec![
        gitref("HEAD", RefKind::Head, "c2"),
        gitref("main", RefKind::Branch, "c2"),
        gitref("feature", RefKind::Branch, "c2"),
    ];
    // We're on `main` (HEAD) and just created `feature` from its tip.
    let g = layout_with_refs(commits, refs, Some("main"));

    // `feature` owns nothing, so it's a stub — not a badge on c2.
    assert!(!ref_names(&g, "c2").contains(&"feature".to_string()));
    assert!(ref_names(&g, "c2").contains(&"main".to_string()));

    assert_eq!(g.stubs.len(), 1);
    let stub = &g.stubs[0];
    assert_eq!(stub.name, "feature");
    // Anchored to c2's row, in its own lane to the right, distinct colour.
    assert_eq!(stub.anchor_row, 0);
    assert_eq!(stub.anchor_lane, lane_of(&g, "c2"));
    assert!(stub.lane >= g.rows.iter().map(|r| r.lane).max().unwrap());
    assert_ne!(stub.color, color_of(&g, "c2"));
    // The lane count was widened to include the stub lane (so the label
    // column sits to the right of it).
    assert!(g.lane_count > stub.lane);
}

/// Issue #30: a stub has its own identity and the *correct* tip commit. The
/// stub's anchor row must be the exact commit its branch points at — that hash
/// is what the UI's menu shows and what "branch from the stub" forks off, so if
/// it drifted to some other commit, the hollow dot would misrepresent the
/// branch and branching would target the wrong commit.
#[test]
fn a_stub_anchor_is_its_branchs_own_tip_commit() {
    // A coloured side branch `feature` (tip F2), plus a brand-new branch `fork`
    // created at feature's *tip* F2 — so `fork` owns nothing and is a stub.
    //   D  main tip
    //   F2 feature tip  <- `fork` also points here
    //   C
    //   F1
    //   B  fork point
    //   A
    let commits = vec![
        commit("D", &["C"]),
        commit("F2", &["F1"]),
        commit("C", &["B"]),
        commit("F1", &["B"]),
        commit("B", &["A"]),
        commit("A", &[]),
    ];
    let refs = vec![
        gitref("HEAD", RefKind::Head, "D"),
        gitref("main", RefKind::Branch, "D"),
        gitref("feature", RefKind::Branch, "F2"),
        gitref("fork", RefKind::Branch, "F2"),
    ];
    let g = layout_with_refs(commits, refs, Some("main"));

    // `feature` is the real line; `fork` (created at its tip) is the stub.
    let stub = g.stubs.iter().find(|s| s.name == "fork").expect("fork is a stub");
    assert!(g.stubs.iter().all(|s| s.name != "feature"), "feature is a real line");
    // The stub's tip is exactly F2 — feature's own tip, the commit `fork`
    // points at — so branching from the stub forks off F2, not some parent.
    assert_eq!(
        g.rows[stub.anchor_row].commit.id.0, "F2",
        "the stub's tip must be its branch's own commit"
    );
    // And its colour slot is distinct from the branch it forked off.
    assert_ne!(stub.color, color_of(&g, "F2"), "a new branch differs from its parent");
}

/// Issue #30: several branches created at the *same* commit cascade — each is
/// its own stub, stacked so a deeper one forks off the previous stub's tip (one
/// lane to the right) rather than every stub fanning back to the shared commit.
/// This is what makes "create a branch from a hollow dot" draw a new dot off
/// the dot you clicked.
#[test]
fn stubs_sharing_a_commit_cascade_off_one_another() {
    // main at c2; `aaa` and `bbb` both freshly created at c2 (own nothing).
    let commits = vec![
        commit("c2", &["c1"]),
        commit("c1", &["c0"]),
        commit("c0", &[]),
    ];
    let refs = vec![
        gitref("HEAD", RefKind::Head, "c2"),
        gitref("main", RefKind::Branch, "c2"),
        gitref("aaa", RefKind::Branch, "c2"),
        gitref("bbb", RefKind::Branch, "c2"),
    ];
    let g = layout_with_refs(commits, refs, Some("main"));

    assert_eq!(g.stubs.len(), 2, "both new branches are stubs");
    // Ordered deterministically by name: aaa is the base of the cascade, bbb
    // stacks above it.
    let aaa = g.stubs.iter().find(|s| s.name == "aaa").unwrap();
    let bbb = g.stubs.iter().find(|s| s.name == "bbb").unwrap();
    // Both anchor at the same commit (c2, row 0).
    assert_eq!(aaa.anchor_row, 0);
    assert_eq!(bbb.anchor_row, 0);
    // First forks off the commit; second forks off the first (one deeper, one
    // lane further right — that's how the connector finds the previous tip).
    assert_eq!(aaa.depth, 0, "first stub forks off the commit");
    assert_eq!(bbb.depth, 1, "second stub forks off the first stub's tip");
    assert_eq!(bbb.lane, aaa.lane + 1, "the deeper stub sits one lane right");
    // Distinct colours, and neither is the trunk slot.
    assert_ne!(aaa.color, bbb.color);
    assert_ne!(aaa.color, 0);
    assert_ne!(bbb.color, 0);
}

/// Issue #28: a branch created at an *interior* commit of an existing branch's
/// line must become a stub forking off that commit — it must NOT claim the
/// lower half of the existing branch's first-parent chain. Ordering by name
/// used to let `aaa` (created at F1, inside `feature`) claim F1..base and split
/// `feature`'s colour in two, drawing a spurious line back to an earlier dot.
/// Now the branch with the newer tip (`feature`, tip F2) owns the whole line
/// and `aaa` is a stub.
#[test]
fn a_branch_at_an_interior_commit_is_a_stub_not_a_stolen_line() {
    // main: D -> C -> B -> A ; feature: F2 -> F1 -> B ; aaa points at F1.
    // Rows are newest-first (row 0 at top).
    let commits = vec![
        commit("D", &["C"]),  // 0  main tip
        commit("F2", &["F1"]), // 1  feature tip
        commit("C", &["B"]),  // 2
        commit("F1", &["B"]), // 3  aaa points here (interior of feature)
        commit("B", &["A"]),  // 4  fork point
        commit("A", &[]),     // 5
    ];
    let refs = vec![
        gitref("HEAD", RefKind::Head, "D"),
        gitref("main", RefKind::Branch, "D"),
        gitref("feature", RefKind::Branch, "F2"),
        gitref("aaa", RefKind::Branch, "F1"),
    ];
    let g = layout_with_refs(commits, refs, Some("main"));

    // `feature` keeps ONE colour down its whole line (F2 and F1 match), and
    // it's distinct from main's trunk colour.
    assert_eq!(
        color_of(&g, "F2"),
        color_of(&g, "F1"),
        "feature must not be split in two by aaa stealing F1"
    );
    assert_ne!(color_of(&g, "F1"), color_of(&g, "D"), "feature isn't the trunk");
    assert_eq!(color_of(&g, "D"), 0, "main (checked out) owns the trunk colour");

    // `aaa` owns nothing → it's a stub anchored at F1, not a badge, not a line.
    assert_eq!(g.stubs.len(), 1);
    assert_eq!(g.stubs[0].name, "aaa");
    assert_eq!(g.stubs[0].anchor_row, 3, "stub forks off F1's dot");
    assert!(!ref_names(&g, "F1").contains(&"aaa".to_string()));
    // `feature` stays a real line: it's badged on its tip, not a stub.
    assert!(g.stubs.iter().all(|s| s.name != "feature"));
    assert!(ref_names(&g, "F2").contains(&"feature".to_string()));
}

/// The "commit on a fresh branch stub" bug: a commit created on a side
/// branch (without checking it out) is the newest commit, so the lane walk
/// used to hand it lane 0 — gluing it on top of the trunk as one vertical
/// line — and the trunk recolour pass then painted it blue. The branch
/// looked like it had vanished into main. It must fork right into its own
/// lane with its own colour, badge on the new commit.
#[test]
fn a_commit_on_a_side_branch_forks_out_instead_of_absorbing_the_trunk() {
    //   X   igdj's first commit (newest; must fork right)
    //   T   main tip, checked out
    //   B
    let commits = vec![
        commit("X", &["T"]),
        commit("T", &["B"]),
        commit("B", &[]),
    ];
    let refs = vec![
        gitref("HEAD", RefKind::Head, "T"),
        gitref("main", RefKind::Branch, "T"),
        gitref("igdj", RefKind::Branch, "X"),
    ];
    let g = layout_with_refs(commits, refs, Some("main"));
    assert_well_formed(&g);

    // The trunk keeps lane 0 top to bottom; the side commit forks right.
    assert_eq!(lane_of(&g, "T"), 0, "main's tip stays in the trunk lane");
    assert_eq!(lane_of(&g, "B"), 0);
    assert_eq!(lane_of(&g, "X"), 1, "the side-branch commit must not take the trunk lane");
    // …and keeps its own colour: the trunk recolour pass is lane-gated, so
    // it can no longer absorb it.
    assert_eq!(color_of(&g, "T"), 0);
    assert_ne!(color_of(&g, "X"), 0, "the side branch keeps a distinct colour");
    // igdj is a real line now (badged on its commit), not a stub.
    assert!(g.stubs.is_empty(), "a branch with a commit of its own is no stub");
    assert!(ref_names(&g, "X").contains(&"igdj".to_string()));
}

#[test]
fn linear_history_is_one_colour() {
    let g = layout(vec![
        commit("c", &["b"]),
        commit("b", &["a"]),
        commit("a", &[]),
    ]);
    // Nothing branches, so every commit shares one branch colour (with no
    // refs there's no trunk, so it's the synthetic line's own stable slot).
    let first = g.rows[0].color;
    assert!(
        g.rows.iter().all(|r| r.color == first),
        "one branch, one colour"
    );
}

/// The July test round's issue #6, pinned: a stub that takes its first
/// commit becomes a line of exactly the colour the stub already had — so
/// on screen the stub visibly *grows into* its branch instead of the new
/// commit appearing to land on a differently-coloured line. (Colour is a
/// pure function of the branch name.)
#[test]
fn a_stub_keeps_its_colour_when_its_first_commit_arrives() {
    // Before: `topic` freshly created at main's tip T — a stub.
    let before = layout_with_refs(
        vec![commit("T", &["B"]), commit("B", &[])],
        vec![
            gitref("HEAD", RefKind::Head, "T"),
            gitref("main", RefKind::Branch, "T"),
            gitref("topic", RefKind::Branch, "T"),
        ],
        Some("main"),
    );
    let stub = before.stubs.iter().find(|s| s.name == "topic").expect("topic is a stub");
    let stub_color = stub.color;

    // After: `topic` takes its first (empty) commit X.
    let after = layout_with_refs(
        vec![commit("X", &["T"]), commit("T", &["B"]), commit("B", &[])],
        vec![
            gitref("HEAD", RefKind::Head, "T"),
            gitref("main", RefKind::Branch, "T"),
            gitref("topic", RefKind::Branch, "X"),
        ],
        Some("main"),
    );
    assert!(after.stubs.is_empty(), "topic owns a commit now — no stub");
    assert_eq!(
        color_of(&after, "X"),
        stub_color,
        "the line must wear the colour the stub had"
    );
    // And it forks right of the trunk rather than extending it.
    assert_eq!(lane_of(&after, "T"), 0);
    assert_ne!(lane_of(&after, "X"), 0);
    // Main is untouched by the operation: still blue, still lane 0.
    assert_eq!(color_of(&after, "T"), 0);
}

/// Issue #30: `main` owns the trunk colour (slot 0, the one blue line) even
/// when a *different* branch is checked out. Here HEAD is on `feature`, yet
/// `main`'s line must still be blue and `feature` a distinct non-trunk colour.
#[test]
fn main_owns_the_trunk_colour_even_when_not_checked_out() {
    let g = layout_with_refs(
        vec![
            commit("M", &["C", "D"]),
            commit("C", &["B"]),
            commit("D", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ],
        vec![
            gitref("HEAD", RefKind::Head, "D"),
            gitref("main", RefKind::Branch, "M"),
            gitref("feature", RefKind::Branch, "D"),
        ],
        // Checked out on `feature`, not `main`.
        Some("feature"),
    );
    assert_eq!(color_of(&g, "M"), 0, "main is the trunk (slot 0) regardless of HEAD");
    assert_ne!(color_of(&g, "D"), 0, "the checked-out feature is not the trunk");
    assert!(g.stubs.is_empty(), "both branches own commits — neither is a stub");
}

/// A branch ahead of `main` (its first-parent chain runs through main's tip)
/// forks *off* the trunk tip into its own lane and colour — even when it's
/// the checked-out branch. An earlier design instead extended the trunk's
/// lane and blue upward through such a branch, which made the same line
/// flip between blue and its own colour depending on what was checked out —
/// the "main keeps changing colour" instability from the July test round.
/// The trunk line now always ends at main's own tip, whoever is checked out.
#[test]
fn a_branch_ahead_of_main_forks_off_the_trunk_tip_instead_of_extending_it() {
    //   E   feature tip (checked out, ahead of main)
    //   D   feature
    //   C   main tip
    //   | S side tip (off B)
    //   B
    //   A
    let commits = vec![
        commit("E", &["D"]),
        commit("D", &["C"]),
        commit("S", &["B"]),
        commit("C", &["B"]),
        commit("B", &["A"]),
        commit("A", &[]),
    ];
    let refs = vec![
        gitref("HEAD", RefKind::Head, "E"),
        gitref("main", RefKind::Branch, "C"),
        gitref("feature", RefKind::Branch, "E"),
        gitref("side", RefKind::Branch, "S"),
    ];
    // Checked out on `feature`, which is ahead of `main`.
    let g = layout_with_refs(commits, refs, Some("feature"));
    assert_well_formed(&g);

    // The trunk keeps lane 0 and the trunk colour, ending at its own tip.
    for c in ["C", "B", "A"] {
        assert_eq!(lane_of(&g, c), 0, "{c} stays on the trunk lane");
        assert_eq!(color_of(&g, c), 0, "{c} keeps the trunk colour");
    }
    // The ahead branch forks right in its own stable colour — the same
    // whether or not it's checked out.
    for c in ["E", "D"] {
        assert_ne!(lane_of(&g, c), 0, "{c} must not extend the trunk lane");
        assert_eq!(color_of(&g, c), stable_color_slot("feature"));
    }
    let g2 = layout_with_refs(
        vec![
            commit("E", &["D"]),
            commit("D", &["C"]),
            commit("S", &["B"]),
            commit("C", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ],
        vec![
            gitref("HEAD", RefKind::Head, "C"),
            gitref("main", RefKind::Branch, "C"),
            gitref("feature", RefKind::Branch, "E"),
            gitref("side", RefKind::Branch, "S"),
        ],
        Some("main"),
    );
    // The HEAD badge moves with the checkout, but the geometry and colours
    // must not: same rows, same lanes, same colour slots.
    let shape = |g: &Graph| {
        g.rows
            .iter()
            .map(|r| (r.commit.id.0.clone(), r.lane, r.color))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&g), shape(&g2), "checkout state must not change lanes or colours");
}

#[test]
fn each_branch_gets_its_own_stable_colour() {
    // HEAD on main; a feature branch tip at D. Main's first-parent chain
    // (M→C→B→A) is one colour; the feature line (D) is a different one.
    //
    //   M        merge[C, D]
    //   |\
    //   C D      (D = feature tip)
    //   |/
    //   B
    //   |
    //   A
    let g = layout_with_refs(
        vec![
            commit("M", &["C", "D"]),
            commit("C", &["B"]),
            commit("D", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ],
        vec![
            gitref("HEAD", RefKind::Head, "M"),
            gitref("main", RefKind::Branch, "M"),
            gitref("feature", RefKind::Branch, "D"),
        ],
        Some("main"),
    );

    // The whole mainline (incl. the shared base B/A) is HEAD's branch colour.
    let main = color_of(&g, "M");
    for c in ["M", "C", "B", "A"] {
        assert_eq!(color_of(&g, c), main, "{c} is on the main line");
    }
    assert_eq!(main, 0, "HEAD's branch takes colour slot 0 (the trunk)");
    // The feature commit is a different, consistent colour.
    assert_ne!(color_of(&g, "D"), main, "the feature branch differs");
}

#[test]
fn a_tag_only_side_commit_still_gets_a_colour() {
    // A side commit S reachable only as M's second parent, with no branch ref
    // (only a tag). It must still be coloured — via the synthetic fallback —
    // and distinct from the trunk.
    //
    //   M     merge[C, S]
    //   |\
    //   C S   (S tagged, no branch)
    //   |/
    //   B
    let g = layout_with_refs(
        vec![
            commit("M", &["C", "S"]),
            commit("C", &["B"]),
            commit("S", &["B"]),
            commit("B", &[]),
        ],
        vec![
            gitref("HEAD", RefKind::Head, "M"),
            gitref("main", RefKind::Branch, "M"),
            gitref("v2", RefKind::Tag, "S"),
        ],
        Some("main"),
    );
    assert_eq!(ref_names(&g, "S"), vec!["v2"], "the tag still badges S");
    assert_ne!(
        color_of(&g, "S"),
        color_of(&g, "M"),
        "the un-branched side line gets its own colour"
    );
    // Every row is coloured (no commit left out).
    assert_eq!(g.rows.len(), 4);
}
