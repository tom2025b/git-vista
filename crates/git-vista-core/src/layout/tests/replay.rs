//! Replay-classification tests: the streaming classifier
//! ([`ReplayClassifier`](crate::layout::replay::ReplayClassifier)) proven to say
//! exactly what the whole-graph colour/stub/badge algorithm says.
//!
//! Paged history cannot run the whole-graph passes — it never holds the whole
//! graph. So it runs a per-row classifier instead, and the *only* thing that
//! makes that safe is this oracle: every existing colour/stub fixture in the
//! crate is fed through both algorithms and the four observable outputs are
//! compared — per-row colours, badges by commit id, stubs by anchor, and the
//! frame's stable named slots.
//!
//! The prefix half matters just as much. A page starting at row `n` replays
//! `[0, n)` with `emit = false`, which must still reconstruct every claim *and*
//! advance the cumulative stub offset while suppressing badges and stubs — page
//! 2's stub columns are numbered off page 1's suppressed rows. So each fixture
//! is also replayed at *every* page boundary, and the emitted tail must match
//! the oracle's tail exactly, `lane_offset` included.

use std::collections::HashSet;

use super::*;
use crate::color::stable_color_slot;
use crate::layout::layout_with_refs;
use crate::layout::replay::ReplayClassifier;
use crate::layout::stream::StreamLayout;
use crate::layout::topology::{stable_topo_order, trunk_reserve_tip};
use crate::model::{FrameStub, GraphRow, RefKind};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// [`commit`] with an explicit commit time — the shared fixture hardcodes 0, and
/// the clock-skew cases turn on time order.
fn at(id: &str, parents: &[&str], time: i64) -> CommitSummary {
    let mut c = commit(id, parents);
    c.time = time;
    c
}

/// One parameterized case: a commit set, its refs, and which branch is checked
/// out — i.e. exactly the three arguments [`layout_with_refs`] takes, so the
/// whole-graph oracle and the replay run see identical input.
struct Case {
    what: &'static str,
    commits: Vec<CommitSummary>,
    refs: Vec<GitRef>,
    head: Option<&'static str>,
}

/// The 12-commit DAG from the stream tests: a trunk (`a06..a12`), two merged
/// side lines, an octopus merge whose parent vector disagrees with arrival
/// order, a tag, a local branch `aaa` on an interior trunk commit (a stub), two
/// equal-second tips (`a12`/`b01`), a parent (`a07`) whose timestamp is newer
/// than every one of its children, and a local/remote collision on `a12`.
fn equivalence_fixture() -> (Vec<CommitSummary>, Vec<GitRef>) {
    let commits = vec![
        at("a08", &["a07"], 992),
        at("a12", &["a11"], 1000),
        at("v01", &["a07"], 993),
        at("a09", &["a08", "u01", "v01"], 995),
        at("s01", &["a08"], 997),
        at("a06", &[], 991),
        at("b01", &["a09"], 1000),
        at("a11", &["a10", "s02"], 999),
        at("u01", &["a07"], 994),
        at("a10", &["a09"], 996),
        at("s02", &["s01"], 998),
        at("a07", &["a06"], 1001),
    ];
    let refs = vec![
        gitref("main", RefKind::Branch, "a12"),
        gitref("side", RefKind::Branch, "s02"),
        gitref("aaa", RefKind::Branch, "a10"),
        gitref("v1.0", RefKind::Tag, "a08"),
        gitref("origin/main", RefKind::RemoteBranch, "a12"),
    ];
    (commits, refs)
}

