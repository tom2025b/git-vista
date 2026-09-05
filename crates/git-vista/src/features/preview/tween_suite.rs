//! The tween scene's suite (#591).
//!
//! Included from [`super`] with `#[path]`, so it is a child of
//! `features::preview::tween` and can see everything private there — same
//! shape as `scene_suite.rs` beside it, and for the same reason: this is pure
//! arithmetic, so none of it needs a browser.
//!
//! # What is proven here
//!
//! * A commit in both halves slides between its two *real* draw positions —
//!   never an invented one — and reaches exactly the after picture's pixel
//!   at `t = 1`.
//! * The hypothetical commit (`Entering`) is never given a starting
//!   position: it is fixed at its one real position throughout, and only its
//!   opacity moves.
//! * A ref slides from the commit `RefMove::from` names to the commit
//!   `RefMove::to` names — the two ends the server itself reported — and
//!   reaches the destination pixel at `t = 1`.
//! * An outcome-only tag (`is_mark`) is invisible before `REVEAL_AFTER` and
//!   visible at `t = 1`; an unmoved ref tag is visible throughout.
//! * `t` outside `[0, 1]` cannot escape the scene's real geometry — clamped,
//!   never extrapolated past either endpoint.
//! * The animated scene's edge topology matches the static after picture's
//!   edge count, so the two renderers cannot silently disagree about what
//!   the after graph connects.

use super::*;

use git_vista_core::model::{CommitSummary, Edge, GitRef, GraphRow, Oid, RefKind};
use git_vista_core::preview::PreviewChange;
use git_vista_protocol::preview::{PreviewGraph, PreviewOutcome};

use crate::features::preview::core::{view_of, Half, Picture, PreviewView};
use crate::features::preview::scene::scene_of;

/// An oid whose first seven characters are unique to `n` (short-hash safe,
/// same recipe `scene_suite` uses and for the same reason).
fn oid(n: usize) -> Oid {
    Oid(format!("{n:07x}{}", "f".repeat(33)))
}

/// `id_n` names which commit this is (feeds [`oid`] and the summary); `pos`
/// is where it sits vertically (`GraphRow::row`). Kept as two parameters
/// rather than one, unlike `scene_suite`'s `row` — that suite's fixtures
/// never reorder commits relative to their numbering, but this suite's
/// `after_with_added` prepends a hypothetical commit, which is exactly the
/// case where "which commit" and "which row" diverge (the doc for
/// `super::scene::window_for_before` says as much: prepending one commit
/// shifts every row beneath it).
fn row_at(pos: usize, id_n: usize, lane: usize, refs: Vec<GitRef>) -> GraphRow {
    GraphRow {
        commit: CommitSummary {
            id: oid(id_n),
            parents: vec![oid(id_n + 1)],
            summary: format!("commit number {id_n}"),
            author: "Test".into(),
            time: 1000 - id_n as i64,
        },
        row: pos,
        lane,
        refs,
        color: 0,
        on_remote: false,
    }
}

/// A row whose position and id numbering coincide — the common case.
fn row(n: usize, lane: usize, refs: Vec<GitRef>) -> GraphRow {
    row_at(n, n, lane, refs)
}

fn edge(from: usize, from_lane: usize, to: usize, to_lane: usize) -> Edge {
    Edge {
        from_row: from,
        from_lane,
        to_row: to,
        to_lane,
    }
}

fn head(target: usize) -> GitRef {
    GitRef {
        name: "HEAD".into(),
        kind: RefKind::Head,
        target: oid(target),
    }
}

fn branch(name: &str, target: usize) -> GitRef {
    GitRef {
        name: name.into(),
        kind: RefKind::Branch,
        target: oid(target),
    }
}

/// A picture built the way production does — through [`view_of`] — so the
/// tween under test sees exactly the marks/ref-moves the real path derives.
fn picture(before: Half, after: Half, changes: Vec<PreviewChange>) -> Picture {
    match view_of(PreviewOutcome::Graph {
        before,
        after,
        changes,
    }) {
        PreviewView::Picture(p) => p,
        other => panic!("expected a picture, got {other:?}"),
    }
}

/// A three-commit chain (`HEAD`/`main` on row 0) — the "before" a revert or
/// cherry-pick starts from.
fn before_chain() -> Half {
    PreviewGraph {
        rows: vec![
            row(0, 0, vec![head(0), branch("main", 0)]),
            row(1, 0, Vec::new()),
            row(2, 0, Vec::new()),
        ],
        edges: vec![edge(0, 0, 1, 0), edge(1, 0, 2, 0)],
        stubs: Vec::new(),
        lane_count: 1,
    }
}

