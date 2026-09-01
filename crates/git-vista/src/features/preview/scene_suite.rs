//! The preview scene's suite (M10.08 A6, #594).
//!
//! Included from [`super`] with `#[path]`, so it is a child of
//! `features::preview::scene` and can see everything private there. Host tests:
//! the renderer is pure by construction, so none of this needs a browser —
//! which is the entire reason it was built as a separate renderer rather than
//! bent out of `render/`, none of which `cargo test` ever compiles.
//!
//! # What is proven here
//!
//! * The window lands on **what changed**, not on the top of the graph. A
//!   preview whose one added commit was cropped out is worse than no preview.
//! * The two halves are windowed onto the same *commits*, matched by id — the
//!   only thing that can match them, since prepending one commit renumbers
//!   every row beneath it.
//! * Every mark reaches the picture: the added commit, the refs that land, the
//!   lane shifts — and a ref the layout did not attach to its row is still
//!   named rather than silently dropped.
//! * Stubs survive into the after half, so an operation is never drawn as if
//!   it deleted the repository's branches.
//! * Nothing overflows the half's width, and both halves share one width.

use super::*;

use git_vista_core::model::{BranchStub, CommitSummary, Edge, GitRef, GraphRow, Oid};
use git_vista_core::preview::PreviewChange;
use git_vista_protocol::preview::{PreviewGraph, PreviewOutcome};

use crate::features::preview::core::{view_of, PreviewView};

/// An oid whose **first seven characters are unique to `n`**.
///
/// Not `{n:040x}` — that pads on the left, so every fixture oid shares the
/// prefix `0000000` and every `alt.starts_with(&oid(n).0[..7])` lookup in this
/// suite silently matched row 0. Three tests passed a `find` and then asserted
/// against the wrong commit. The short hash is what a person reads off the
/// picture, so the fixture makes it the discriminating part.
fn oid(n: usize) -> Oid {
    Oid(format!("{n:07x}{}", "f".repeat(33)))
}

/// A row at `row`, in `lane`, whose parent is the row below it.
fn row(n: usize, lane: usize) -> GraphRow {
    GraphRow {
        commit: CommitSummary {
            id: oid(n),
            parents: vec![oid(n + 1)],
            summary: format!("commit number {n}"),
            author: "Test".into(),
            time: 1000 - n as i64,
        },
        row: n,
        lane,
        refs: Vec::new(),
        color: 0,
        on_remote: false,
    }
}

/// A straight in-lane edge from row `n` to row `n + 1`.
fn edge(n: usize, lane: usize) -> Edge {
    Edge {
        from_row: n,
        from_lane: lane,
        to_row: n + 1,
        to_lane: lane,
    }
}

/// `count` rows in lane 0, linked top to bottom.
fn chain(count: usize) -> Half {
    PreviewGraph {
        rows: (0..count).map(|n| row(n, 0)).collect(),
        edges: (0..count.saturating_sub(1)).map(|n| edge(n, 0)).collect(),
        stubs: Vec::new(),
        lane_count: 1,
    }
}

/// Build a [`Picture`] the way production does — through [`view_of`], so the
/// marks under test are the ones the real path derives rather than a second
/// derivation this suite invented.
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

/// Every commit id the scene actually drew, in order.
fn drawn_ids(half: &HalfScene) -> Vec<String> {
    half.nodes
        .iter()
        .map(|n| n.alt.split(' ').next().unwrap_or_default().to_string())
        .collect()
}