/// Every colour/stub fixture in the crate, plus the cases the whole-graph tests
/// never had a fixture for (local `master` as the trunk, the checked-out branch
/// as the trunk, a local/remote collision on one tip, and same-anchor stubs whose
/// trunk ranks differ). Between them these cover every case the plan lists:
/// trunk `main`/`master`/checked-out priority, local-versus-remote collision,
/// equal tip row and name tie, the issue-#28 interior stub, merge convergence,
/// deleted-branch synthetic colour, same-anchor stub cascade, and clock skew.
fn cases() -> Vec<Case> {
    let mut cases = vec![
        // A branch freshly created on the trunk tip owns nothing: one stub.
        Case {
            what: "fresh branch on the trunk tip is a stub",
            commits: vec![
                commit("c2", &["c1"]),
                commit("c1", &["c0"]),
                commit("c0", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "c2"),
                gitref("main", RefKind::Branch, "c2"),
                gitref("feature", RefKind::Branch, "c2"),
            ],
            head: Some("main"),
        },
        // A stub anchored on another *branch's* own tip, not the trunk's.
        Case {
            what: "stub anchored on another branch's own tip",
            commits: vec![
                commit("D", &["C"]),
                commit("F2", &["F1"]),
                commit("C", &["B"]),
                commit("F1", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "D"),
                gitref("main", RefKind::Branch, "D"),
                gitref("feature", RefKind::Branch, "F2"),
                gitref("fork", RefKind::Branch, "F2"),
            ],
            head: Some("main"),
        },
        // Same-anchor cascade, broken by name alone (both losers are rank 3).
        Case {
            what: "same-anchor stub cascade, name tie",
            commits: vec![
                commit("c2", &["c1"]),
                commit("c1", &["c0"]),
                commit("c0", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "c2"),
                gitref("main", RefKind::Branch, "c2"),
                gitref("aaa", RefKind::Branch, "c2"),
                gitref("bbb", RefKind::Branch, "c2"),
            ],
            head: Some("main"),
        },
        // Same-anchor cascade whose losers have *different* trunk ranks: the
        // checked-out `feature` is rank 2 and `aaa` is rank 3, so the cascade
        // order is the priority key's, which is not name order.
        Case {
            what: "same-anchor stub cascade with mixed trunk ranks",
            commits: vec![
                commit("c2", &["c1"]),
                commit("c1", &["c0"]),
                commit("c0", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "c2"),
                gitref("main", RefKind::Branch, "c2"),
                gitref("feature", RefKind::Branch, "c2"),
                gitref("aaa", RefKind::Branch, "c2"),
            ],
            head: Some("feature"),
        },
        // Issue #28: a branch created at an *interior* commit of another
        // branch's line is a stub, not a stolen half-line.
        Case {
            what: "issue #28 interior stub",
            commits: vec![
                commit("D", &["C"]),
                commit("F2", &["F1"]),
                commit("C", &["B"]),
                commit("F1", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "D"),
                gitref("main", RefKind::Branch, "D"),
                gitref("feature", RefKind::Branch, "F2"),
                gitref("aaa", RefKind::Branch, "F1"),
            ],
            head: Some("main"),
        },
        // A commit on a side branch: newest row, but not the trunk's colour.
        Case {
            what: "commit on a side branch forks out",
            commits: vec![commit("X", &["T"]), commit("T", &["B"]), commit("B", &[])],
            refs: vec![
                gitref("HEAD", RefKind::Head, "T"),
                gitref("main", RefKind::Branch, "T"),
                gitref("igdj", RefKind::Branch, "X"),
            ],
            head: Some("main"),
        },
        // No refs at all: every line is a synthetic `~<short-id>` claim.
        Case {
            what: "no refs at all, every line synthetic",
            commits: vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])],
            refs: Vec::new(),
            head: None,
        },
        // A stub immediately before, and immediately after, its first commit.
        Case {
            what: "stub before its first commit",
            commits: vec![commit("T", &["B"]), commit("B", &[])],
            refs: vec![
                gitref("HEAD", RefKind::Head, "T"),
                gitref("main", RefKind::Branch, "T"),
                gitref("topic", RefKind::Branch, "T"),
            ],
            head: Some("main"),
        },
        Case {
            what: "stub after its first commit",
            commits: vec![commit("X", &["T"]), commit("T", &["B"]), commit("B", &[])],
            refs: vec![
                gitref("HEAD", RefKind::Head, "T"),
                gitref("main", RefKind::Branch, "T"),
                gitref("topic", RefKind::Branch, "X"),
            ],
            head: Some("main"),
        },
        // Trunk priority: `main` owns slot 0 even when it is not checked out.
        Case {
            what: "main owns the trunk when another branch is checked out",
            commits: vec![
                commit("M", &["C", "D"]),
                commit("C", &["B"]),
                commit("D", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "D"),
                gitref("main", RefKind::Branch, "M"),
                gitref("feature", RefKind::Branch, "D"),
            ],
            head: Some("feature"),
        },
        // A branch ahead of main forks off the trunk tip — checked out or not.
        Case {
            what: "branch ahead of main, checked out",
            commits: vec![
                commit("E", &["D"]),
                commit("D", &["C"]),
                commit("S", &["B"]),
                commit("C", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "E"),
                gitref("main", RefKind::Branch, "C"),
                gitref("feature", RefKind::Branch, "E"),
                gitref("side", RefKind::Branch, "S"),
            ],
            head: Some("feature"),
        },
        Case {
            what: "branch ahead of main, main checked out",
            commits: vec![
                commit("E", &["D"]),
                commit("D", &["C"]),
                commit("S", &["B"]),
                commit("C", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "C"),
                gitref("main", RefKind::Branch, "C"),
                gitref("feature", RefKind::Branch, "E"),
                gitref("side", RefKind::Branch, "S"),
            ],
            head: Some("main"),
        },
        // Merge convergence: two claims meet on the shared base.
        Case {
            what: "merge convergence on a shared base",
            commits: vec![
                commit("M", &["C", "D"]),
                commit("C", &["B"]),
                commit("D", &["B"]),
                commit("B", &["A"]),
                commit("A", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "M"),
                gitref("main", RefKind::Branch, "M"),
                gitref("feature", RefKind::Branch, "D"),
            ],
            head: Some("main"),
        },
        // Deleted-branch synthetic colour: a side line reachable only as a
        // merge's second parent, carrying nothing but a tag.
        Case {
            what: "deleted-branch side line gets a synthetic colour",
            commits: vec![
                commit("M", &["C", "S"]),
                commit("C", &["B"]),
                commit("S", &["B"]),
                commit("B", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "M"),
                gitref("main", RefKind::Branch, "M"),
                gitref("v2", RefKind::Tag, "S"),
            ],
            head: Some("main"),
        },
        // Trunk priority, rank 1: local `master` outranks the checked-out
        // branch when no local `main` exists.
        Case {
            what: "master is the trunk when main is absent",
            commits: vec![
                commit("T1", &["M2"]),
                commit("M3", &["M2"]),
                commit("M2", &["M1"]),
                commit("M1", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "T1"),
                gitref("master", RefKind::Branch, "M3"),
                gitref("topic", RefKind::Branch, "T1"),
            ],
            head: Some("topic"),
        },
        // Trunk priority, ranks 0 and 1 together: `main` outranks a local
        // `master` sitting on an interior commit of main's own line, which
        // demotes `master` to a stub.
        Case {
            what: "main outranks a local master on its own line",
            commits: vec![
                commit("c0", &["c1"]),
                commit("c1", &["c2"]),
                commit("c2", &["c3"]),
                commit("c3", &["c4"]),
                commit("c4", &["c5"]),
                commit("c5", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "c0"),
                gitref("main", RefKind::Branch, "c0"),
                gitref("master", RefKind::Branch, "c2"),
                gitref("zzz", RefKind::Branch, "c4"),
            ],
            head: Some("main"),
        },
        // Trunk priority, rank 2: with neither main nor master, the checked-out
        // branch owns slot 0.
        Case {
            what: "checked-out branch is the trunk when main and master are absent",
            commits: vec![commit("X", &["T"]), commit("T", &["B"]), commit("B", &[])],
            refs: vec![
                gitref("HEAD", RefKind::Head, "T"),
                gitref("topic", RefKind::Branch, "T"),
                gitref("other", RefKind::Branch, "X"),
            ],
            head: Some("topic"),
        },
        // Local versus remote on one tip, twice over: the local ref always wins
        // its own tip and the remote stays an ordinary badge — and a remote with
        // a tip of its own still claims that line.
        Case {
            what: "local and remote refs collide on one tip",
            commits: vec![
                commit("c2", &["c1"]),
                commit("s1", &["c1"]),
                commit("c1", &["c0"]),
                commit("c0", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "c2"),
                gitref("main", RefKind::Branch, "c2"),
                gitref("feature", RefKind::Branch, "s1"),
                gitref("origin/main", RefKind::RemoteBranch, "c2"),
                gitref("origin/feature", RefKind::RemoteBranch, "s1"),
            ],
            head: Some("main"),
        },
        Case {
            what: "a remote-only line claims its own colour",
            commits: vec![
                commit("c2", &["c1"]),
                commit("s1", &["c1"]),
                commit("c1", &["c0"]),
                commit("c0", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "c2"),
                gitref("main", RefKind::Branch, "c2"),
                gitref("origin/main", RefKind::RemoteBranch, "c2"),
                gitref("origin/topic", RefKind::RemoteBranch, "s1"),
            ],
            head: Some("main"),
        },
        // Equal tip row / equal commit time, broken by name.
        Case {
            what: "equal-time tips broken by name",
            commits: vec![
                commit("M", &["B"]),
                commit("S1", &["B"]),
                commit("S2", &["B"]),
                commit("B", &[]),
            ],
            refs: vec![
                gitref("HEAD", RefKind::Head, "M"),
                gitref("main", RefKind::Branch, "M"),
                gitref("one", RefKind::Branch, "S1"),
                gitref("two", RefKind::Branch, "S2"),
            ],
            head: Some("main"),
        },
        // Clock skew: a parent claiming a newer time than its child.
        Case {
            what: "clock-skewed child above its parent",
            commits: vec![at("P", &[], 100), at("C", &["P"], 50)],
            refs: Vec::new(),
            head: None,
        },
        Case {
            what: "clock-skewed parent under a decorated trunk",
            commits: vec![at("T", &["P"], 50), at("P", &["R"], 100), at("R", &[], 10)],
            refs: vec![
                gitref("HEAD", RefKind::Head, "T"),
                gitref("main", RefKind::Branch, "T"),
            ],
            head: Some("main"),
        },
    ];

    let (commits, refs) = equivalence_fixture();
    cases.push(Case {
        what: "the 12-commit equivalence fixture",
        commits,
        refs,
        head: Some("main"),
    });
    cases
}