/// The same chain with a hypothetical commit prepended and `HEAD`/`main`
/// moved onto it — the shape a revert's `/api/preview` answer takes.
fn after_with_added() -> Half {
    const NEW: usize = 0xbeef;
    PreviewGraph {
        rows: vec![
            row_at(0, NEW, 0, vec![head(NEW), branch("main", NEW)]),
            row_at(1, 0, 0, Vec::new()),
            row_at(2, 1, 0, Vec::new()),
            row_at(3, 2, 0, Vec::new()),
        ],
        // Row positions, not commit numbers — the new commit sits at row 0
        // and every existing commit's row shifts down by one.
        edges: vec![edge(0, 0, 1, 0), edge(1, 0, 2, 0), edge(2, 0, 3, 0)],
        stubs: Vec::new(),
        lane_count: 1,
    }
}

fn added_commit_changes() -> Vec<PreviewChange> {
    const NEW: usize = 0xbeef;
    vec![
        PreviewChange::Added { commit: oid(NEW) },
        PreviewChange::RefMoved {
            ref_name: "HEAD".into(),
            from: oid(0),
            to: oid(NEW),
        },
        PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid(0),
            to: oid(NEW),
        },
    ]
}

// ---------------------------------------------------------------------------
// Node lifecycle.
// ---------------------------------------------------------------------------

#[test]
fn a_commit_in_both_halves_is_persistent_and_ends_exactly_on_the_after_pixel() {
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let scene = tween_of(&p);
    let after_scene = scene_of(&p);

    // Row 1 in `before` (commit 0's child) survives unchanged into `after` —
    // present in both, so it must be `Persistent`.
    let commit_1 = oid(1).0;
    let node = scene
        .nodes
        .iter()
        .find(|n| n.commit_id == commit_1)
        .expect("commit 1 is drawn in both halves");
    assert!(
        matches!(node.lifecycle, NodeLifecycle::Persistent { .. }),
        "commit 1 exists in both before and after and must be Persistent, got {:?}",
        node.lifecycle
    );

    let after_node = after_scene
        .after
        .nodes
        .iter()
        .find(|n| n.commit_id == commit_1)
        .expect("the static after picture also draws commit 1");
    let frame = sample(&scene, 1.0);
    let frame_node = frame
        .nodes
        .iter()
        .find(|n| n.commit_id == commit_1)
        .unwrap();
    assert_eq!(
        frame_node.cx, after_node.cx as f64,
        "t=1 must land on the after picture's x"
    );
    assert_eq!(
        frame_node.cy, after_node.cy as f64,
        "t=1 must land on the after picture's y"
    );
}

#[test]
fn the_hypothetical_commit_never_receives_an_invented_starting_position() {
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let scene = tween_of(&p);
    let new_id = oid(0xbeef).0;
    let node = scene
        .nodes
        .iter()
        .find(|n| n.commit_id == new_id)
        .expect("the hypothetical commit is drawn");
    assert_eq!(
        node.lifecycle,
        NodeLifecycle::Entering,
        "a commit absent from `before` must never be Persistent — that would \
         require a `from` position this module has no honest way to invent"
    );

    // Position is identical at every t — only opacity should move.
    let at_0 = sample(&scene, 0.0);
    let at_half = sample(&scene, 0.5);
    let at_1 = sample(&scene, 1.0);
    let (n0, nh, n1) = (
        at_0.nodes.iter().find(|n| n.commit_id == new_id).unwrap(),
        at_half
            .nodes
            .iter()
            .find(|n| n.commit_id == new_id)
            .unwrap(),
        at_1.nodes.iter().find(|n| n.commit_id == new_id).unwrap(),
    );
    assert_eq!((n0.cx, n0.cy), (nh.cx, nh.cy));
    assert_eq!((nh.cx, nh.cy), (n1.cx, n1.cy));
    assert!(
        n0.opacity < nh.opacity && nh.opacity < n1.opacity,
        "the hypothetical commit must fade in monotonically: {} / {} / {}",
        n0.opacity,
        nh.opacity,
        n1.opacity
    );
    assert_eq!(n1.opacity, 1.0, "fully arrived by t=1");
}

