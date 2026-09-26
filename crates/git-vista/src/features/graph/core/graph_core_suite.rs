//! `GraphCore`'s epoch/invalidation tests: extracted verbatim from
//! `graph_core_tests` (a `#[cfg(test)]` child module inline in `core.rs`) so
//! the parent file can be read as production code. A child module of `core`,
//! same as its siblings here, so it still reaches `core.rs`'s private items
//! through `super::`.

use super::*;

fn gen(s: &str) -> GenerationToken {
    GenerationToken::new(s).expect("valid generation token")
}

const X: &str = "00000000-0000-4000-8000-000000000001";
const Y: &str = "00000000-0000-4000-8000-000000000002";

fn frame(worktree: Option<&str>) -> Frame {
    Frame {
        generation: gen("identical-history"),
        refs: vec![],
        head_branch: None,
        head_state: Default::default(),
        branch_colors: vec![],
        repo_label: None,
        repo_id: Some("repository-x".into()),
        worktree_id: worktree.map(str::to_string),
        read_only: true,
        resettable: false,
        repo_url: None,
        remote_web_url: None,
    }
}

fn bound() -> GraphCore {
    let mut graph = GraphCore::default();
    let frame = frame(Some(X));
    graph
        .accept_seed(&graph.seed_request(), &frame, &frame.generation)
        .unwrap();
    graph
}

fn bound_at_generation(generation: &str) -> GraphCore {
    let mut graph = GraphCore::at_generation(generation);
    let frame = frame(Some(X));
    graph
        .accept_seed(&graph.seed_request(), &frame, &frame.generation)
        .unwrap();
    graph
}

#[test]
fn binding_requires_both_current_frame_and_matching_first_page() {
    let mut graph = GraphCore::default();
    let discovery = graph.seed_request();
    let frame = frame(Some(X));
    assert!(graph
        .accept_seed(&discovery, &frame, &gen("different-page"))
        .is_err());
    assert!(
        graph.binding().is_none(),
        "Frame alone cannot establish a binding"
    );
    graph
        .accept_seed(&discovery, &frame, &frame.generation)
        .unwrap();
    assert_eq!(graph.binding().unwrap().worktree, X);
    assert_eq!(
        graph.epoch(),
        discovery.epoch,
        "accepting a seed does not request another epoch"
    );
    assert_eq!(
        graph.binding_revision(),
        discovery.revision,
        "acceptance does not restart the resource"
    );
}

#[test]
fn equal_history_does_not_admit_another_repository_into_a_bound_seed() {
    let mut graph = bound();
    graph.force_bump();
    let request = graph.seed_request();
    let other = frame(Some(Y));
    assert!(
        graph
            .accept_seed(&request, &other, &other.generation)
            .is_err(),
        "same history token is not repository identity"
    );
    assert_eq!(graph.binding().unwrap().worktree, X);
}

#[test]
fn retired_discovery_cannot_bind_after_refresh_or_selection() {
    let mut graph = GraphCore::default();
    let old = graph.seed_request();
    graph.force_bump();
    let frame = frame(Some(X));
    assert!(graph.accept_seed(&old, &frame, &frame.generation).is_err());
    let next = graph.seed_request();
    graph.begin_selection();
    assert!(
        graph.accept_seed(&next, &frame, &frame.generation).is_err(),
        "binding revision fences replies before a selection bumps epoch"
    );
}

#[test]
fn replacement_requests_keep_the_target_while_the_displayed_epoch_is_retired() {
    let mut graph = bound();
    let binding = graph.binding().unwrap();
    for _cause in ["Refresh", "retry", "drift", "feed"] {
        graph.force_bump();
        let request = graph.seed_request();
        assert_eq!(
            request.selector(),
            Ok(Some(X)),
            "replacement Frame cannot rediscover session Y"
        );
        let frame = frame(Some(X));
        assert_eq!(request.page_selector(&frame), Ok(Some(X)));
        assert_eq!(
            graph.binding(),
            Some(binding.clone()),
            "binding survives absent status_frame"
        );
    }
}

#[test]
fn only_latest_selection_success_can_supply_a_candidate_target() {
    let mut graph = bound();
    let first = graph.begin_selection();
    let second = graph.begin_selection();
    assert!(
        !graph.finish_selection(first, Some(Y)),
        "older selection success cannot win"
    );
    assert!(graph.finish_selection(second, Some(Y)));
    assert!(
        graph.live_binding().is_none(),
        "candidate needs a validated seed before feed attachment"
    );
    let request = graph.seed_request();
    assert_eq!(request.selector(), Ok(Some(Y)));
    let frame = frame(Some(Y));
    graph
        .accept_seed(&request, &frame, &frame.generation)
        .unwrap();
    assert_eq!(graph.binding().unwrap().worktree, Y);
}