// ---------------------------------------------------------------------------
// The whole-graph oracle, expressed in paged terms
// ---------------------------------------------------------------------------

/// Each row's badges, keyed by the commit they sit on, in row order. Compares
/// whole [`GitRef`]s (name, kind and target) and their order, not just names.
fn badges_of(rows: &[GraphRow]) -> Vec<(Oid, Vec<GitRef>)> {
    rows.iter()
        .map(|r| (r.commit.id.clone(), r.refs.clone()))
        .collect()
}

/// The oracle side of [`badges_of`].
fn badges_by_oid(whole: &Graph) -> Vec<(Oid, Vec<GitRef>)> {
    badges_of(&whole.rows)
}

/// The commit-lane high-water: the width of the graph *before* [`decorate`]
/// widens `lane_count` to cover the stub columns. A [`FrameStub`]'s
/// `lane_offset` is relative to this, never to `Graph::lane_count`.
fn commit_lane_high_water(whole: &Graph) -> usize {
    whole.rows.iter().map(|r| r.lane + 1).max().unwrap_or(0)
}

/// Convert the whole graph's legacy [`BranchStub`](crate::model::BranchStub)
/// cascade into the anchor-OID-relative [`FrameStub`] a page carries: the row
/// index becomes the anchor commit's id, and the absolute lane becomes an offset
/// past the commit lanes. Test-oracle code only — production paging emits
/// `FrameStub` directly.
fn frame_stubs_by_anchor(whole: &Graph) -> Vec<FrameStub> {
    let high_water = commit_lane_high_water(whole);
    whole
        .stubs
        .iter()
        .map(|s| {
            assert!(
                s.lane >= high_water,
                "a stub column sits right of every commit lane"
            );
            FrameStub {
                name: s.name.clone(),
                anchor_commit: whole.rows[s.anchor_row].commit.id.clone(),
                lane_offset: s.lane - high_water,
                color: s.color,
                depth: s.depth,
            }
        })
        .collect()
}