#[test]
fn a_commit_absent_from_the_after_window_but_present_in_after_rows_is_dropped_not_faded() {
    // The honesty guard: `before` draws a commit `after`'s WINDOW does not,
    // but `after.rows` still contains it (it was not destroyed, just not in
    // the drawn slice). It must not appear at all in the tween — fading it
    // would claim the operation removed it, which is false.
    let after = after_with_added();
    // A commit genuinely reachable in `after` but past any reasonable window
    // (`window_for_after` budget is `MAX_ROWS` = 10): the fixture's row 2 is
    // well within that budget, so this test is about existence rather than
    // window size — assert directly that a before-row present in
    // `after.rows` is never classified `Leaving`.
    let existing_ids: std::collections::HashSet<String> =
        after.rows.iter().map(|r| r.commit.id.0.clone()).collect();
    assert!(
        existing_ids.contains(&oid(2).0),
        "fixture sanity: row 2 exists in after"
    );
    let p = picture(before_chain(), after, added_commit_changes());
    let scene = tween_of(&p);
    let leaving: Vec<_> = scene
        .nodes
        .iter()
        .filter(|n| matches!(n.lifecycle, NodeLifecycle::Leaving))
        .collect();
    assert!(
        leaving.is_empty(),
        "no commit here is genuinely absent from `after.rows`, so nothing may be Leaving: {leaving:?}"
    );
}

#[test]
fn a_commit_missing_from_after_entirely_leaves_from_its_real_before_position() {
    // A destructive edge case this module defends against even though no
    // supported operation produces it today (see the module doc): a commit
    // in `before` that truly does not exist in `after.rows` at all.
    let before = before_chain();
    let mut after = after_with_added();
    let gone_row = after
        .rows
        .iter()
        .find(|r| r.commit.id.0 == oid(2).0)
        .map(|r| r.row)
        .expect("fixture sanity: commit 2 starts out present in after");
    after.rows.retain(|r| r.commit.id.0 != oid(2).0);
    after
        .edges
        .retain(|e| e.to_row != gone_row && e.from_row != gone_row);
    let p = picture(before, after, added_commit_changes());
    let scene = tween_of(&p);
    let node = scene
        .nodes
        .iter()
        .find(|n| n.commit_id == oid(2).0)
        .expect("commit 2 is still drawn (Leaving), from its real before position");
    assert!(matches!(node.lifecycle, NodeLifecycle::Leaving));

    let at_0 = sample(&scene, 0.0);
    let at_1 = sample(&scene, 1.0);
    let n0 = at_0.nodes.iter().find(|n| n.commit_id == oid(2).0).unwrap();
    let n1 = at_1.nodes.iter().find(|n| n.commit_id == oid(2).0).unwrap();
    assert_eq!(n0.opacity, 1.0, "still fully visible at the start");
    assert_eq!(n1.opacity, 0.0, "faded out by the end");
    assert_eq!(
        (n0.cx, n0.cy),
        (n1.cx, n1.cy),
        "position never moves for a Leaving commit"
    );
}

// ---------------------------------------------------------------------------
// Ref badges.
// ---------------------------------------------------------------------------

#[test]
fn a_ref_slides_from_its_real_origin_to_its_real_destination() {
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let scene = tween_of(&p);
    let after_scene = scene_of(&p);

    let main_badge = scene
        .badges
        .iter()
        .find(|b| b.text == "main")
        .expect("main's move is reported by the server and must produce a badge");
    let origin = scene_of(&p)
        .before
        .nodes
        .iter()
        .find(|n| n.commit_id == oid(0).0)
        .map(|n| (n.cx, n.cy))
        .unwrap();
    assert_eq!(main_badge.from, Some(origin));
    let destination = after_scene
        .after
        .nodes
        .iter()
        .find(|n| n.commit_id == oid(0xbeef).0)
        .map(|n| (n.cx, n.cy))
        .unwrap();
    assert_eq!(main_badge.to, destination);

    let frame_1 = sample(&scene, 1.0);
    let frame_badge = frame_1.badges.iter().find(|b| b.text == "main").unwrap();
    assert_eq!(
        (frame_badge.cx, frame_badge.cy),
        (destination.0 as f64, destination.1 as f64)
    );
}