#[test]
fn failed_or_ambiguous_selection_retains_x_without_default_discovery() {
    let mut graph = bound();
    let old = graph.binding().unwrap();
    let ticket = graph.begin_selection();
    assert!(
        graph.live_binding().is_none(),
        "selection pauses following immediately"
    );
    let clone = crate::features::dialogs::core::clone_settlement::<String>(Err("timed out".into()));
    assert!(clone.bump_epoch);
    assert!(clone
        .alert
        .as_deref()
        .unwrap()
        .contains("check the repository picker"));
    assert!(graph.finish_selection(ticket, clone.mode_screen_for.as_deref()));
    assert_eq!(
        graph.seed_request().selector(),
        Ok(Some(X)),
        "failed clone must never request session default"
    );
    assert!(graph.binding().unwrap().revision > old.revision);
}

#[test]
fn status_read_after_retirement_keeps_the_accepted_target() {
    let mut graph = bound();
    let accepted_epoch = graph.epoch();
    graph.force_bump();
    assert_ne!(
        graph.epoch(),
        accepted_epoch,
        "status_frame is retired at this point"
    );
    assert_eq!(
        graph.status_target().as_deref(),
        Some(X),
        "retirement cannot turn a status read into session-default discovery"
    );
}

#[test]
fn ambiguous_first_selection_disables_discovery_and_a_late_success_cannot_restore_it() {
    let mut graph = GraphCore::default();
    let old = graph.begin_selection();
    let current = graph.begin_selection();
    assert!(graph.finish_selection(current, None));
    let before = graph.clone();
    assert!(!graph.finish_selection(old, Some(Y)));
    assert_eq!(graph, before);
    assert_eq!(graph.seed_request().selector(), Err(FOLLOWING_UNAVAILABLE));
}

#[test]
fn absent_empty_and_invalid_identity_disable_following_and_refresh_discovery() {
    for id in [None, Some(""), Some("/some/path"), Some("not-an-id")] {
        let mut graph = GraphCore::default();
        let frame = frame(id);
        graph
            .accept_seed(&graph.seed_request(), &frame, &frame.generation)
            .unwrap();
        assert!(graph.following_unavailable());
        assert!(graph.live_binding().is_none());
        graph.force_bump();
        assert_eq!(
            graph.seed_request().selector(),
            Err(FOLLOWING_UNAVAILABLE),
            "missing identity never authorizes another unselected request"
        );
    }
}

#[test]
fn historical_return_retains_the_explicit_target_after_session_moves() {
    let mut graph = bound();
    graph.show_as_of("observation".into(), 123, Some(X.into()));
    let frame = frame(Some(X));
    graph
        .accept_seed(&graph.seed_request(), &frame, &frame.generation)
        .unwrap();
    assert!(graph.live_binding().is_none());
    graph.return_to_live();
    assert_eq!(graph.seed_request().selector(), Ok(Some(X)));
}

#[test]
fn append_is_fenced_as_soon_as_selection_starts_before_an_epoch_bump() {
    let mut graph = bound();
    let request = graph.seed_request();
    assert_eq!(graph.page_target(&request, Some(X)).as_deref(), Some(X));
    graph.begin_selection();
    assert_eq!(
        graph.status_target().as_deref(),
        Some(X),
        "pending selection cannot default status onto another tab's desk"
    );
    assert_eq!(graph.epoch(), request.epoch);
    assert!(
        graph.page_target(&request, Some(X)).is_none(),
        "old append cannot land during selection"
    );
    assert!(
        graph.page_target(&request, None).is_none(),
        "append cannot fall back to default selection"
    );
}

#[test]
fn retained_graph_after_a_failed_read_cannot_be_relabelled_as_a_new_target() {
    let mut graph = bound();
    let accepted = graph.seed_request();
    graph.force_bump();
    assert!(graph.may_show_retained(&accepted, Some(X)));
    assert_eq!(
        graph.seed_request().selector(),
        Ok(Some(X)),
        "failed target still retries explicitly"
    );
    let selection = graph.begin_selection();
    graph.finish_selection(selection, Some(Y));
    assert!(
        !graph.may_show_retained(&accepted, Some(X)),
        "old X must not be shown as selected Y"
    );
}