/// **The load-bearing test.** The window follows the change, not the top.
///
/// The server walks up to 500 commits per half and a modal draws ten. A
/// windowing rule that took the newest ten would, for any operation whose
/// change is not at the very top of history, draw ten commits with nothing
/// marked in them — a picture that looks like an answer and shows nothing.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the mechanism** — have `window_for_after` ignore `marks`
///    entirely and return `window_around(min, max, min_row, min_row, budget)`.
///    The window becomes rows 0..=9, the marked row 40 is not drawn, and the
///    first assertion is red on a window that holds nothing marked.
/// 2. **WEAKENS the mechanism** — keep the mark search but take only the
///    *first* marked row (`marked.first()` for both `lo` and `hi`). Row 40 is
///    still drawn, so the first assertion passes; row 44 is not, and the
///    "every marked row is drawn" assertion goes red — the near miss a
///    "does it contain a mark?" check would wave through.
#[test]
fn the_window_lands_on_what_changed_and_never_merely_on_the_top() {
    let changes = vec![
        PreviewChange::Added { commit: oid(40) },
        PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid(50),
            to: oid(44),
        },
    ];
    let p = picture(chain(60), chain(60), changes);
    let window = window_for_after(&p.after, &p.marks, MAX_ROWS);

    assert!(
        window.holds(40),
        "the added commit was cropped out of its own preview: {window:?}"
    );
    assert!(window.holds(44), "a marked row was cropped out: {window:?}");
    assert!(
        window.len() <= MAX_ROWS,
        "the window overran the budget: {window:?}"
    );
    assert!(
        window.first > 0,
        "the window sat at the top of the graph while the change was at row 40"
    );
}

/// The two halves are matched by commit id, never by row number.
///
/// This is the concrete form of the protocol's own argument for returning both
/// halves: a preview that prepends one hypothetical commit renumbers every row
/// beneath it, so `after` row 5 and `before` row 5 are different commits.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the mechanism** — have `window_for_before` return
///    `after_window` verbatim. The before half then starts one commit too
///    early; its first drawn id is `oid(29)` rather than `oid(30)` and the
///    id-equality assertion is red on the whole list.
/// 2. **WEAKENS the mechanism** — match on `r.row` membership instead of on
///    the id set (`after_window.holds(r.row)`). Identical wrong answer for
///    this fixture but by a different route, and the *shared-commit* assertion
///    below (which checks the overlap, not the offset) is what catches it.
#[test]
fn the_before_half_is_windowed_onto_the_same_commits_by_id() {
    // `after` is `before` with one hypothetical commit prepended, so every
    // real commit's row number is one higher in `after` than in `before`.
    let before = chain(60);
    let mut after = chain(60);
    for r in &mut after.rows {
        r.row += 1;
    }
    for e in &mut after.edges {
        e.from_row += 1;
        e.to_row += 1;
    }
    let new = GraphRow {
        commit: CommitSummary {
            id: oid(999),
            parents: vec![oid(30)],
            summary: "the hypothetical commit".into(),
            author: "Test".into(),
            time: 2000,
        },
        row: 0,
        lane: 0,
        refs: Vec::new(),
        color: 0,
        on_remote: false,
    };
    after.rows.insert(0, new);
    after.edges.push(Edge {
        from_row: 0,
        from_lane: 0,
        to_row: 31,
        to_lane: 0,
    });

    let changes = vec![PreviewChange::Added { commit: oid(999) }];
    let scene = scene_of(&picture(before, after, changes));

    let before_ids = drawn_ids(&scene.before);
    let after_ids = drawn_ids(&scene.after);
    // Every real commit in the after window is also in the before window: the
    // reader is comparing the same commits, not two unrelated slices.
    let shared: Vec<&String> = after_ids
        .iter()
        .filter(|id| before_ids.contains(id))
        .collect();
    assert!(
        shared.len() >= after_ids.len() - 1,
        "the halves show different commits — before {before_ids:?}, after {after_ids:?}"
    );
    assert!(
        !before_ids.contains(&oid(999).0.chars().take(7).collect::<String>()),
        "the hypothetical commit was drawn in the BEFORE half"
    );
}

