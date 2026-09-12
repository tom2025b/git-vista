//! `GraphCore`'s epoch/invalidation tests: extracted verbatim from
//! `graph_core_tests` (a `#[cfg(test)]` child module inline in `core.rs`) so
//! the parent file can be read as production code. A child module of `core`,
//! same as its siblings here, so it still reaches `core.rs`'s private items
//! through `super::`.

use super::*;

fn gen(s: &str) -> GenerationToken {
    GenerationToken::new(s).expect("valid generation token")
}

#[test]
fn an_invalidation_carrying_the_generation_we_already_have_does_not_bump_the_epoch() {
    // The whole point of D3: stop re-reading everything after every write.
    let mut g = GraphCore::at_generation("77");
    let before = g.epoch();
    let applied = g.on_invalidate(&Invalidate {
        generation: Some(gen("77")),
        scope: InvalidateScope::Graph,
    });
    assert_eq!(applied, Applied::NoChange);
    assert_eq!(g.epoch(), before, "nothing moved, so nothing re-reads");
}

#[test]
fn an_invalidation_carrying_a_newer_generation_bumps_the_epoch() {
    let mut g = GraphCore::at_generation("77");
    let before = g.epoch();
    let applied = g.on_invalidate(&Invalidate {
        generation: Some(gen("78")),
        scope: InvalidateScope::Graph,
    });
    assert_eq!(applied, Applied::Committed);
    assert_eq!(g.epoch(), before + 1);
}

#[test]
fn an_invalidation_with_no_generation_bumps_conservatively() {
    // The server could not read a generation after execution (ADR 0020 allows None).
    // Re-reading is the safe default; silently skipping would strand a stale graph.
    let mut g = GraphCore::at_generation("77");
    let before = g.epoch();
    g.on_invalidate(&Invalidate {
        generation: None,
        scope: InvalidateScope::Graph,
    });
    assert_eq!(g.epoch(), before + 1);
}

// #783: `an_invalidation_scoped_elsewhere_is_ignored` used to live here,
// constructing `InvalidateScope::Activity` purely as a witness for "a scope
// GraphCore doesn't care about" — Activity itself had no producer and no
// consumer of its own anywhere in this crate. #783 deleted that variant (see
// `features/core_traits.rs`), which leaves `InvalidateScope` with only
// `Everything` and `Graph` — nothing left to construct as a third, ignored
// scope, so the test that needed one is gone with it. `on_invalidate`'s own
// `if !matches!(.., Graph | Everything)` guard is unchanged and still
// compiles; it simply has no reachable case to prove false right now. Whoever
// adds the next real scope should add this test back against it.

#[test]
fn an_invalidation_scoped_everything_still_bumps_the_graph() {
    // `OperationsCore::settle` always publishes `InvalidateScope::Everything`
    // (Task 4) — a write can move refs, the tree and the journal at once.
    let mut g = GraphCore::at_generation("77");
    let applied = g.on_invalidate(&Invalidate {
        generation: Some(gen("78")),
        scope: InvalidateScope::Everything,
    });
    assert_eq!(applied, Applied::Committed);
}

#[test]
fn force_bump_always_advances_regardless_of_generation_and_reports_the_new_epoch() {
    let mut g = GraphCore::at_generation("77");
    let before = g.epoch();
    let reported = g.force_bump();
    assert_eq!(g.epoch(), before + 1);
    assert_eq!(
        reported,
        before + 1,
        "the caller needs the epoch it is loading INTO"
    );
}

#[test]
fn as_of_switch_and_return_retire_requests_but_refresh_keeps_the_selector() {
    let mut graph = GraphCore::at_generation("live");
    let old_epoch = graph.epoch();
    graph.show_as_of("signed-observation".into(), 123, Some("worktree".into()));
    assert!(graph.view().is_historical());
    assert!(graph.epoch() > old_epoch);
    assert_eq!(graph.view().token(), Some("signed-observation"));
    assert_eq!(graph.view().repo(), Some("worktree"));
    let historical = graph.view().clone();
    graph.force_bump();
    assert_eq!(
        *graph.view(),
        historical,
        "refresh/retry must never return to live"
    );
    let retired = graph.epoch();
    graph.return_to_live();
    assert!(graph.epoch() > retired);
    assert_eq!(graph.view(), &HistoryView::Live);
    assert!(graph.view().token().is_none());
    assert!(graph.view().repo().is_none());
}

#[test]
fn as_of_background_mutations_do_not_replace_or_refresh_the_historical_epoch() {
    let mut graph = GraphCore::default();
    graph.show_as_of("observation".into(), 123, None);
    let pinned = graph.clone();
    for scope in [InvalidateScope::Graph, InvalidateScope::Everything] {
        for generation in [None, Some(gen("new-live-generation"))] {
            assert_eq!(
                graph.on_invalidate(&Invalidate { scope, generation }),
                Applied::NoChange
            );
            assert_eq!(graph, pinned);
        }
    }
    graph.return_to_live();
    assert_eq!(
        graph.on_invalidate(&Invalidate {
            scope: InvalidateScope::Graph,
            generation: Some(gen("fresh"))
        }),
        Applied::Committed
    );
}
