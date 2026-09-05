//! Host tests for the plan-freshness decision (#555).

use super::*;

use git_vista_protocol::change_feed::{ChangeFeedHealth, WatchBudget};
use git_vista_protocol::{GenerationToken, RefName, UnixSeconds};

fn watching() -> ChangeFeedHealth {
    ChangeFeedHealth::Watching {
        watches: 12,
        budget: WatchBudget::Derived {
            watches: 4038,
            from_watches: 516_898,
            from_instances: 128,
        },
    }
}

/// Snapshots in these tests are numbered by the order `log_of` records them,
/// so the ordinary case is a continuous chain; the gap tests below stamp their
/// own sequence deliberately.
fn named(generation: &str, refs: &[&str], other: bool) -> ChangeFeedSnapshot {
    ChangeFeedSnapshot {
        seq: 0,
        generation: Some(GenerationToken::new(generation).unwrap()),
        health: watching(),
        changed: RefDelta::Named {
            refs: refs.iter().map(|r| RefName::new(*r).unwrap()).collect(),
            other,
        },
        at: UnixSeconds(1),
    }
}

fn unknown_delta(generation: &str) -> ChangeFeedSnapshot {
    ChangeFeedSnapshot {
        seq: 0,
        generation: Some(GenerationToken::new(generation).unwrap()),
        health: watching(),
        changed: RefDelta::Unknown,
        at: UnixSeconds(1),
    }
}

fn blind() -> ChangeFeedSnapshot {
    ChangeFeedSnapshot {
        seq: 0,
        generation: None,
        health: ChangeFeedHealth::Blind {
            reason: "git status could not be run".to_string(),
            since: UnixSeconds(5),
        },
        changed: RefDelta::Unknown,
        at: UnixSeconds(5),
    }
}

/// A verdict over a plan that is on screen — the shape every assertion below
/// about freshness is really about.
fn ready(freshness: PlanFreshness) -> PlanVerdict {
    PlanVerdict::Fresh(freshness)
}

fn plan(generation: &str, expects: &[&str]) -> PlanOnScreen {
    PlanOnScreen {
        generation: generation.to_string(),
        expects: expects.iter().map(|e| (*e).to_string()).collect(),
    }
}

/// Record a run of snapshots as a **continuous** feed — seq 1, 2, 3 … — which
/// is what a client that received every publication holds.
fn log_of(snapshots: Vec<ChangeFeedSnapshot>) -> FeedLog {
    let mut log = FeedLog::new();
    for (n, snapshot) in snapshots.into_iter().enumerate() {
        log.record(ChangeFeedSnapshot {
            seq: n as u64 + 1,
            ..snapshot
        });
    }
    log
}

#[test]
fn a_plan_whose_generation_is_still_live_is_current_and_is_the_only_arm_that_is() {
    let log = log_of(vec![unknown_delta("100")]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Current);
    assert!(verdict.execute_offered());
    assert_eq!(freshness_headline(&verdict), None, "nothing to say");
}

#[test]
fn every_stale_arm_withdraws_the_execute_control() {
    // The panel may never be more optimistic than `enforce_fresh`, which
    // compares the whole digest and refuses on any movement.
    for verdict in [
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()],
        },
        PlanFreshness::Moved { refs: Vec::new() },
        PlanFreshness::MovedElsewhere,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected,
        },
        PlanFreshness::Unknown {
            reason: FeedUnavailable::Blind {
                reason: "unreadable".to_string(),
            },
        },
    ] {
        assert!(
            !verdict.execute_offered(),
            "{verdict:?} must not offer a button whose purpose is to fail"
        );
        assert!(
            freshness_headline(&verdict).is_some(),
            "{verdict:?} must say something"
        );
        assert!(rebuild_framing(&verdict).is_some());
        assert!(
            !confirm_enabled(true, &ready(verdict.clone())),
            "and the composed answer must withdraw it too"
        );
    }
}

#[test]
fn a_ref_the_plan_names_moving_is_named_back_to_the_user() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/heads/main"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()]
        }
    );
    assert_eq!(
        freshness_headline(&verdict).unwrap(),
        "refs/heads/main moved while this was on screen."
    );
}

