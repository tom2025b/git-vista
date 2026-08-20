//! `LoadedHistory`'s validate-then-commit append tests, the camera's
//! prefetch/single-flight decision (`should_prefetch`, `PageLoadState`), and
//! the cursor-retry/staleness tests that share their fixtures. Extracted
//! verbatim from the front half of `core.rs`'s inline `mod tests` (a
//! `#[cfg(test)]` child module) so the parent file can be read as production
//! code — the doc comment at the top of `core.rs` itself notes these 17
//! `LoadedHistory` tests moved here once already, from the crate-root
//! `history.rs` (M1.10, #63); this is the same move one level further. A
//! child module of `core`, so it still reaches `core.rs`'s private items
//! (`LoadedHistory::apply_page`, `label_occupancy`/`text_x`) through
//! `super::`. The UI-copy tests that shared this `mod tests` block but share
//! none of its fixtures live separately in `ui_copy_suite.rs`.

use super::*;
use git_vista_core::model::CommitSummary;

/// Every fixture page belongs to the same repository generation unless a test
/// is specifically about a generation change.
const GEN: &str = "g1";

/// A viewport exactly 1.5 * 560 / 56 = 15 rows of lookahead at scale 1, so the
/// prefetch boundary below is an exact number rather than an approximation.
const VIEWPORT_H: f64 = 560.0;

fn generation(value: &str) -> GenerationToken {
    GenerationToken::new(value).expect("test generation token")
}

fn commit(id: &str) -> CommitSummary {
    CommitSummary {
        id: Oid(id.into()),
        parents: vec![],
        summary: format!("commit {id}"),
        author: "tester".into(),
        time: 0,
    }
}

fn row(index: usize, lane: usize, id: &str) -> GraphRow {
    GraphRow {
        commit: commit(id),
        row: index,
        lane,
        refs: vec![],
        color: 0,
        on_remote: false,
    }
}

fn edge(from_row: usize, from_lane: usize, to_row: usize, to_lane: usize) -> Edge {
    Edge {
        from_row,
        from_lane,
        to_row,
        to_lane,
    }
}

fn stub(name: &str, anchor: &str, lane_offset: usize) -> FrameStub {
    FrameStub {
        name: name.into(),
        anchor_commit: Oid(anchor.into()),
        lane_offset,
        color: 3,
        depth: 0,
    }
}

fn page(
    rows: Vec<GraphRow>,
    edges: Vec<Edge>,
    stubs: Vec<FrameStub>,
    lane_count: usize,
    cursor: Option<&str>,
    generation_value: &str,
) -> Page {
    Page {
        rows,
        edges,
        stubs,
        lane_count,
        cursor: cursor.map(str::to_owned),
        generation: generation(generation_value),
    }
}

/// Page 1: rows 0..2 on one lane, the straight edge between them, more to come.
fn page_one() -> Page {
    page(
        vec![row(0, 0, "aaa0"), row(1, 0, "bbb1")],
        vec![edge(0, 0, 1, 0)],
        vec![],
        1,
        Some("c1"),
        GEN,
    )
}

fn seeded() -> LoadedHistory {
    LoadedHistory::from_first_page(page_one()).expect("page 1 is valid")
}

/// Page 2 in its plain form: row 2 only, hanging off row 1 in the same lane.
fn page_two() -> Page {
    page(
        vec![row(2, 0, "ccc2")],
        vec![edge(1, 0, 2, 0)],
        vec![],
        1,
        Some("c2"),
        GEN,
    )
}

#[test]
fn two_pages_append_without_mutating_prefix() {
    let mut history = seeded();
    let before = history.clone();

    let delta = history
        .append_page("c1", page_two())
        .expect("a contiguous page-2 append is valid");

    assert_eq!(delta.old_row_count, 2);
    assert!(
        !delta.prefix_geometry_changed,
        "a straight same-lane append must not move an existing label"
    );
    assert!(!delta.stub_geometry_changed, "no stubs on either page");

    // The prefix is *the same rows*, not merely equivalent ones.
    assert_eq!(&history.rows[..2], &before.rows[..]);
    assert_eq!(&history.edges[..1], &before.edges[..]);
    assert_eq!(&history.label_occupancy()[..2], before.label_occupancy());
    assert_eq!(&history.text_x()[..2], before.text_x());

    assert_eq!(history.rows.len(), 3);
    assert_eq!(history.cursor.as_deref(), Some("c2"));
    assert_eq!(history.oid_to_row.get(&Oid("ccc2".into())), Some(&2));
    assert!(!history.is_complete(), "a cursor means more pages remain");
}

