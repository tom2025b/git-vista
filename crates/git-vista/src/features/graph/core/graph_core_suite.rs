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

fn settlement_for(graph: &GraphCore, generation: Option<GenerationToken>) -> Invalidate {
    Invalidate {
        binding: graph.binding(),
        target: Some(("repository-x".into(), X.into())),
        generation,
        scope: InvalidateScope::Everything,
    }
}

#[test]
fn force_bump_advances_intent_revision() {
    let mut graph = bound();
    let before = graph.intent_revision();
    graph.force_bump();
    assert_eq!(
        graph.intent_revision(),
        before + 1,
        "Refresh/drift invalidates outstanding intent"
    );
}

#[test]
fn missing_generation_settlement_advances_intent_revision() {
    let mut graph = bound();
    let before = graph.intent_revision();
    assert_eq!(
        graph.on_invalidate(&settlement_for(&graph, None)),
        Applied::Committed
    );
    assert_eq!(
        graph.intent_revision(),
        before + 1,
        "missing-generation settlement invalidates outstanding intent"
    );
}

#[test]
fn changed_generation_settlement_advances_intent_revision() {
    let mut graph = bound();
    let before = graph.intent_revision();
    assert_eq!(
        graph.on_invalidate(&settlement_for(&graph, Some(gen("settled")))),
        Applied::Committed
    );
    assert_eq!(
        graph.intent_revision(),
        before + 1,
        "changed-generation settlement invalidates outstanding intent"
    );
}

#[test]
fn render_epoch_counts_remain_exact_while_only_real_invalidations_advance_intent() {
    let mut graph = bound();
    // Outstanding RebuildToken still consumes the render epoch in phase 3.
    // Every event below retains the pre-phase count; Preview is untouched.
    graph.force_bump();
    assert_eq!((graph.epoch(), graph.intent_revision()), (1, 1));
    graph.on_invalidate(&settlement_for(&graph, None));
    assert_eq!((graph.epoch(), graph.intent_revision()), (2, 2));
    graph.on_invalidate(&settlement_for(&graph, Some(gen("settled"))));
    assert_eq!((graph.epoch(), graph.intent_revision()), (3, 3));
    assert_eq!(
        graph.on_invalidate(&settlement_for(&graph, Some(gen("settled")))),
        Applied::NoChange
    );
    assert_eq!((graph.epoch(), graph.intent_revision()), (3, 3));
    let binding = graph.binding().unwrap();
    assert_eq!(
        graph.force_bump_for_feed(&binding, Some(&gen("feed"))),
        Applied::Committed
    );
    assert_eq!((graph.epoch(), graph.intent_revision()), (4, 3));
    assert_eq!(
        graph.force_bump_for_feed(&binding, Some(&gen("feed"))),
        Applied::NoChange
    );
    assert_eq!(
        graph.on_invalidate(&settlement_for(&graph, Some(gen("feed")))),
        Applied::NoChange
    );
    assert_eq!(
        (graph.epoch(), graph.intent_revision()),
        (4, 3),
        "coalesced settlement changes neither counter"
    );
}

#[test]
fn phase_three_feed_reload_still_refuses_a_held_opener_until_shell_ownership_lands() {
    use crate::features::core_traits::{RequestKey, RequestTarget};
    use crate::features::operations::core::{OpenerState, PendingIntent};
    use crate::features::operations::kind::OperationKind;
    let mut graph = bound();
    let held = PendingIntent {
        seq: 1,
        key: RequestKey {
            epoch: graph.epoch(),
            generation: None,
            target: RequestTarget::Branch("topic".into()),
        },
        kind: OperationKind::Push {
            branch: "topic".into(),
            set_upstream: false,
            force: None,
        },
    };
    let mut openers = OpenerState::default();
    assert!(openers.admit(&held, &graph));
    graph.force_bump_for_feed(&graph.binding().unwrap(), Some(&gen("feed")));
    // TEMPORARY status-quo pin. PHASE 5 MUST INVERT THIS when O1 lands WITH
    // P2's universal Shell serial. Until then, admission across feed reloads
    // could let an old precheck hijack a newer confirmation opened off-path.
    assert!(
        !openers.admit(&held, &graph),
        "phase 3 deliberately still refuses held openers after feed reload"
    );
    assert_eq!(
        openers.notices().len(),
        1,
        "the interim refusal must be observable"
    );
}