#[test]
fn a_ref_the_plan_does_not_name_moving_is_said_differently_and_still_refused() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::MovedElsewhere);
    assert!(!verdict.execute_offered());
    assert!(freshness_headline(&verdict)
        .unwrap()
        .contains("not in a way this operation depends on"));
}

#[test]
fn a_working_tree_change_is_never_called_irrelevant() {
    // The reassuring sentence has to be earned. A worktree/index/stash move
    // names no ref and can still change what a commit writes, so it takes the
    // arm that claims least.
    let log = log_of(vec![unknown_delta("100"), named("101", &[], true)]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Moved { refs: Vec::new() });
    assert_eq!(
        freshness_headline(&verdict).unwrap(),
        "The repository changed while this was on screen."
    );
}

#[test]
fn a_gap_in_what_the_client_saw_cannot_produce_the_reassuring_answer() {
    // The client reconnected: the first snapshot after the gap can name
    // nothing. Anything that happened during the outage is unaccounted for, so
    // "not in a way this operation depends on" is not available.
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
        unknown_delta("102"),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Moved { refs: Vec::new() });
}

#[test]
fn a_plan_generation_the_client_never_saw_cannot_be_differenced() {
    let log = log_of(vec![named("101", &["refs/tags/v9"], false)]);
    let verdict = freshness(&plan("nobody-saw-this", &["refs/heads/main"]), &log);
    assert_eq!(verdict, PlanFreshness::Moved { refs: Vec::new() });
}

#[test]
fn a_blind_feed_says_it_could_not_tell_rather_than_that_nothing_changed() {
    let log = log_of(vec![unknown_delta("100"), blind()]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::Blind {
                reason: "git status could not be run".to_string()
            }
        },
        "the plan's own generation is still the live one as far as anybody \
         knows — and that is exactly the reasoning ADR 0055 forbids"
    );
}

#[test]
fn no_feed_at_all_is_unknown_and_not_current() {
    let verdict = freshness(&plan("100", &[]), &FeedLog::new());
    assert_eq!(
        verdict,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected
        }
    );
}

#[test]
fn a_dropped_stream_forgets_what_it_saw_rather_than_differencing_across_the_gap() {
    let mut log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
    ]);
    log.clear();
    assert!(log.is_empty());
    log.record(named("102", &["refs/tags/v9"], false));
    // 100 is no longer in the log, so nothing can be named across the outage.
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::Moved { refs: Vec::new() }
    );
}

#[test]
fn several_moves_accumulate_and_the_ref_the_plan_names_is_still_found() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/tags/v9"], false),
        named("102", &["refs/remotes/origin/main"], false),
        named("103", &["refs/heads/main"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()]
        },
        "the plan was built three snapshots ago; the whole span is differenced"
    );
}

#[test]
fn a_plan_that_returns_to_its_own_generation_is_current_again() {
    // A ref moved and moved back. `enforce_fresh` would admit this plan — the
    // digest is equal — so the panel must not be more pessimistic than the gate
    // either.
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/heads/main"], false),
        named("100", &["refs/heads/main"], false),
    ]);
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::Current
    );
}

#[test]
fn the_log_is_bounded_and_drops_the_oldest_first() {
    let mut log = FeedLog::new();
    for n in 0..(LOG_DEPTH + 5) {
        log.record(ChangeFeedSnapshot {
            seq: n as u64 + 1,
            ..named(&n.to_string(), &[], false)
        });
    }
    assert_eq!(
        log.latest().unwrap().generation.as_ref().unwrap().as_str(),
        (LOG_DEPTH + 4).to_string()
    );
    assert!(
        log.moved_since("0").is_none(),
        "a generation older than the log cannot be differenced against"
    );
    assert!(log.moved_since(&(LOG_DEPTH + 3).to_string()).is_some());
}