#[test]
fn another_target_and_retired_revision_cannot_coalesce_or_reload_current_x() {
    let mut graph = bound_at_generation("same-token");
    let old = graph.binding().unwrap();
    let mut foreign = old.clone();
    foreign.worktree = Y.into();
    assert_eq!(
        graph.force_bump_for_feed(&foreign, Some(&gen("new-token"))),
        Applied::NoChange
    );
    assert_eq!(
        graph.on_invalidate(&Invalidate {
            binding: Some(foreign),
            target: Some(("repository-x".into(), Y.into())),
            generation: Some(gen("new-token")),
            scope: InvalidateScope::Everything,
        }),
        Applied::NoChange,
        "a Y settlement cannot create an X reload"
    );
    let selection = graph.begin_selection();
    graph.finish_selection(selection, None);
    let before = graph.epoch();
    assert_eq!(
        graph.on_invalidate(&Invalidate {
            binding: Some(old.clone()),
            target: Some(("repository-x".into(), X.into())),
            generation: Some(gen("same-token")),
            scope: InvalidateScope::Everything,
        }),
        Applied::NoChange,
        "a retired X revision cannot populate the current record"
    );
    assert_eq!(
        graph.force_bump_for_feed(&old, Some(&gen("same-token"))),
        Applied::NoChange
    );
    assert_eq!(graph.epoch(), before);
    let current = graph.binding().unwrap();
    assert_eq!(
        graph.force_bump_for_feed(&current, Some(&gen("same-token"))),
        Applied::Committed,
        "equal token from old binding cannot suppress current reload"
    );
}

#[test]
fn unknown_provenance_requests_a_pinned_comparison_without_storing_its_token() {
    let mut graph = bound();
    assert_eq!(
        graph.on_invalidate(&Invalidate {
            binding: None,
            target: Some(("repository-x".into(), X.into())),
            generation: Some(gen("unproven")),
            scope: InvalidateScope::Everything,
        }),
        Applied::Committed
    );
    assert_eq!(
        graph.epoch(),
        0,
        "resumed outcome must compare history first"
    );
    assert_eq!(graph.reconciliation(), 1);
    assert!(
        graph.generation.is_none(),
        "unknown issuing revision cannot store provenance"
    );
    assert_eq!(graph.seed_request().selector(), Ok(Some(X)));
}

#[test]
fn an_invalidation_carrying_the_generation_we_already_have_does_not_bump_the_epoch() {
    // The whole point of D3: stop re-reading everything after every write.
    let mut g = bound_at_generation("77");
    let before = g.epoch();
    let applied = g.on_invalidate(&Invalidate {
        binding: g.binding(),
        target: g.binding().map(|b| (b.repository.unwrap(), b.worktree)),
        generation: Some(gen("77")),
        scope: InvalidateScope::Graph,
    });
    assert_eq!(applied, Applied::NoChange);
    assert_eq!(g.epoch(), before, "nothing moved, so nothing re-reads");
}

#[test]
fn an_invalidation_carrying_a_newer_generation_bumps_the_epoch() {
    let mut g = bound_at_generation("77");
    let before = g.epoch();
    let applied = g.on_invalidate(&Invalidate {
        binding: g.binding(),
        target: g.binding().map(|b| (b.repository.unwrap(), b.worktree)),
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
    let mut g = bound_at_generation("77");
    let before = g.epoch();
    g.on_invalidate(&Invalidate {
        binding: g.binding(),
        target: g.binding().map(|b| (b.repository.unwrap(), b.worktree)),
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
    let mut g = bound_at_generation("77");
    let applied = g.on_invalidate(&Invalidate {
        binding: g.binding(),
        target: g.binding().map(|b| (b.repository.unwrap(), b.worktree)),
        generation: Some(gen("78")),
        scope: InvalidateScope::Everything,
    });
    assert_eq!(applied, Applied::Committed);
}

#[test]
fn force_bump_always_advances_regardless_of_generation_and_reports_the_new_epoch() {
    let mut g = bound_at_generation("77");
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
    let mut graph = bound_at_generation("live");
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
                graph.on_invalidate(&Invalidate {
                    binding: graph.binding(),
                    target: graph.binding().map(|b| (b.repository.unwrap(), b.worktree)),
                    scope,
                    generation
                }),
                Applied::NoChange
            );
            assert_eq!(graph, pinned);
        }
    }
    graph.return_to_live();
    assert_eq!(
        graph.on_invalidate(&Invalidate {
            binding: graph.binding(),
            target: graph.binding().map(|b| (b.repository.unwrap(), b.worktree)),
            scope: InvalidateScope::Graph,
            generation: Some(gen("fresh"))
        }),
        Applied::NoChange
    );
}