#[test]
fn stage_success_after_feed_refetches_bound_status_without_resending() {
    use crate::features::status::detail::core::StageState;
    let mut graph = bound();
    let mut stage = StageState::default();
    let ticket = stage.begin(&graph, Some("X")).unwrap();
    assert!(
        stage.begin(&graph, Some("X")).is_none(),
        "a duplicate click cannot send another write"
    );
    graph.force_bump_for_feed(&graph.binding().unwrap(), Some(&gen("feed")));
    assert!(
        stage.busy(&graph),
        "feed reload does not release a sent action"
    );
    assert!(
        stage.complete(&ticket, &graph, &Ok(())),
        "success across feed must refetch X"
    );
    assert!(!stage.busy(&graph));
    assert!(stage.notices().is_empty());
    assert!(
        !stage.complete(&ticket, &graph, &Ok(())),
        "completion can refetch at most once"
    );
}

#[test]
fn stage_error_after_feed_is_visible_without_replacing_a_dialog() {
    use crate::features::status::detail::core::StageState;
    let mut graph = bound();
    let mut stage = StageState::default();
    let ticket = stage.begin(&graph, Some("X")).unwrap();
    graph.force_bump_for_feed(&graph.binding().unwrap(), Some(&gen("feed")));
    assert!(stage.complete(&ticket, &graph, &Err("index locked".into())));
    assert!(!stage.busy(&graph));
    assert_eq!(
        stage.notices()[0].message,
        format!("Stage request started from X ({X}) failed: index locked")
    );
    let serial = stage.notices()[0].serial;
    stage.dismiss(serial);
    assert!(stage.notices().is_empty());
    assert!(!stage.complete(&ticket, &graph, &Err("index locked".into())));
    assert!(
        stage.notices().is_empty(),
        "a dismissed outcome is not published twice"
    );
}

#[test]
fn stage_old_desk_reports_outcome_without_refetching_y_or_clearing_its_busy() {
    use crate::features::status::detail::core::StageState;
    for result in [Ok(()), Err("index locked".into())] {
        let mut graph = bound();
        let mut stage = StageState::default();
        let old = stage.begin(&graph, Some("X")).unwrap();
        let selection = graph.begin_selection();
        graph.finish_selection(selection, Some(Y));
        let y = frame(Some(Y));
        graph
            .accept_seed(&graph.seed_request(), &y, &y.generation)
            .unwrap();
        let current = stage.begin(&graph, Some("Y")).unwrap();
        assert!(
            !stage.complete(&old, &graph, &result),
            "old outcome must not refetch Y"
        );
        assert!(
            stage.busy(&graph),
            "old completion cannot clear newer busy state"
        );
        assert!(stage.notices()[0]
            .message
            .contains(&format!("started from X ({X})")));
        assert!(stage.complete(&current, &graph, &Ok(())));
        assert!(!stage.busy(&graph));
    }
}

#[test]
fn stage_real_invalidation_reports_completion_without_applying_it_to_current_status() {
    use crate::features::status::detail::core::StageState;
    let mut graph = bound();
    let mut stage = StageState::default();
    let ticket = stage.begin(&graph, Some("X")).unwrap();
    graph.force_bump();
    assert!(
        !stage.complete(&ticket, &graph, &Ok(())),
        "Refresh/drift is still an invalidation"
    );
    assert_eq!(stage.notices()[0].message, format!("Stage request started from X ({X}) completed. Review that repository's status before continuing."));
    assert!(!stage.busy(&graph));
}

#[test]
fn stage_selection_before_epoch_change_refuses_current_surface_but_keeps_outcome() {
    use crate::features::status::detail::core::StageState;
    let mut graph = bound();
    let mut stage = StageState::default();
    let ticket = stage.begin(&graph, Some("X")).unwrap();
    let epoch = graph.epoch();
    graph.begin_selection();
    assert_eq!(graph.epoch(), epoch);
    assert!(
        !stage.complete(&ticket, &graph, &Err("index locked".into())),
        "binding must fence independently of invalidation revision"
    );
    assert!(stage.notices()[0].message.contains("index locked"));
}