#[test]
fn two_named_refs_read_as_a_sentence() {
    let log = log_of(vec![
        unknown_delta("100"),
        named("101", &["refs/heads/main", "refs/heads/side"], false),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main", "refs/heads/side"]), &log);
    assert_eq!(
        freshness_headline(&verdict).unwrap(),
        "refs/heads/main and refs/heads/side moved while this was on screen."
    );
}

#[test]
fn a_confirmation_with_no_plan_on_screen_is_left_exactly_as_it_was() {
    // Most confirmations in this app have no preview and therefore no plan.
    // This feature must make no claim about them: a feed that has not connected
    // yet would otherwise disable half the dialogs in the app.
    assert!(confirm_enabled(true, &PlanVerdict::NoPlan));
    assert!(!confirm_enabled(false, &PlanVerdict::NoPlan));
    assert_eq!(blocked_by_staleness(&PlanVerdict::NoPlan), None);
}

#[test]
fn a_stale_plan_withdraws_the_confirmation_and_says_which_kind_of_stale() {
    let moved = ready(PlanFreshness::Moved {
        refs: vec!["refs/heads/main".to_string()],
    });
    assert!(!confirm_enabled(true, &moved));
    assert!(blocked_by_staleness(&moved)
        .unwrap()
        .contains("moved after this picture was drawn"));

    let unknown = ready(PlanFreshness::Unknown {
        reason: FeedUnavailable::NotConnected,
    });
    assert!(!confirm_enabled(true, &unknown));
    assert!(blocked_by_staleness(&unknown)
        .unwrap()
        .contains("isn't known"));

    // Reassuring, and still refused — `enforce_fresh` compares the whole
    // digest, so this plan would 409.
    assert!(!confirm_enabled(
        true,
        &ready(PlanFreshness::MovedElsewhere)
    ));
    assert!(blocked_by_staleness(&ready(PlanFreshness::MovedElsewhere)).is_some());
}

#[test]
fn a_current_plan_leaves_the_dialogs_own_verdict_alone() {
    assert!(confirm_enabled(true, &ready(PlanFreshness::Current)));
    assert!(
        !confirm_enabled(false, &ready(PlanFreshness::Current)),
        "freshness may withdraw a confirmation, never grant one"
    );
    assert_eq!(blocked_by_staleness(&ready(PlanFreshness::Current)), None);
}

// --- the seam census -------------------------------------------------------
//
// `core.rs` being pure and host-tested does not prove the wasm-only code asks
// it the questions it answers. `cargo test` compiles none of the three files
// below, so what binds them is a source-level census — the habit
// `features/mod.rs` describes, applied at the moment a decision moved out of a
// view file. It is deliberately about *specific callers asking specific
// functions*, not about purity in general.

/// The wasm-only reactive wrapper: one `EventSource`, and the log the decision
/// reads.
const FEED_SIGNALS: &str = include_str!("signals.rs");

/// The wasm-only preview wrapper: where the plan on screen is remembered.
const PREVIEW_SIGNALS: &str = include_str!("../preview/signals.rs");

/// The wasm-only confirm dialog: where the verdict withdraws a button.
const CONFIRM_DIALOG: &str = include_str!("../../dialogs/confirm.rs");

#[test]
fn the_feed_subscription_asks_core_for_the_verdict_and_forgets_across_a_gap() {
    assert!(
        FEED_SIGNALS.contains("verdict(slot, log)"),
        "the wasm wrapper must ask `core::verdict`, never re-derive the \
         answer where no host test compiles it"
    );
    assert!(
        FEED_SIGNALS.contains("log.clear()"),
        "a dropped stream must forget what it saw — differencing across a gap \
         is how the reassuring sentence gets printed over changes nobody saw"
    );
    assert!(
        FEED_SIGNALS.contains("PROTOCOL_QUERY"),
        "EventSource cannot set headers, so the version rides in the query \
         string — the server allows it for this path alone"
    );
}

#[test]
fn the_preview_remembers_the_plan_it_drew_and_drops_it_with_the_picture() {
    assert!(
        PREVIEW_SIGNALS.contains("PlanOnScreen {"),
        "the plan's generation and expected refs are what make a freshness \
         question answerable at all"
    );
    assert!(
        PREVIEW_SIGNALS.contains("expected_ref_changes"),
        "the refs the plan names are what separate `Moved` from \
         `MovedElsewhere`"
    );
    let clear = PREVIEW_SIGNALS
        .split("pub fn clear(&self)")
        .nth(1)
        .expect("Preview::clear exists");
    assert!(
        clear.contains("self.plan.set(PlanSlot::Absent)"),
        "clearing the panel must clear the plan: a generation left behind \
         answers the next dialog's freshness question with the last one's plan"
    );
}

#[test]
fn the_confirm_dialog_composes_the_two_halves_rather_than_reimplementing_them() {
    assert!(
        CONFIRM_DIALOG.contains("confirm_enabled(enabled, &plan_freshness)"),
        "the composition itself must be the host-tested function — an `&&` \
         written in this file is exactly #612's origin"
    );
    assert!(
        CONFIRM_DIALOG.contains("blocked_by_staleness(&plan_freshness)"),
        "a withdrawn button must carry its reason; #65's finding is that an \
         unspoken one is unreachable by the user it was written for"
    );
    assert!(
        CONFIRM_DIALOG.contains("freshness_notice_view(preview, freshness)"),
        "and the notice must actually be rendered, above the picture it \
         invalidates"
    );
    let notice = CONFIRM_DIALOG
        .find("freshness_notice_view(preview, freshness)")
        .expect("the notice is rendered");
    let panel = CONFIRM_DIALOG
        .find("preview_panel_view(preview)")
        .expect("the picture is rendered");
    assert!(
        notice < panel,
        "the staleness warning goes ABOVE the picture: printed under it, a \
         reader meets it after they have already believed the picture"
    );
}

// --- #664 review, finding 3: a delta read across a gap is not a delta ------

/// Record snapshots at exactly the sequence numbers given — a client that
/// missed the ones in between.
fn log_at(numbered: Vec<(u64, ChangeFeedSnapshot)>) -> FeedLog {
    let mut log = FeedLog::new();
    for (seq, snapshot) in numbered {
        log.record(ChangeFeedSnapshot { seq, ..snapshot });
    }
    log
}

#[test]
fn a_publication_this_client_never_received_cannot_produce_the_reassuring_answer() {
    // codex's reproduction, as a test. The feed's transport keeps only the
    // latest value, so a slow reader skips publications WITHOUT disconnecting:
    // `refs/heads/main` moves in publication 2, an unrelated tag moves in
    // publication 3, and this client polls once and sees only 3.
    //
    // Read as a chain, that says "only a tag moved" — and a plan expecting
    // `main` is told the repository moved "but not in a way this operation
    // depends on". The button is still withdrawn, so nothing unsafe happens;
    // the EXPLANATION is false, which is this milestone's own failure shape
    // pointed at itself.
    let log = log_at(vec![
        (1, unknown_delta("100")),
        // publication 2 — naming refs/heads/main — never arrived
        (3, named("102", &["refs/tags/v9"], false)),
    ]);
    let verdict = freshness(&plan("100", &["refs/heads/main"]), &log);
    assert_eq!(
        verdict,
        PlanFreshness::Moved { refs: Vec::new() },
        "a gap in the sequence means this client cannot name what moved"
    );
    assert_ne!(
        verdict,
        PlanFreshness::MovedElsewhere,
        "and it certainly cannot say the change was irrelevant"
    );
}

#[test]
fn a_continuous_run_is_still_read_as_a_chain() {
    // The other half: the fix must not make every delta unusable. A client that
    // received every publication reads them exactly as before.
    let log = log_at(vec![
        (1, unknown_delta("100")),
        (2, named("101", &["refs/tags/v9"], false)),
        (3, named("102", &["refs/tags/v10"], false)),
    ]);
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::MovedElsewhere,
        "two tags moved, this plan names neither, and nothing was missed"
    );
}

#[test]
fn the_first_snapshot_on_a_stream_is_never_read_as_a_delta() {
    // A client that connects to a feed already running receives the current
    // snapshot, whose `changed` is a difference against a publication made
    // before this client existed.
    let log = log_at(vec![(9, named("102", &["refs/tags/v9"], false))]);
    assert!(
        log.moved_since("102").is_some(),
        "the reading itself is perfectly good"
    );
    assert_eq!(
        freshness(&plan("100", &["refs/heads/main"]), &log),
        PlanFreshness::Moved { refs: Vec::new() },
        "but its delta describes a span this client did not watch"
    );
}

// --- #664 review, findings 6 and 7 -----------------------------------------

use crate::features::operations::kind::{CheckoutElsewhere, ForceWithLease, OperationKind};
use git_vista_protocol::plan::{Advisory, RiskLevel};
use git_vista_protocol::{CommitOid, Explanation};

fn force_push_op(plan: PlanOnScreen) -> OperationKind {
    OperationKind::Push {
        branch: "main".to_string(),
        set_upstream: false,
        force: Some(ForceWithLease {
            expected_remote_tip: CommitOid::new("0123456789abcdef0123456789abcdef01234567")
                .unwrap(),
            risk: RiskLevel::Destructive,
            advisories: Vec::<Advisory>::new(),
            explanation: Explanation {
                sections: Vec::new(),
            },
            plan,
        }),
    }
}

#[test]
fn a_force_push_carries_its_own_plan_because_it_has_no_preview() {
    // Finding 7. `preview_subject(Push)` is `NotPreviewable`, so `preview.plan()`
    // is `None` on the single most destructive confirmation in the app — while
    // that confirmation is displaying a server-built plan's explanation, its
    // risk, and the oid it will overwrite. Freshness taken only from the
    // preview saw nothing and left the button enabled.
    let carried = PlanOnScreen {
        generation: "100".to_string(),
        expects: vec!["refs/heads/main".to_string()],
    };
    let found = plan_on_screen(&force_push_op(carried.clone()), PlanSlot::Absent);
    assert_eq!(
        found,
        PlanSlot::Ready(carried),
        "a force-with-lease confirmation always has a plan on screen"
    );
}

#[test]
fn a_previewed_plan_still_wins_where_there_is_one() {
    let carried = PlanOnScreen {
        generation: "100".to_string(),
        expects: vec!["refs/heads/main".to_string()],
    };
    let previewed = PlanOnScreen {
        generation: "200".to_string(),
        expects: vec!["refs/heads/side".to_string()],
    };
    assert_eq!(
        plan_on_screen(&force_push_op(carried), PlanSlot::Ready(previewed.clone())),
        PlanSlot::Ready(previewed),
        "the preview's plan is the one whose picture is on screen"
    );
}

#[test]
fn a_confirmation_with_neither_kind_of_plan_makes_no_claim() {
    let op = OperationKind::Checkout {
        branch: "main".to_string(),
        current: Some("other".to_string()),
        elsewhere: CheckoutElsewhere::Free,
    };
    assert_eq!(plan_on_screen(&op, PlanSlot::Absent), PlanSlot::Absent);
}

#[test]
fn rebuild_is_offered_on_every_stale_arm_and_none_of_the_current_ones() {
    // Finding 6: spec D4 requires Rebuild and Discard, and the first slice
    // shipped the sentence telling the user to rebuild with no way to do it.
    for stale in [
        PlanFreshness::Moved {
            refs: vec!["refs/heads/main".to_string()],
        },
        PlanFreshness::Moved { refs: Vec::new() },
        PlanFreshness::MovedElsewhere,
        PlanFreshness::Unknown {
            reason: FeedUnavailable::NotConnected,
        },
    ] {
        assert!(
            rebuild_is_offered(&ready(stale.clone())),
            "{stale:?} tells the user to rebuild, so it must let them"
        );
        assert!(
            rebuild_framing(&stale).is_some(),
            "and the offer is explained"
        );
    }
    assert!(!rebuild_is_offered(&ready(PlanFreshness::Current)));
    assert!(
        !rebuild_is_offered(&PlanVerdict::NoPlan),
        "a confirmation with no plan on screen has nothing to rebuild"
    );
}

#[test]
fn the_dialog_offers_the_rebuild_it_talks_about_and_asks_core_which_plan() {
    // The seam census for both findings. Neither is visible to `cargo test`:
    // `dialogs/confirm.rs` is wasm-only, and every test above would pass with
    // the button absent and the force-push plan never consulted — which is
    // exactly how both shipped.
    assert!(
        CONFIRM_DIALOG.contains("freshness.of(&plan_on_screen(&op, preview.plan()))"),
        "the dialog must ask which plan is on screen, not assume the preview's \
         — and must fold it into ONE verdict, so the button, the notice and \
         the Rebuild offer cannot disagree"
    );
    assert!(
        CONFIRM_DIALOG.contains("rebuild_is_offered(&plan_freshness)"),
        "and must ask core whether to offer Rebuild, from that same verdict"
    );
    assert!(
        CONFIRM_DIALOG.contains("\"Rebuild\""),
        "a notice telling the user to rebuild, with no control that does, is \
         the defect this pins"
    );
    assert!(
        CONFIRM_DIALOG.contains("PreviewAction::Start(operation) => preview.rebuild(operation)"),
        "a previewable Rebuild fetches a NEW plan through the same path the \
         dialog opened with, in the REBUILD state — `start` would report \
         `Absent` while the request is in flight and re-enable the button over \
         a plan we know is stale (#664 review, defect 1)"
    );
    assert!(
        CONFIRM_DIALOG.contains("PreviewAction::Clear => rebuild_lease("),
        "and a NotPreviewable one — the force-with-lease push — must take its \
         own path. Routing it through `preview_action` alone resolves to \
         `Clear`, so the offered button issued no request at all (#664 review, \
         defect 2)"
    );
    let rebuild = CONFIRM_DIALOG
        .split("let rebuild = move || {")
        .nth(1)
        .expect("the rebuild handler exists");
    let body = &rebuild[..rebuild.find("};").expect("a closed block")];
    assert!(
        !body.contains("run_confirmed") && !body.contains("close_confirm"),
        "Rebuild never executes and never silently dismisses: it replaces the \
         plan and leaves the user to approve it again"
    );

    // The lease path's own two obligations, which nothing else in this file
    // can see: it must say it has started (or the button stays live over a
    // plan we know is stale) and it must say it landed (or the button stays
    // dead over a replacement that arrived).
    let lease = CONFIRM_DIALOG
        .split("fn rebuild_lease(")
        .nth(1)
        .expect("the force-with-lease rebuild path exists");
    assert!(
        lease.contains("preview.note_rebuild_started()"),
        "the lease rebuild must enter the rebuilding state before it awaits"
    );
    assert!(
        lease.contains("preview.note_rebuild_landed()"),
        "and must leave it when the replacement arrives, or the confirmation \
         stays withdrawn over a plan that is right there"
    );
    assert!(
        lease.matches("preview.note_rebuild_failed()").count() >= 3,
        "every way it can fail — either request, and a remote tip it cannot \
         read — must land in the same stated state rather than silently \
         leaving the dialog rebuilding forever"
    );
}

// --- #664 review round 2: the two transitions Rebuild passes through -------
//
// Both defects the review found live in a *transition*, not in a resting
// state: what is true while a replacement is being fetched, and what is true
// when the fetch fails. Every test above this line asserts a resting state,
// which is exactly why they all passed over both.

#[test]
fn a_rebuild_in_flight_does_not_re_enable_the_confirmation() {
    // Defect 1, and it is worse than the problem it replaced. Clicking Rebuild
    // cleared the plan; `confirm_enabled` saw "no plan" and returned **true**,
    // so the stale notice vanished and the execute control went live with no
    // replacement to review. The user got there by acting on being told the
    // plan was stale.
    //
    // The modal's own dispatch sends a branch-only request to the legacy merge
    // endpoint rather than submitting a plan, so the execution generation-gate
    // never sees it: this window could dispatch an unreviewed operation.
    let rebuilding = verdict(&PlanSlot::Rebuilding, &FeedLog::new());
    assert_eq!(rebuilding, PlanVerdict::Rebuilding);
    assert!(
        !confirm_enabled(true, &rebuilding),
        "there is nothing to approve while the replacement is in flight"
    );
    assert!(
        blocked_by_staleness(&rebuilding).is_some(),
        "and the reason is said, not merely enforced"
    );
    assert!(
        verdict_headline(&rebuilding).is_some(),
        "the notice must not vanish the moment Rebuild is pressed — that is \
         the user losing the only thing telling them why"
    );
    assert!(
        !rebuild_is_offered(&rebuilding),
        "and Rebuild is not offered twice for one replacement"
    );
}

#[test]
fn a_rebuild_that_failed_leaves_the_confirmation_withdrawn_and_offers_another_go() {
    // The second half of defect 1: "if `/api/plan` fails, that state persists."
    // A failed replacement is not the absence of a claim — it is a failed
    // attempt to replace a claim we know was stale.
    let failed = verdict(&PlanSlot::RebuildFailed, &FeedLog::new());
    assert_eq!(failed, PlanVerdict::RebuildFailed);
    assert!(!confirm_enabled(true, &failed));
    assert!(blocked_by_staleness(&failed)
        .unwrap()
        .contains("couldn't be built"));
    assert!(
        rebuild_is_offered(&failed),
        "trying again is the only useful thing left on this dialog"
    );
    assert!(verdict_headline(&failed).is_some());
    assert!(verdict_framing(&failed).is_some());
}

#[test]
fn a_confirmation_that_never_had_a_plan_is_still_left_alone() {
    // The distinction the whole `PlanSlot` exists for, asserted directly:
    // `Absent` and `Rebuilding` are both "no plan right now", and only one of
    // them may leave the confirmation offerable. #594 decided a preview
    // informs and never gates, so an ordinary dialog whose plan has not
    // arrived keeps its own verdict.
    let absent = verdict(&PlanSlot::Absent, &FeedLog::new());
    assert_eq!(absent, PlanVerdict::NoPlan);
    assert!(confirm_enabled(true, &absent));
    assert_eq!(verdict_headline(&absent), None, "and says nothing about it");
}

#[test]
fn a_rebuild_in_flight_outranks_the_plan_it_is_replacing() {
    // The force-with-lease case specifically. Its plan is carried on the
    // operation, so a rebuild that did not outrank it would read the plan it
    // is in the middle of replacing — and report it fresh or stale on the
    // strength of a generation the user has already asked to be rid of.
    let carried = PlanOnScreen {
        generation: "100".to_string(),
        expects: vec!["refs/heads/main".to_string()],
    };
    assert_eq!(
        plan_on_screen(&force_push_op(carried.clone()), PlanSlot::Rebuilding),
        PlanSlot::Rebuilding
    );
    assert_eq!(
        plan_on_screen(&force_push_op(carried.clone()), PlanSlot::RebuildFailed),
        PlanSlot::RebuildFailed
    );
    // And when it lands, the carried plan is the new one and is read again.
    assert_eq!(
        plan_on_screen(&force_push_op(carried.clone()), PlanSlot::Absent),
        PlanSlot::Ready(carried)
    );
}

#[test]
fn the_lease_rebuild_is_a_different_path_because_push_has_no_preview() {
    // Defect 2, pinned where a host test can see it. `preview_subject(Push)`
    // is `NotPreviewable`, so routing the force-with-lease rebuild through
    // `preview_action` alone resolves to `Clear` — the button was offered,
    // clicking it issued no request, and nothing changed.
    use crate::features::dialogs::core::preview_subject;
    use crate::features::preview::core::{preview_action, PreviewAction};

    let op = force_push_op(PlanOnScreen {
        generation: "100".to_string(),
        expects: Vec::new(),
    });
    assert_eq!(
        preview_action(Some(preview_subject(&op))),
        PreviewAction::Clear,
        "this is the routing that made the offered button inert; the dialog's \
         `Clear` arm is what has to do the work"
    );
    assert!(
        rebuild_is_offered(&verdict(
            &plan_on_screen(&op, PlanSlot::Absent),
            &FeedLog::new()
        )),
        "and a force-with-lease plan the feed cannot vouch for does offer \
         Rebuild — so the arm above is reachable, not theoretical"
    );
}