/// The Frame's stable named slots: every branch ref in input order paired with
/// the palette slot the whole-graph colouring gives its *name* — slot 0 for the
/// trunk (local `main`, then local `master`, then the checked-out local branch),
/// otherwise the name's stable hash.
///
/// Grounded against `whole` rather than merely re-derived: every stub's colour
/// and the trunk's own row colour must agree with what this claims.
///
/// Takes `head` as well as the plan's `(&whole, &refs)` because the trunk rule
/// itself does (`layout/color.rs:99-105`): with neither a local `main` nor a
/// local `master`, the checked-out branch owns slot 0, and nothing in `whole`
/// names it (`HEAD` is a badge on a commit, not a branch name).
fn named_slots(whole: &Graph, refs: &[GitRef], head: Option<&str>) -> Vec<(String, usize)> {
    let has_local = |name: &str| {
        refs.iter()
            .any(|r| matches!(r.kind, RefKind::Branch) && r.name == name)
    };
    let trunk_name: Option<&str> = if has_local("main") {
        Some("main")
    } else if has_local("master") {
        Some("master")
    } else {
        head.filter(|h| has_local(h))
    };
    let slot_of = |name: &str| {
        if Some(name) == trunk_name {
            0
        } else {
            stable_color_slot(name)
        }
    };

    // Grounding 1: a stub's colour is its name's slot.
    for stub in &whole.stubs {
        assert_eq!(
            slot_of(&stub.name),
            stub.color,
            "stub {} wears its name's slot",
            stub.name
        );
    }
    // Grounding 2: the trunk's own tip row is slot 0.
    if let Some(trunk) = trunk_name {
        let tip = refs
            .iter()
            .find(|r| matches!(r.kind, RefKind::Branch) && r.name == trunk)
            .map(|r| r.target.clone());
        if let Some(row) = tip.and_then(|t| whole.rows.iter().find(|r| r.commit.id == t)) {
            assert_eq!(row.color, 0, "the trunk's tip row is slot 0");
        }
    }

    refs.iter()
        .filter(|r| r.is_branch())
        .map(|r| (r.name.clone(), slot_of(&r.name)))
        .collect()
}