#[test]
fn stage_older_serial_cannot_refetch_or_clear_newer_action_on_same_desk() {
    use crate::features::status::detail::core::StageState;
    let mut graph = bound();
    let mut stage = StageState::default();
    let old = stage.begin(&graph, Some("X")).unwrap();
    graph.force_bump();
    let newer = stage.begin(&graph, Some("X")).unwrap();
    assert!(!stage.complete(&old, &graph, &Ok(())));
    assert!(
        stage.busy(&graph),
        "action serial protects a newer busy state even on X"
    );
    assert!(stage.complete(&newer, &graph, &Ok(())));
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
fn pinned_page_selector_refuses_a_foreign_frame_before_requesting_page_one() {
    let graph = bound();
    let request = graph.seed_request();
    let expected = frame(Some(X));
    let foreign = frame(Some(Y));
    assert_eq!(expected.generation, foreign.generation);
    assert_eq!(request.page_selector(&expected), Ok(Some(X)));
    // Exercise the pre-page boundary alone: accept_seed's independent guard
    // cannot rescue a selector that would send the next request to Y.
    assert_eq!(
        request.page_selector(&foreign),
        Err("The history response belongs to another repository."),
        "a pinned Frame read must not authorize page one on another worktree"
    );
}

#[test]
fn discovery_reply_cannot_replace_a_binding_accepted_since_its_capture() {
    let mut graph = GraphCore::default();
    let first = graph.seed_request();
    let pending_discovery = graph.seed_request();
    let accepted = frame(Some(X));
    let foreign = frame(Some(Y));
    graph
        .accept_seed(&first, &accepted, &accepted.generation)
        .unwrap();

    // Acceptance deliberately changes neither epoch nor binding revision.
    // A previously captured discovery ticket therefore still passes the
    // request fence, and discovery itself permits either usable worktree.
    assert!(graph.request_is_current(&pending_discovery));
    assert_eq!(pending_discovery.target, SeedTarget::Discovery);
    assert_eq!(pending_discovery.page_selector(&foreign), Ok(Some(Y)));
    assert_eq!(accepted.generation, foreign.generation);
    let before = graph.clone();
    assert_eq!(
        graph.accept_seed(&pending_discovery, &foreign, &foreign.generation),
        Err("This history response cannot replace the tab's repository."),
        "acceptance must refuse Y even when the captured discovery request permits it"
    );
    assert_eq!(
        graph, before,
        "a refused reply must leave the binding intact"
    );
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
fn selected_candidate_does_not_own_status_until_frame_and_page_are_accepted() {
    let mut graph = bound();
    let selection = graph.begin_selection();
    assert!(graph.finish_selection(selection, Some(Y)));
    let request = graph.seed_request();
    assert_eq!(request.selector(), Ok(Some(Y)));
    assert_eq!(
        graph.status_target().as_deref(),
        Some(X),
        "successful selection still leaves status on accepted X while Y is only a candidate"
    );
    let candidate = frame(Some(Y));
    assert!(graph
        .accept_seed(&request, &candidate, &gen("wrong-page"))
        .is_err());
    assert_eq!(graph.status_target().as_deref(), Some(X));
    graph
        .accept_seed(&request, &candidate, &candidate.generation)
        .unwrap();
    assert_eq!(graph.status_target().as_deref(), Some(Y));
}

#[test]
fn live_stage_binding_always_names_the_accepted_status_worktree() {
    // Public transitions only: never manufacture a Bound/accepted mismatch by
    // editing private fields. Stage completion relies on this API invariant.
    // Equality is exercised for accepted/reloaded/restored live targets;
    // explicit is_none assertions cover transitions without a live binding.
    let check = |graph: &GraphCore| {
        if let Some(binding) = graph.live_binding() {
            assert_eq!(
                graph.status_target().as_deref(),
                Some(binding.worktree.as_str()),
                "a usable live Stage binding must name the accepted status target"
            );
        }
    };
    let mut graph = GraphCore::default();
    check(&graph);
    assert!(graph.live_binding().is_none());
    let x = frame(Some(X));
    graph
        .accept_seed(&graph.seed_request(), &x, &x.generation)
        .unwrap();
    check(&graph);
    graph.force_bump_for_feed(&graph.binding().unwrap(), Some(&gen("feed")));
    check(&graph);
    graph.force_bump();
    check(&graph);

    let selection = graph.begin_selection();
    assert!(
        graph.live_binding().is_none(),
        "Selecting cannot authorize a Stage refetch"
    );
    check(&graph);
    graph.finish_selection(selection, Some(Y));
    assert!(
        graph.live_binding().is_none(),
        "Candidate is a seed target, not an admitted callback target"
    );
    check(&graph);
    let y = frame(Some(Y));
    let request = graph.seed_request();
    assert!(graph.accept_seed(&request, &y, &gen("wrong-page")).is_err());
    assert!(graph.live_binding().is_none());
    check(&graph);
    graph.accept_seed(&request, &y, &y.generation).unwrap();
    check(&graph);

    let failed = graph.begin_selection();
    graph.finish_selection(failed, None);
    assert_eq!(graph.live_binding().unwrap().worktree, Y);
    check(&graph); // Revision changes, accepted worktree identity does not.
    graph.show_as_of("observation".into(), 123, Some(X.into()));
    assert!(graph.live_binding().is_none());
    check(&graph);
    graph
        .accept_seed(&graph.seed_request(), &x, &x.generation)
        .unwrap();
    assert!(graph.live_binding().is_none());
    check(&graph);
    graph.return_to_live();
    assert_eq!(graph.live_binding().unwrap().worktree, X);
    check(&graph);

    let mut degraded = GraphCore::default();
    let anonymous = frame(None);
    degraded
        .accept_seed(&degraded.seed_request(), &anonymous, &anonymous.generation)
        .unwrap();
    assert!(degraded.live_binding().is_none());
    check(&degraded);
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