#[test]
fn first_page_nonzero_row_rejects_before_mutation() {
    let err = LoadedHistory::from_first_page(page(
        vec![row(1, 0, "bbb1")],
        vec![],
        vec![],
        1,
        Some("c1"),
        GEN,
    ))
    .expect_err("page 1 must start at absolute row zero");
    assert_eq!(
        err,
        HistoryInvariantError::NonContiguousRow {
            expected: 0,
            actual: 1
        }
    );
}

#[test]
fn gap_or_reordered_page_rejects_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    // A gap: page 2 starts at row 3 when the aggregate ends at row 1.
    let err = history
        .append_page(
            "c1",
            page(vec![row(3, 0, "ddd3")], vec![], vec![], 1, None, GEN),
        )
        .expect_err("a page that skips a row must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::NonContiguousRow {
            expected: 2,
            actual: 3
        }
    );
    assert_eq!(history, before);

    // Reordered within the page: the rows are the right *set*, wrong order.
    let err = history
        .append_page(
            "c1",
            page(
                vec![row(3, 0, "ddd3"), row(2, 0, "ccc2")],
                vec![],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect_err("a reordered page must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::NonContiguousRow {
            expected: 2,
            actual: 3
        }
    );
    assert_eq!(history, before);
}

#[test]
fn oid_index_rejects_duplicate_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    // Re-delivering a commit the aggregate already holds.
    let err = history
        .append_page(
            "c1",
            page(vec![row(2, 0, "aaa0")], vec![], vec![], 1, None, GEN),
        )
        .expect_err("an OID already in the aggregate must be refused");
    assert_eq!(err, HistoryInvariantError::DuplicateOid(Oid("aaa0".into())));
    assert_eq!(history, before);

    // And a page that repeats an OID inside itself.
    let err = history
        .append_page(
            "c1",
            page(
                vec![row(2, 0, "ccc2"), row(3, 0, "ccc2")],
                vec![],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect_err("an OID repeated within one page must be refused");
    assert_eq!(err, HistoryInvariantError::DuplicateOid(Oid("ccc2".into())));
    assert_eq!(history, before);
}

#[test]
fn duplicate_edge_rejects_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    let err = history
        .append_page(
            "c1",
            page(
                vec![row(2, 0, "ccc2")],
                vec![edge(0, 0, 2, 0), edge(0, 0, 2, 0)],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect_err("the same four-field edge twice in one page must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::DuplicateEdge {
            from_row: 0,
            from_lane: 0,
            to_row: 2,
            to_lane: 0
        }
    );
    assert_eq!(history, before);
}

#[test]
fn generation_mismatch_rejects_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    let err = history
        .append_page("c1", page_two_with_generation("g2"))
        .expect_err("a page minted against another generation must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::GenerationMismatch {
            expected: generation("g1"),
            actual: generation("g2"),
        }
    );
    assert_eq!(history, before);
}

fn page_two_with_generation(generation_value: &str) -> Page {
    page(
        vec![row(2, 0, "ccc2")],
        vec![edge(1, 0, 2, 0)],
        vec![],
        1,
        Some("c2"),
        generation_value,
    )
}

#[test]
fn stale_cursor_rejects_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    let err = history
        .append_page("c0", page_two())
        .expect_err("a response to a cursor the aggregate has moved past must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::CursorChanged {
            requested: "c0".into(),
            current: Some("c1".into()),
        }
    );
    assert_eq!(history, before);
}