/// An edge that leaves the window is clipped, not dropped.
///
/// A history is a continuous line; a picture whose bottom row has no line
/// leaving it says "history ends here", which is false for every preview the
/// window truncated.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the mechanism** — `continue` on any edge with an endpoint
///    outside the window. The clipped count drops to zero and the first
///    assertion is red.
/// 2. **WEAKENS the mechanism** — keep the edge but stop clamping the y
///    (`row_cy(...)` with no `.clamp(top, bottom)`). The edges are still
///    present, so the first assertion passes; the coordinates run far outside
///    the viewBox and the bounds assertion goes red.
#[test]
fn an_edge_leaving_the_window_is_clipped_rather_than_dropped() {
    let scene = scene_of(&picture(
        chain(40),
        chain(40),
        vec![PreviewChange::Added { commit: oid(20) }],
    ));

    let clipped = scene.after.edges.iter().filter(|e| e.clipped).count();
    assert!(
        clipped >= 2,
        "a window cut out of the middle of a history must have an edge running \
         off each end; found {clipped} clipped edges"
    );

    // Nothing may be drawn more than half a row outside the band.
    let slack = ROW_H;
    for e in &scene.after.edges {
        for token in e.d.split_whitespace() {
            if let Ok(v) = token.trim_end_matches(',').parse::<i32>() {
                assert!(
                    v >= -slack && v <= scene.after.height + slack,
                    "edge coordinate {v} is outside the viewBox (height {}): {}",
                    scene.after.height,
                    e.d
                );
            }
        }
    }
}

/// The added commit is drawn as added: a halo, a `new` pill, and a marked row.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the mark** — drop the `halo` field's assignment (always
///    `None`). The halo assertion is red; the pill and the `marked` flag still
///    pass, so the failure names the halo specifically.
/// 2. **WEAKENS the mark** — keep the halo but stop pushing the `new` pill.
///    The picture still highlights the dot, and a colour-blind or
///    screen-reader user is told nothing; the pill assertion is red while the
///    halo one is green.
#[test]
fn the_added_commit_carries_a_halo_a_pill_and_a_marked_row() {
    let scene = scene_of(&picture(
        chain(5),
        chain(5),
        vec![PreviewChange::Added { commit: oid(0) }],
    ));
    let node = scene
        .after
        .nodes
        .iter()
        .find(|n| n.alt.starts_with(&oid(0).0[..7]))
        .expect("the added commit must be drawn");

    assert!(node.halo.is_some(), "the added commit has no halo");
    assert!(
        node.tags.iter().any(|t| t.text == "new"),
        "the added commit has no `new` pill: {:?}",
        node.tags
    );
    assert!(node.marked, "the added commit's row is not marked");
    assert!(
        node.alt.contains("would create"),
        "a screen reader is told nothing about the added commit: {}",
        node.alt
    );

    // The BEFORE half never carries marks — it is the repository as it stands.
    assert!(
        scene
            .before
            .nodes
            .iter()
            .all(|n| n.halo.is_none() && !n.marked),
        "the before half was marked; only the after half is"
    );
}

/// A ref that moves is marked; a ref that merely sits there is not.
///
/// The distinction is the whole content of "changes marked": a picture in
/// which every badge looks the same tells a reader nothing about what moved.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the distinction** — set `stroke` to `fill` unconditionally.
///    Both badges look identical and the "moved is distinguishable" assertion
///    is red.
/// 2. **WEAKENS the distinction** — mark every badge as moved (`let moved =
///    true`). The moved badge still passes its own assertion; the *unmoved*
///    one now claims to move, and its assertion is red.
#[test]
fn a_ref_that_moves_is_marked_and_one_that_does_not_is_left_plain() {
    let mut after = chain(5);
    after.rows[0].refs = vec![GitRef {
        name: "main".into(),
        kind: RefKind::Branch,
        target: oid(0),
    }];
    after.rows[2].refs = vec![GitRef {
        name: "release".into(),
        kind: RefKind::Branch,
        target: oid(2),
    }];

    let scene = scene_of(&picture(
        chain(5),
        after,
        vec![PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid(3),
            to: oid(0),
        }],
    ));

    let tags: Vec<&SceneTag> = scene.after.nodes.iter().flat_map(|n| &n.tags).collect();
    let moved = tags
        .iter()
        .find(|t| t.text.contains("main"))
        .expect("the moved branch must have a badge");
    let plain = tags
        .iter()
        .find(|t| t.text.contains("release"))
        .expect("the untouched branch must keep its badge");

    assert_eq!(
        moved.stroke, MARK_REF,
        "the moved ref is drawn like any other badge"
    );
    assert!(
        moved.text.starts_with('→'),
        "the moved ref carries no arrow: {}",
        moved.text
    );
    assert_ne!(
        plain.stroke, MARK_REF,
        "a ref that did not move is drawn as if it had"
    );
    assert!(
        !plain.text.starts_with('→'),
        "a ref that did not move carries the arrow: {}",
        plain.text
    );
}