/// `commit id -> row` over the whole graph, for filtering oracle stubs down to
/// the rows one page owns.
fn row_of(whole: &Graph, id: &Oid) -> usize {
    whole
        .rows
        .iter()
        .position(|r| &r.commit.id == id)
        .expect("a stub anchor is always in the window")
}

// ---------------------------------------------------------------------------
// The replay side
// ---------------------------------------------------------------------------

/// Lay a fixture out through the one lane algorithm and classify every row with
/// [`ReplayClassifier`], suppressing response output for rows `[0, split)` — the
/// prefix a page starting at `split` replays without emitting.
///
/// Returns only what a page would carry: the emitted rows and the stubs whose
/// anchor row it owns.
fn replay(
    normalized: &[CommitSummary],
    refs: &[GitRef],
    head: Option<&str>,
    split: usize,
) -> (Vec<GraphRow>, Vec<FrameStub>) {
    let present: HashSet<Oid> = normalized.iter().map(|c| c.id.clone()).collect();
    let trunk_tip = trunk_reserve_tip(refs, head).filter(|tip| present.contains(tip));
    let mut stream = StreamLayout::new(trunk_tip);
    for c in normalized {
        stream.push(c.clone(), |oid| present.contains(oid));
    }
    let chunk = stream.finish();

    let mut classifier = ReplayClassifier::new(refs, head);
    let mut rows = Vec::new();
    let mut stubs = Vec::new();
    for mut row in chunk.rows {
        let emit = row.row >= split;
        let produced = classifier.decorate(&mut row, emit);
        if emit {
            stubs.extend(produced);
            rows.push(row);
        } else {
            assert!(
                produced.is_empty(),
                "prefix replay returns no stubs (row {})",
                row.row
            );
            assert!(
                row.refs.is_empty(),
                "prefix replay attaches no badges (row {})",
                row.row
            );
        }
    }
    (rows, stubs)
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn whole_graph_and_replay_classifier_match() {
    for case in cases() {
        let what = case.what;
        let whole = layout_with_refs(case.commits.clone(), case.refs.clone(), case.head);
        let normalized = stable_topo_order(case.commits.clone());
        assert_eq!(normalized.len(), whole.rows.len(), "{what}: row count");

        // --- the whole page, nothing suppressed -----------------------------
        let (replayed_rows, replayed_stubs) = replay(&normalized, &case.refs, case.head, 0);
        let replayed_branch_colors = ReplayClassifier::new(&case.refs, case.head).branch_colors();

        // The classifier must not disturb the geometry it decorates.
        assert_eq!(
            replayed_rows
                .iter()
                .map(|r| (r.commit.id.clone(), r.row, r.lane))
                .collect::<Vec<_>>(),
            whole
                .rows
                .iter()
                .map(|r| (r.commit.id.clone(), r.row, r.lane))
                .collect::<Vec<_>>(),
            "{what}: rows, in order, in their lanes"
        );

        assert_eq!(
            replayed_rows.iter().map(|r| r.color).collect::<Vec<_>>(),
            whole.rows.iter().map(|r| r.color).collect::<Vec<_>>(),
            "{what}: per-row colours"
        );
        assert_eq!(
            badges_of(&replayed_rows),
            badges_by_oid(&whole),
            "{what}: badges by commit id"
        );
        assert_eq!(
            replayed_stubs,
            frame_stubs_by_anchor(&whole),
            "{what}: stubs by anchor"
        );
        assert_eq!(
            replayed_branch_colors,
            named_slots(&whole, &case.refs, case.head),
            "{what}: named slots"
        );

        // --- and at every page boundary ------------------------------------
        // Rows [0, split) are replayed with emission suppressed. The tail must
        // still carry the oracle's colours, badges and — the fragile part —
        // the oracle's stub `lane_offset`s, which are numbered off the
        // suppressed prefix.
        let oracle_badges = badges_by_oid(&whole);
        let oracle_stubs = frame_stubs_by_anchor(&whole);
        for split in 1..=normalized.len() {
            let (tail_rows, tail_stubs) = replay(&normalized, &case.refs, case.head, split);
            assert_eq!(
                tail_rows.iter().map(|r| r.color).collect::<Vec<_>>(),
                whole.rows[split..]
                    .iter()
                    .map(|r| r.color)
                    .collect::<Vec<_>>(),
                "{what}: per-row colours from row {split}"
            );
            assert_eq!(
                badges_of(&tail_rows),
                oracle_badges[split..].to_vec(),
                "{what}: badges from row {split}"
            );
            let expected: Vec<FrameStub> = oracle_stubs
                .iter()
                .filter(|s| row_of(&whole, &s.anchor_commit) >= split)
                .cloned()
                .collect();
            assert_eq!(
                tail_stubs, expected,
                "{what}: stubs from row {split} keep their cumulative offsets"
            );
        }
    }
}