#[test]
fn destination_page_accepts_cross_page_source_edge() {
    let mut history = seeded();

    // from_row 0 lives in page 1, to_row 2 is this page's own row: the page
    // owning the *destination* owns the edge.
    history
        .append_page(
            "c1",
            page(
                vec![row(2, 0, "ccc2")],
                vec![edge(0, 0, 2, 0)],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect("a cross-page source edge is valid on the destination's page");

    assert_eq!(history.rows.len(), 3);
    assert_eq!(history.edges.last(), Some(&edge(0, 0, 2, 0)));
    assert!(
        history.is_complete(),
        "no cursor means the history is whole"
    );
}

#[test]
fn prefix_destination_edge_rejects_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    // to_row 1 belongs to page 1, which already delivered this edge.
    let err = history
        .append_page(
            "c1",
            page(
                vec![row(2, 0, "ccc2")],
                vec![edge(0, 0, 1, 0)],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect_err("an edge landing in the prefix must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::EdgeDestinationOutsidePage {
            page_start: 2,
            page_end: 3,
            to_row: 1,
        }
    );
    assert_eq!(history, before);
}

#[test]
fn future_destination_edge_rejects_before_mutation() {
    let mut history = seeded();
    let before = history.clone();

    // to_row 3 belongs to a page that hasn't arrived yet.
    let err = history
        .append_page(
            "c1",
            page(
                vec![row(2, 0, "ccc2")],
                vec![edge(0, 0, 3, 0)],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect_err("an edge landing past this page must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::EdgeDestinationOutsidePage {
            page_start: 2,
            page_end: 3,
            to_row: 3,
        }
    );
    assert_eq!(history, before);
}

#[test]
fn lane_high_water_regression_rejects_before_mutation() {
    // Seed with two commit lanes, then offer a page claiming only one: lanes
    // never shrink, so an already-drawn lane can't vanish under the graph.
    let mut history = LoadedHistory::from_first_page(page(
        vec![row(0, 0, "aaa0"), row(1, 1, "bbb1")],
        vec![edge(0, 0, 1, 1)],
        vec![],
        2,
        Some("c1"),
        GEN,
    ))
    .expect("a two-lane page 1 is valid");
    let before = history.clone();

    let err = history
        .append_page(
            "c1",
            page(
                vec![row(2, 0, "ccc2")],
                vec![edge(1, 1, 2, 0)],
                vec![],
                1,
                None,
                GEN,
            ),
        )
        .expect_err("a shrinking lane high-water must be refused");
    assert_eq!(
        err,
        HistoryInvariantError::LaneHighWaterRegressed {
            previous: 2,
            actual: 1,
        }
    );
    assert_eq!(history, before);
}

#[test]
fn valid_append_updates_cursor_lane_stubs_atomically() {
    let mut history = seeded();

    let delta = history
        .append_page(
            "c1",
            page(
                vec![row(2, 1, "ccc2")],
                vec![edge(0, 0, 2, 1)],
                vec![stub("wip", "ccc2", 0)],
                2,
                Some("c2"),
                GEN,
            ),
        )
        .expect("page 2 is valid");

    assert_eq!(history.rows.len(), 3);
    assert_eq!(history.cursor.as_deref(), Some("c2"));
    assert_eq!(history.lane_high_water, 2);
    assert_eq!(history.oid_to_row.len(), 3);
    assert_eq!(history.oid_to_row.get(&Oid("ccc2".into())), Some(&2));

    // The stub resolves against its own page's anchor row and sits past the
    // commit lanes.
    assert_eq!(
        history.resolved_stubs(),
        vec![ResolvedStub {
            stub: stub("wip", "ccc2", 0),
            anchor_row: 2,
            anchor_lane: 1,
            lane: 2,
        }]
    );

    // The lane-changing edge widens the rows it passes through, so the delta
    // tells the view its old labels moved.
    assert_eq!(
        delta,
        AppendDelta {
            old_row_count: 2,
            prefix_geometry_changed: true,
            stub_geometry_changed: true,
        }
    );
    assert!(history.label_occupancy()[0] >= 1);
    assert_eq!(
        history.text_x().len(),
        history.rows.len(),
        "text-x is populated from the committed occupancy"
    );
}

#[test]
fn stale_page_request_key_is_not_current() {
    let key = PageRequestKey {
        epoch: 7,
        generation: generation("g1"),
        cursor: "c1".into(),
    };

    assert!(key.is_current(7, &generation("g1"), Some("c1")));
    // Each of the three coordinates alone makes the reply stale.
    assert!(!key.is_current(8, &generation("g1"), Some("c1")), "epoch");
    assert!(
        !key.is_current(7, &generation("g2"), Some("c1")),
        "generation"
    );
    assert!(!key.is_current(7, &generation("g1"), Some("c2")), "cursor");
    assert!(
        !key.is_current(7, &generation("g1"), None),
        "a completed history has no cursor left to match"
    );
}

#[test]
fn prefetch_uses_one_point_five_viewports_and_single_flight() {
    let idle = PageLoadState::Idle;

    // 15 rows of lookahead at scale 1: 85 + 15 reaches 100, 84 + 15 doesn't.
    assert!(should_prefetch(
        85, 100, VIEWPORT_H, 1.0, &idle, true, false
    ));
    assert!(!should_prefetch(
        84, 100, VIEWPORT_H, 1.0, &idle, true, false
    ));
    // Zoomed out, a viewport covers more rows, so the lookahead grows with 1/scale.
    assert!(should_prefetch(
        70, 100, VIEWPORT_H, 0.5, &idle, true, false
    ));
    assert!(!should_prefetch(
        69, 100, VIEWPORT_H, 0.5, &idle, true, false
    ));
    // A degenerate scale must saturate, not divide by zero.
    assert!(should_prefetch(0, 100, VIEWPORT_H, 0.0, &idle, true, false));

    // Single flight: a request already in the air blocks another.
    assert!(!should_prefetch(
        85,
        100,
        VIEWPORT_H,
        1.0,
        &PageLoadState::Loading {
            cursor: "c1".into()
        },
        true,
        false
    ));
    assert!(!should_prefetch(
        85,
        100,
        VIEWPORT_H,
        1.0,
        &PageLoadState::Error {
            cursor: "c1".into(),
            message: "boom".into(),
            retry: PageRetry::SameCursor,
        },
        true,
        false
    ));
    // A complete history has no cursor to follow.
    assert!(!should_prefetch(
        85, 100, VIEWPORT_H, 1.0, &idle, false, false
    ));
}

#[test]
fn eager_prefetch_ignores_viewport_proximity_but_still_respects_flight_and_cursor() {
    // #217: once `want_full_history` is set, the App drives pagination back
    // to completion after an epoch bump regardless of where the camera
    // sits — the whole point is that the camera is back at the top
    // (`home`) and nowhere near the loaded edge yet.
    let idle = PageLoadState::Idle;
    assert!(
        should_prefetch(0, 100, VIEWPORT_H, 1.0, &idle, true, true),
        "eager bypasses the viewport lookahead check"
    );

    // Still single-flight: a request already in the air blocks another,
    // eager or not — eager must never stack concurrent requests.
    assert!(!should_prefetch(
        0,
        100,
        VIEWPORT_H,
        1.0,
        &PageLoadState::Loading {
            cursor: "c1".into()
        },
        true,
        true
    ));
    // Still stops the instant there is nothing left to fetch — eager keeps
    // asking for the next page, it doesn't invent one.
    assert!(
        !should_prefetch(0, 100, VIEWPORT_H, 1.0, &idle, false, true),
        "a complete history has no cursor left to chase, eager or not"
    );
    // Still doesn't retry a failed page on its own — the same "the user
    // asks" rule `page_error_blocks_prefetch_until_explicit_retry` pins.
    assert!(!should_prefetch(
        0,
        100,
        VIEWPORT_H,
        1.0,
        &PageLoadState::Error {
            cursor: "c1".into(),
            message: "boom".into(),
            retry: PageRetry::SameCursor,
        },
        true,
        true
    ));
}

/// #217 (review finding): the whole fix rests on one invariant the pure
/// function above cannot see — `app/mod.rs`'s epoch-reset effect resets
/// `complete` and `print_open` but must NOT touch `want_full_history`,
/// since that latch surviving the bump is exactly what lets a fresh epoch
/// resume pagination instead of leaving Print Graph dark. That wiring is
/// wasm-only and unreachable from a host test, so pin it at the source
/// level — the same thing `features/a11y/audit.rs` does for DOM
/// invariants it cannot mount. Without this, folding the third field in
/// beside its two siblings (three lines away) silently reintroduces the
/// original bug with `cargo test` fully green.
#[test]
fn the_epoch_reset_effect_does_not_clear_the_full_history_latch() {
    const APP_MOD: &str = include_str!("../../../app/mod.rs");
    let after = APP_MOD
        .split_once("let epoch = graph.get().epoch();")
        .expect("app/mod.rs no longer contains the epoch-reset effect")
        .1;
    let body = &after[..after
        .find("    });")
        .expect("the epoch-reset effect is no longer a closed block")];
    assert!(
        body.contains("print_open.set(false)") && body.contains("complete.set(false)"),
        "the epoch-reset effect no longer resets the two flags it must reset — \
         this test's anchor has drifted and its guarantee is now vacuous"
    );
    assert!(
        !body.contains("want_full_history"),
        "the epoch-reset effect now touches `want_full_history`. Clearing it \
         there reintroduces #217: Print Graph goes dark after every Refresh / \
         write-settle / drift reload and stays dark until the user manually \
         re-scrolls the entire history. Repository-switch leakage is handled by \
         comparing the latched worktree id at the point of use in canvas.rs, \
         not by clearing the latch here. Effect body was:\n{body}"
    );
}

#[test]
fn page_error_blocks_prefetch_until_explicit_retry() {
    // Same camera boundary throughout: only the load state differs.
    let at_boundary =
        |state: &PageLoadState| should_prefetch(85, 100, VIEWPORT_H, 1.0, state, true, false);

    let failed = PageLoadState::Error {
        cursor: "c1".into(),
        message: "500 Internal Server Error".into(),
        retry: PageRetry::SameCursor,
    };
    assert!(
        !at_boundary(&failed),
        "a failed page must not be retried by the camera"
    );
    assert!(
        at_boundary(&PageLoadState::Idle),
        "the user's explicit Retry clears the error and the same camera fetches"
    );
}

#[test]
fn bad_cursor_retry_reseeds_instead_of_reusing_cursor() {
    let rejected = PageLoadState::Error {
        cursor: "stale-cursor".into(),
        message: "400 Bad Request".into(),
        retry: PageRetry::Reseed,
    };
    assert!(
        !should_prefetch(85, 100, VIEWPORT_H, 1.0, &rejected, true, false),
        "the rejected cursor is never fetched again automatically"
    );

    // Retry bumps the reload epoch (App owns that signal); the in-flight key
    // carrying the rejected cursor is stale the moment it does, so a late
    // reply can't re-enter the new aggregate.
    let key = PageRequestKey {
        epoch: 7,
        generation: generation(GEN),
        cursor: "stale-cursor".into(),
    };
    assert!(!key.is_current(8, &generation(GEN), Some("stale-cursor")));

    // And the reseed starts from page 1, which carries the server's fresh
    // cursor — never the rejected one.
    let reseeded = seeded();
    assert_eq!(reseeded.cursor.as_deref(), Some("c1"));
    assert_ne!(reseeded.cursor.as_deref(), Some("stale-cursor"));
    assert!(should_prefetch(
        85,
        100,
        VIEWPORT_H,
        1.0,
        &PageLoadState::Idle,
        true,
        false
    ));
}

#[test]
fn fixed_loading_overlay_depends_only_on_page_state() {
    // The overlay's *placement* (an untransformed HTML child of `.graph`, so
    // over-pan can't carry it off-screen) is the canvas view's job; what this
    // module owns is the one state that shows it.
    assert!(show_fixed_loading_overlay(&PageLoadState::Loading {
        cursor: "c1".into()
    }));
    assert!(!show_fixed_loading_overlay(&PageLoadState::Idle));
    assert!(!show_fixed_loading_overlay(&PageLoadState::Error {
        cursor: "c1".into(),
        message: "boom".into(),
        retry: PageRetry::SameCursor,
    }));
    assert!(!show_fixed_loading_overlay(&PageLoadState::Error {
        cursor: "c1".into(),
        message: "400 Bad Request".into(),
        retry: PageRetry::Reseed,
    }));
}