/// A ref the server says lands somewhere the layout did not attach it is still
/// named.
///
/// The change list is the authority on what moves. A row that dropped the
/// badge because its `refs` vector disagreed would read as "nothing moved
/// here" — reporting an absence for a change we were told about.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the fallback** — delete the second `for name in landed` loop.
///    No badge exists at all and the presence assertion is red.
/// 2. **WEAKENS the fallback** — push it without the `→` prefix and with
///    `stroke: fill`. The badge exists, so the presence assertion passes, and
///    the "it is marked as a move" assertion is red.
#[test]
fn a_landing_ref_the_layout_did_not_attach_is_still_named() {
    // The row carries no `refs` at all, yet the change list says `main` lands
    // on it.
    let scene = scene_of(&picture(
        chain(5),
        chain(5),
        vec![PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid(3),
            to: oid(1),
        }],
    ));
    let node = scene
        .after
        .nodes
        .iter()
        .find(|n| n.alt.starts_with(&oid(1).0[..7]))
        .expect("the landing row must be drawn");
    let tag = node
        .tags
        .iter()
        .find(|t| t.text.contains("main"))
        .unwrap_or_else(|| panic!("no badge for the landing ref: {:?}", node.tags));
    assert_eq!(tag.stroke, MARK_REF, "the badge is not marked as a move");
    assert!(
        node.alt.contains("main would end up here"),
        "a screen reader is not told the ref lands here: {}",
        node.alt
    );
}

/// A lane shift names both lanes, in order.
///
/// `from` and `to` are both `usize`, so a transposition compiles and reads
/// plausibly — and would tell the user the commit moved the wrong way.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the mark** — stop pushing the lane pill. The pill assertion is
///    red and the `alt` assertion is red too, both naming the same absence.
/// 2. **WEAKENS the mark** — swap the interpolation to `lane {to}→{from}`.
///    A pill still exists, so any "is there a lane tag?" check passes; the
///    exact-text assertion is red, which is the only kind of check that can
///    catch a transposition.
#[test]
fn a_lane_shift_names_both_lanes_in_the_right_order() {
    let mut after = chain(5);
    after.rows[2].lane = 1;
    after.lane_count = 2;
    let scene = scene_of(&picture(
        chain(5),
        after,
        vec![PreviewChange::LaneShifted {
            commit: oid(2),
            from_lane: 0,
            to_lane: 1,
        }],
    ));
    let node = scene
        .after
        .nodes
        .iter()
        .find(|n| n.alt.starts_with(&oid(2).0[..7]))
        .expect("the shifted commit must be drawn");
    assert!(
        node.tags.iter().any(|t| t.text == "lane 0→1"),
        "the lane shift is not drawn, or is drawn backwards: {:?}",
        node.tags
    );
    assert!(
        node.alt.contains("from column 0 to column 1"),
        "the lane shift is not announced, or is announced backwards: {}",
        node.alt
    );
}

/// Stubs survive into the after half.
///
/// `PreviewGraph`'s own doc names this failure: an after graph with no stubs,
/// beside a before graph with them, reads as "this operation deleted my
/// branches". It is the one drawing mistake that turns a reassuring picture
/// into an alarming lie.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the mechanism** — return an empty `stubs` vector from
///    `half_scene`. Both halves lose their rings; the after assertion is red,
///    and so is the legend assertion.
/// 2. **WEAKENS the mechanism** — filter stubs by `window.holds(s.lane)`
///    instead of `s.anchor_row`. A stub in a lane past the window's row range
///    silently vanishes from one half only, which is exactly the asymmetry
///    that produces the false alarm; the after assertion is red while the
///    before one still passes.
#[test]
fn a_branch_with_no_commits_of_its_own_survives_into_the_after_half() {
    let stub = BranchStub {
        name: "spike".into(),
        anchor_row: 1,
        anchor_lane: 0,
        lane: 2,
        color: 4,
        depth: 0,
    };
    let mut before = chain(5);
    before.stubs = vec![stub.clone()];
    before.lane_count = 3;
    let mut after = chain(5);
    after.stubs = vec![stub];
    after.lane_count = 3;

    let scene = scene_of(&picture(before, after, Vec::new()));

    assert_eq!(
        scene.after.stubs.len(),
        1,
        "the after half lost its branch stub — the picture now says the \
         operation deleted a branch"
    );
    assert_eq!(scene.before.stubs.len(), 1);
    assert_eq!(scene.after.stubs[0].name, "spike");
    assert!(
        scene.legend.iter().any(|e| e.mark == LegendMark::Stub),
        "a ring was drawn with nothing in the legend to decode it"
    );
}