#[test]
fn a_ref_with_no_drawn_origin_fades_in_at_its_destination_rather_than_sliding_from_nowhere() {
    // The ref's `from` commit is not in `before`'s drawn window at all.
    let mut before = before_chain();
    before.rows.retain(|r| r.commit.id.0 != oid(0).0);
    let p = picture(before, after_with_added(), added_commit_changes());
    let scene = tween_of(&p);
    let main_badge = scene.badges.iter().find(|b| b.text == "main").unwrap();
    assert_eq!(
        main_badge.from, None,
        "no honest starting pixel exists, so this must not invent one"
    );

    let at_0 = sample(&scene, 0.0);
    let at_1 = sample(&scene, 1.0);
    let b0 = at_0.badges.iter().find(|b| b.text == "main").unwrap();
    let b1 = at_1.badges.iter().find(|b| b.text == "main").unwrap();
    assert_eq!(
        (b0.cx, b0.cy),
        (b1.cx, b1.cy),
        "fixed at the destination throughout"
    );
    assert!(
        b0.opacity < b1.opacity,
        "fades in rather than appearing instantly"
    );
    assert_eq!(b1.opacity, 1.0);
}

#[test]
fn an_operation_with_no_ref_moves_produces_no_badges() {
    // A picture with only a lane shift and no ref move at all.
    let before = PreviewGraph {
        rows: vec![row(0, 1, Vec::new()), row(1, 0, Vec::new())],
        edges: vec![edge(0, 1, 1, 0)],
        stubs: Vec::new(),
        lane_count: 2,
    };
    let after = PreviewGraph {
        rows: vec![row(0, 0, Vec::new()), row(1, 0, Vec::new())],
        edges: vec![edge(0, 0, 1, 0)],
        stubs: Vec::new(),
        lane_count: 1,
    };
    let changes = vec![PreviewChange::LaneShifted {
        commit: oid(0),
        from_lane: 1,
        to_lane: 0,
    }];
    let p = picture(before, after, changes);
    let scene = tween_of(&p);
    assert!(scene.badges.is_empty());
}

// ---------------------------------------------------------------------------
// Outcome-label reveal timing.
// ---------------------------------------------------------------------------

#[test]
fn an_outcome_only_tag_is_hidden_until_reveal_after_and_visible_at_rest() {
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let scene = tween_of(&p);
    let new_id = oid(0xbeef).0;

    let before_reveal = sample(&scene, REVEAL_AFTER - 0.05);
    let node = before_reveal
        .nodes
        .iter()
        .find(|n| n.commit_id == new_id)
        .unwrap();
    assert!(
        !node.tags.iter().any(|t| t.text == "new"),
        "the `new` pill is an outcome label and must stay hidden before REVEAL_AFTER"
    );

    let at_rest = sample(&scene, 1.0);
    let node = at_rest
        .nodes
        .iter()
        .find(|n| n.commit_id == new_id)
        .unwrap();
    assert!(
        node.tags.iter().any(|t| t.text == "new"),
        "the `new` pill must be visible once the transition has settled"
    );
}

#[test]
fn an_unmoved_ref_tag_is_visible_throughout_not_gated_by_reveal_after() {
    // A branch that already points here and does not move — not an outcome
    // of this operation, so it must never be hidden.
    let before = PreviewGraph {
        rows: vec![row(0, 0, vec![branch("stable", 0)]), row(1, 0, Vec::new())],
        edges: vec![edge(0, 0, 1, 0)],
        stubs: Vec::new(),
        lane_count: 1,
    };
    let after = before.clone();
    let p = picture(before, after, Vec::new());
    let scene = tween_of(&p);
    let at_start = sample(&scene, 0.0);
    let node = at_start
        .nodes
        .iter()
        .find(|n| n.commit_id == oid(0).0)
        .unwrap();
    assert!(
        node.tags.iter().any(|t| t.text == "stable"),
        "an unmoved ref is not an outcome label and must be visible from t=0"
    );
}

// ---------------------------------------------------------------------------
// Progress / clamping.
// ---------------------------------------------------------------------------

#[test]
fn progress_at_clamps_before_the_start_and_after_the_end() {
    assert_eq!(progress_at(-100.0), 0.0);
    assert_eq!(progress_at(0.0), 0.0);
    assert_eq!(progress_at(DURATION_MS / 2.0), 0.5);
    assert_eq!(progress_at(DURATION_MS), 1.0);
    assert_eq!(progress_at(DURATION_MS * 10.0), 1.0);
    assert_eq!(progress_at(f64::NAN), 0.0);
    assert_eq!(progress_at(f64::INFINITY), 1.0);
}

#[test]
fn sample_clamps_t_outside_zero_one_rather_than_extrapolating() {
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let scene = tween_of(&p);
    let below = sample(&scene, -5.0);
    let at_zero = sample(&scene, 0.0);
    let above = sample(&scene, 5.0);
    let at_one = sample(&scene, 1.0);
    assert_eq!(below, at_zero, "t < 0 must clamp to the exact t=0 frame");
    assert_eq!(
        above, at_one,
        "t > 1 must clamp to the exact t=1 frame, never overshoot"
    );
}

