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

#[test]
fn an_invalidation_scoped_elsewhere_is_ignored() {
    let mut g = GraphCore::at_generation("77");
    let before = g.epoch();
    let applied = g.on_invalidate(&Invalidate {
        generation: Some(gen("78")),
        scope: InvalidateScope::Activity,
    });
    assert_eq!(applied, Applied::NoChange);
    assert_eq!(g.epoch(), before);
}

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