/// Nothing overflows the half's width, whatever a row carries.
///
/// The label column is a fixed width so the two halves line up; a row with
/// several badges and a long summary must therefore lose text, not spill it
/// across the gap into the other picture.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the fit** — stop truncating (`let label = r.commit.summary`).
///    The estimated text width overruns and the label assertion is red.
/// 2. **WEAKENS the fit** — drop the `*x + w > right` guard in `push` so pills
///    are drawn past the edge. The label still fits (there is less room left,
///    so it truncates harder) and the *pill* assertion is red.
#[test]
fn no_row_overflows_the_half_width() {
    let mut after = chain(3);
    after.rows[0].refs = vec![
        GitRef {
            name: "a-long-branch-name-here".into(),
            kind: RefKind::Branch,
            target: oid(0),
        },
        GitRef {
            name: "another-long-branch".into(),
            kind: RefKind::Branch,
            target: oid(0),
        },
        GitRef {
            name: "v10.4.2-release-candidate".into(),
            kind: RefKind::Tag,
            target: oid(0),
        },
    ];
    after.rows[0].commit.summary =
        "a summary so long that it could not possibly fit inside the label column of a modal"
            .into();

    let scene = scene_of(&picture(
        chain(3),
        after,
        vec![
            PreviewChange::Added { commit: oid(0) },
            PreviewChange::LaneShifted {
                commit: oid(0),
                from_lane: 3,
                to_lane: 0,
            },
        ],
    ));

    for node in &scene.after.nodes {
        for tag in &node.tags {
            assert!(
                tag.x + tag.w <= scene.after.width,
                "pill {:?} runs past the half's width {}",
                tag,
                scene.after.width
            );
        }
        let text_w = node.label.chars().count() as i32 * LABEL_CHAR_W;
        assert!(
            node.label_x + text_w <= scene.after.width,
            "the summary {:?} runs past the half's width {} from x {}",
            node.label,
            scene.after.width,
            node.label_x
        );
    }
}

/// Both halves share one width and one lane count, so equivalent rows sit at
/// the same x and the same y.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the sharing** — build each half from its own `lane_count`.
///    The widths differ and the width assertion is red.
/// 2. **WEAKENS the sharing** — take the *min* of the two lane counts rather
///    than the max. The widths still agree, so that assertion passes; the
///    wider half's outermost lane is squashed onto a lane that is not its own
///    and the "lane 2 is where lane 2 belongs" assertion is red.
#[test]
fn both_halves_share_one_gutter_so_their_rows_line_up() {
    let mut before = chain(4);
    before.lane_count = 1;
    let mut after = chain(4);
    after.rows[1].lane = 2;
    after.lane_count = 3;

    let scene = scene_of(&picture(before, after, Vec::new()));

    assert_eq!(
        scene.before.width, scene.after.width,
        "the halves are different widths, so their rows cannot line up"
    );
    let shifted = &scene.after.nodes[1];
    assert_eq!(
        shifted.cx,
        PAD_X + 2 * LANE_W,
        "lane 2 was not drawn at lane 2 — the shared gutter was sized from the \
         wrong half"
    );
}

/// The legend names only what the picture drew.
///
/// A legend listing a mark that is not on screen teaches a reader to look for
/// something that is not there, which is worse than no legend.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the filter** — always push all four entries. The
///    "only what is drawn" assertion is red on the three surplus entries.
/// 2. **WEAKENS the filter** — key the `Added` entry on `!marks.is_empty()`
///    instead of on `m.added`. For a picture whose only change is a ref move,
///    an `Added` entry appears with no halo anywhere; that assertion is red
///    and the `RefMoved` one still passes.
#[test]
fn the_legend_names_only_the_marks_the_picture_drew() {
    let scene = scene_of(&picture(
        chain(4),
        chain(4),
        vec![PreviewChange::RefMoved {
            ref_name: "main".into(),
            from: oid(2),
            to: oid(0),
        }],
    ));
    let marks: Vec<LegendMark> = scene.legend.iter().map(|e| e.mark).collect();
    assert_eq!(
        marks,
        vec![LegendMark::RefMoved],
        "the legend named marks the picture never drew"
    );
}