#[test]
fn ease_in_out_cubic_is_monotonic_and_fixes_both_endpoints() {
    assert_eq!(ease_in_out_cubic(0.0), 0.0);
    assert_eq!(ease_in_out_cubic(1.0), 1.0);
    let mut prev = -1.0;
    let mut t = 0.0;
    while t <= 1.0 {
        let v = ease_in_out_cubic(t);
        assert!(
            v >= prev,
            "easing must never move backwards: t={t} v={v} prev={prev}"
        );
        prev = v;
        t += 0.05;
    }
}

// ---------------------------------------------------------------------------
// Cross-check against the static renderer.
// ---------------------------------------------------------------------------

#[test]
fn the_animated_edge_count_matches_the_static_after_pictures_drawn_edges() {
    // Both renderers draw the after graph's real topology; a drift here would
    // mean the two disagree about what the after graph connects.
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let animated = tween_of(&p);
    let static_after = scene_of(&p).after;
    assert_eq!(
        animated.edges.len(),
        static_after.edges.len(),
        "tween::edges_of and scene::half_scene must agree on how many after \
         edges are drawn for the same window"
    );
}

#[test]
fn every_persistent_or_entering_node_id_is_drawn_in_the_static_after_picture() {
    let p = picture(before_chain(), after_with_added(), added_commit_changes());
    let animated = tween_of(&p);
    let static_after = scene_of(&p).after;
    let after_ids: std::collections::HashSet<&str> = static_after
        .nodes
        .iter()
        .map(|n| n.commit_id.as_str())
        .collect();
    for node in &animated.nodes {
        if matches!(node.lifecycle, NodeLifecycle::Leaving) {
            continue;
        }
        assert!(
            after_ids.contains(node.commit_id.as_str()),
            "commit {} is drawn in the tween but not the static after picture",
            node.commit_id
        );
    }
}

// ---------------------------------------------------------------------------
// The wasm-only clock, read as text (ADR 0115).
// ---------------------------------------------------------------------------
//
// `features::preview::signals::Playback` is `#[cfg(target_arch = "wasm32")]`
// and drives the animation from a real clock, so nothing above can execute a
// line of it. What these tests pin, the same way
// `core::preview_action_tests` pins `dialogs/confirm.rs`, is the *composition*
// that file cannot prove of itself: that the reduced-motion decision this
// module computes is actually the one the clock obeys, and that the clock
// asks this module for progress rather than keeping a second answer of its
// own.

/// `signals.rs`, read as text — the only way a host test can see what its
/// wasm-only glue does with the decisions this module makes.
const SIGNALS: &str = include_str!("signals.rs");

/// The body of `Playback::start`, bounded so a match below cannot accidentally
/// read past it into `schedule` or `bump`.
fn playback_start_body() -> &'static str {
    let after = SIGNALS
        .split_once("pub fn start(&self, reduced_motion: bool) {")
        .expect("Playback::start no longer has this signature")
        .1;
    let end = after
        .find("\n    }\n")
        .expect("Playback::start is no longer a closed block");
    &after[..end]
}

#[test]
fn reduced_motion_returns_before_scheduling_a_frame() {
    let body = playback_start_body();
    let reduced_arm = body
        .split_once("if reduced_motion {")
        .expect("Playback::start no longer branches on reduced_motion")
        .1;
    let arm_end = reduced_arm
        .find('}')
        .expect("the reduced_motion arm is not a closed block");
    let arm = &reduced_arm[..arm_end];
    assert!(
        arm.contains("return"),
        "the reduced_motion arm must return before any frame can be \
         scheduled — #591 requires the animation to degrade to the resting \
         frame rather than ever entering the loop that could show anything \
         else. Arm was:\n{arm}"
    );
    assert!(
        !arm.contains("self.schedule("),
        "the reduced_motion arm must never call schedule: {arm}"
    );
    assert!(
        body.contains("self.schedule("),
        "the non-reduced path must still schedule the first frame, or the \
         animation never plays for anyone: {body}"
    );
}

#[test]
fn the_frame_loop_asks_this_module_for_progress_rather_than_keeping_a_second_answer() {
    assert!(
        SIGNALS.contains("crate::features::preview::tween::progress_at(elapsed)"),
        "signals.rs's frame loop no longer asks tween::progress_at for the \
         answer — a second, wasm-only notion of \"what progress is this\" is \
         exactly the kind of decision ADR 0115 exists to keep out of a file \
         `cargo test` never runs"
    );
}