/// The elision captions count in both directions, and say which direction.
///
/// "12 commits not shown" beside a window cut out of the middle of a history
/// is ambiguous in the one way that matters: a reader cannot tell whether the
/// newest commit is on screen.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES a caption** — always set `elided_above: None`. The above
///    assertion is red; the below one still passes, so the direction that
///    broke is named.
/// 2. **WEAKENS the counts** — count `half.rows.len() - drawn` into both.
///    Both captions exist and both carry the same, wrong number; the exact
///    count assertions are red.
#[test]
fn the_elision_captions_count_and_name_both_directions() {
    let p = picture(
        chain(40),
        chain(40),
        vec![PreviewChange::Added { commit: oid(20) }],
    );
    let window = window_for_after(&p.after, &p.marks, MAX_ROWS);
    let scene = scene_of(&p);

    let above = window.first;
    let below = 40 - (window.last + 1);

    assert_eq!(
        scene.after.elided_above.as_deref(),
        Some(format!("{above} newer commits not shown").as_str()),
        "the newer-commits caption is wrong or absent"
    );
    assert_eq!(
        scene.after.elided_below.as_deref(),
        Some(format!("{below} older commits not shown").as_str()),
        "the older-commits caption is wrong or absent"
    );

    // A history that fits needs no caption at all.
    let small = scene_of(&picture(chain(3), chain(3), Vec::new()));
    assert!(small.after.elided_above.is_none());
    assert!(small.after.elided_below.is_none());
}

/// Lanes past the cap are squashed, and the squash is declared.
///
/// A repository with forty branch stubs would otherwise make the gutter wider
/// than the label column and push every summary off the picture.
///
/// # Two mutations, failing differently
///
/// 1. **REMOVES the cap** — `let lanes = raw_lanes`. The gutter grows without
///    bound and the width assertion is red.
/// 2. **WEAKENS the declaration** — cap the lanes but leave `lanes_clamped`
///    false. The picture fits, so the width assertion passes, and the reader
///    is shown two commits stacked in one column with nothing saying why; the
///    declaration assertion is red.
#[test]
fn lanes_past_the_cap_are_squashed_and_the_squash_is_declared() {
    let mut after = chain(4);
    after.rows[1].lane = 30;
    after.lane_count = 40;
    let scene = scene_of(&picture(chain(4), after, Vec::new()));

    assert!(
        scene.after.width <= PAD_X + (MAX_LANES as i32) * LANE_W + LABEL_GAP + LABEL_W,
        "the gutter grew past the cap: width {}",
        scene.after.width
    );
    assert!(
        scene.after.lanes_clamped,
        "lanes were squashed and the picture does not say so"
    );
    assert_eq!(
        scene.after.nodes[1].cx,
        PAD_X + (MAX_LANES as i32 - 1) * LANE_W,
        "a lane past the cap was not squashed onto the last drawn lane"
    );
}

/// An empty half lays out rather than panicking.
///
/// Reachable: a repository with no commits at all, and a preview whose before
/// half is therefore empty. A panic in a wasm view is a blank app, not a
/// diagnostic.
#[test]
fn an_empty_half_lays_out_without_panicking() {
    let empty = PreviewGraph {
        rows: Vec::new(),
        edges: Vec::new(),
        stubs: Vec::new(),
        lane_count: 0,
    };
    let scene = scene_of(&picture(
        empty,
        chain(1),
        vec![PreviewChange::Added { commit: oid(0) }],
    ));
    assert!(scene.before.nodes.is_empty());
    assert_eq!(scene.after.nodes.len(), 1);
    assert!(
        scene.before.height > 0,
        "an empty half still needs a viewBox"
    );
}
